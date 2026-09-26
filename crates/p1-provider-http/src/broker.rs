//! The transport broker's HTTP side: the native mirror of the frozen `http` and
//! `credential-control` WIT interfaces (`modules/wit/transport.wit`).
//!
//! A provider component lowers a request into a [`LoweredHttpRequest`] and never
//! sees a credential. The host binds each route to a [`RouteAuthority`] — the
//! route file's endpoint and credential source — which the component can neither
//! name nor replace: a lowered request has no field that selects an endpoint or a
//! credential source. [`RouteAuthority::validate`] refuses a request that could
//! leave the endpoint or smuggle a credential header before any credential is read
//! or any connection opened; [`broker_drive`] then hands the validated request to
//! the existing [`drive`] loop, which keeps retry, backoff, the one refresh after
//! 401/403 and the read bounds (freeze item 9). The credential is attached inside
//! the drive loop's request builder, immediately before each send.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, CancellationToken, ProviderError, ProviderErrorKind, ProviderStream,
};
use reqwest::Url;
use reqwest::header::{HeaderName, HeaderValue};

use crate::credential::{Credential, CredentialSource};
use crate::drive::{DriveRequest, drive};
use crate::http::{HttpRequest, RedactedUrl, Transport};
use crate::parser::ResponseParser;
use crate::retry::RetryPolicy;

/// Headers a component may never set: each carries a credential, and the broker
/// is the only party that attaches one.
const FORBIDDEN_HEADERS: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "api-key",
];

// The refusal messages are constants: a refused path or header may carry the very
// secret or prompt text the refusal exists to keep out of logs.
const PATH_REFUSED: &str =
    "the lowered request's path is not a path under the route's endpoint; nothing was sent";
const HEADERS_REFUSED: &str = "the lowered request's headers are invalid or name a credential \
     header the broker alone attaches; nothing was sent";
const ACCOUNT_ID_MISSING: &str = "the route's credential has no account id, which the lowered \
     request's credential use needs; nothing was sent";

/// `credential-control.credential-scheme`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialScheme {
    /// `Authorization: Bearer <token>`.
    Bearer,
}

/// `credential-control.credential-use`: how the broker attaches the route's
/// credential. It names a placement, never a credential or its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CredentialUse {
    pub scheme: CredentialScheme,
    /// The header that carries the credential's account id, when the account
    /// needs one (`chatgpt-account-id`).
    pub account_id_header: Option<String>,
}

/// `http.http-request`, as a provider component lowers it. The method is always
/// POST, the only method the frozen interface has.
#[derive(Clone, PartialEq, Eq)]
pub struct LoweredHttpRequest {
    /// Path and query relative to the route's endpoint, starting with `/`.
    pub path: String,
    /// Headers without any credential.
    pub headers: Vec<(String, String)>,
    pub credential: CredentialUse,
    pub body: Vec<u8>,
}

impl std::fmt::Debug for LoweredHttpRequest {
    /// Header values and the body never appear, and the query string is dropped:
    /// provider endpoints carry prompt material in it.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("LoweredHttpRequest")
            .field("path", &RedactedUrl(&self.path))
            .field("header_names", &header_names(&self.headers))
            .field("credential", &self.credential)
            .field("body_len", &self.body.len())
            .finish()
    }
}

/// A lowered request that passed [`RouteAuthority::validate`]: its URL is the
/// route's endpoint plus the component's path, and its headers carry no
/// credential. Only the broker can build one.
#[derive(Clone)]
pub struct ValidatedRequest {
    url: String,
    headers: Vec<(String, String)>,
    credential: CredentialUse,
    body: Vec<u8>,
}

impl std::fmt::Debug for ValidatedRequest {
    /// Same redaction as [`LoweredHttpRequest`]'s.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ValidatedRequest")
            .field("url", &RedactedUrl(&self.url))
            .field("header_names", &header_names(&self.headers))
            .field("credential", &self.credential)
            .field("body_len", &self.body.len())
            .finish()
    }
}

impl ValidatedRequest {
    /// The request as sent: the credential headers first, then the component's,
    /// then the body. `attach` is false on a proxy-injected route, whose
    /// placeholder credential must never be sent.
    fn to_http_request(&self, credential: &Credential, attach: bool) -> HttpRequest {
        let mut headers = Vec::with_capacity(self.headers.len() + 2);
        if attach {
            match self.credential.scheme {
                CredentialScheme::Bearer => headers.push((
                    "authorization".to_string(),
                    format!("Bearer {}", credential.bearer),
                )),
            }
            // A missing account id never gets here: `AccountIdRequired` fails the
            // attempt as `Authentication` before the builder runs.
            if let (Some(name), Some(account_id)) =
                (&self.credential.account_id_header, &credential.account_id)
            {
                headers.push((name.clone(), account_id.clone()));
            }
        }
        headers.extend(self.headers.iter().cloned());
        HttpRequest {
            url: self.url.clone(),
            headers,
            body: self.body.clone(),
        }
    }
}

/// The endpoint is not an absolute `http`/`https` URL with a host and without
/// userinfo, query or fragment.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error(
    "the route endpoint must be an absolute http or https URL with a host and without \
     userinfo, query or fragment"
)]
pub struct InvalidEndpoint;

/// The route binding the host builds from the route file: the endpoint every
/// request goes to and the credential source that authenticates it. Nothing a
/// component supplies can change either, and no accessor hands out the
/// credential source.
#[derive(Clone)]
pub struct RouteAuthority {
    endpoint: Url,
    /// The endpoint's path without a trailing `/`; every request path extends it.
    prefix: String,
    credentials: Arc<dyn CredentialSource>,
}

impl std::fmt::Debug for RouteAuthority {
    /// The credential source is never formatted; the endpoint has no query or
    /// userinfo by construction.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RouteAuthority")
            .field("endpoint", &RedactedUrl(self.endpoint.as_str()))
            .field("credentials", &"<redacted>")
            .field("proxy_injected", &self.credentials.proxy_injected())
            .finish()
    }
}

impl RouteAuthority {
    pub fn new(
        endpoint: &str,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, InvalidEndpoint> {
        let endpoint = Url::parse(endpoint).map_err(|_| InvalidEndpoint)?;
        let acceptable = matches!(endpoint.scheme(), "http" | "https")
            && endpoint.host_str().is_some()
            && endpoint.username().is_empty()
            && endpoint.password().is_none()
            && endpoint.query().is_none()
            && endpoint.fragment().is_none();
        if !acceptable {
            return Err(InvalidEndpoint);
        }
        let prefix = endpoint.path().trim_end_matches('/').to_string();
        Ok(Self {
            endpoint,
            prefix,
            credentials,
        })
    }

    /// Check a lowered request against the route before anything is read or sent.
    /// A refusal is `Protocol`: the component broke the contract, and the same
    /// request would fail again, so it is never retried.
    pub fn validate(
        &self,
        request: &LoweredHttpRequest,
    ) -> Result<ValidatedRequest, ProviderError> {
        let url = self
            .url_for(&request.path)
            .ok_or_else(|| ProviderError::new(ProviderErrorKind::Protocol, PATH_REFUSED))?;
        check_lowered_headers(&request.headers, &request.credential)?;
        Ok(ValidatedRequest {
            url,
            headers: request.headers.clone(),
            credential: request.credential.clone(),
            body: request.body.clone(),
        })
    }

    /// `endpoint + path` when the path stays under the endpoint. The textual
    /// checks refuse what a URL parser would silently resolve or reinterpret; the
    /// parsed comparison is the final word on where the request goes.
    fn url_for(&self, path: &str) -> Option<String> {
        let rest = path.strip_prefix('/')?;
        // `Url::parse` does not decode `%2f`/`%5c`, so an encoded separator stays
        // inside the endpoint's prefix here, but an origin server that decodes
        // before resolving dot segments would read `/..%2fx` as `/../x` and leave
        // the prefix. Refuse the encoded separators exactly like the literal `\`.
        let lowered = path.to_ascii_lowercase();
        if rest.starts_with('/')
            || lowered.contains("%2f")
            || lowered.contains("%5c")
            || path
                .chars()
                .any(|c| matches!(c, '\\' | '#' | '@') || c.is_whitespace() || c.is_control())
        {
            return None;
        }
        let path_part = path.split('?').next().unwrap_or_default();
        let dot_segment = path_part.split('/').any(|segment| {
            let decoded = segment.to_ascii_lowercase().replace("%2e", ".");
            decoded == "." || decoded == ".."
        });
        if dot_segment {
            return None;
        }
        let base = self.endpoint.as_str().trim_end_matches('/');
        let url = Url::parse(&format!("{base}{path}")).ok()?;
        let same_origin = url.scheme() == self.endpoint.scheme()
            && url.host_str() == self.endpoint.host_str()
            && url.port_or_known_default() == self.endpoint.port_or_known_default()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none();
        let under_prefix = url
            .path()
            .strip_prefix(self.prefix.as_str())
            .is_some_and(|tail| tail.starts_with('/'));
        (same_origin && under_prefix).then(|| url.to_string())
    }
}

/// The credential rule for a component's own headers, shared by the HTTP request
/// and the WebSocket handshake: every name is a valid header token and every value
/// a valid header value (no CR, LF or other control byte); no name is one of the
/// five credential headers, in any case; and none repeats the account-id header
/// the credential use names, which only the broker fills. The account-id header
/// name itself must be a valid, non-credential name.
pub fn check_lowered_headers(
    headers: &[(String, String)],
    credential: &CredentialUse,
) -> Result<(), ProviderError> {
    let refused = || ProviderError::new(ProviderErrorKind::Protocol, HEADERS_REFUSED);
    let account_id_header = credential.account_id_header.as_deref();
    if let Some(name) = account_id_header
        && !allowed_name(name)
    {
        return Err(refused());
    }
    for (name, value) in headers {
        let reserved = account_id_header.is_some_and(|account| name.eq_ignore_ascii_case(account));
        if reserved || !allowed_name(name) || HeaderValue::from_str(value).is_err() {
            return Err(refused());
        }
    }
    Ok(())
}

fn allowed_name(name: &str) -> bool {
    HeaderName::from_bytes(name.as_bytes()).is_ok()
        && !FORBIDDEN_HEADERS
            .iter()
            .any(|forbidden| name.eq_ignore_ascii_case(forbidden))
}

fn header_names(headers: &[(String, String)]) -> Vec<&str> {
    headers.iter().map(|(name, _)| name.as_str()).collect()
}

/// Send one lowered request on its route. An `Err` means the request was refused
/// by [`RouteAuthority::validate`]: no credential was read and nothing was sent.
/// Otherwise the stream is [`drive`]'s, with the route's credential source, a
/// fresh parser per attempt, `retry` and `cancel`.
pub fn broker_drive(
    authority: &RouteAuthority,
    transport: Arc<dyn Transport>,
    request: &LoweredHttpRequest,
    new_parser: Box<dyn Fn() -> Box<dyn ResponseParser> + Send + Sync>,
    retry: RetryPolicy,
    cancel: CancellationToken,
) -> Result<ProviderStream, ProviderError> {
    let validated = authority.validate(request)?;
    let attach = !authority.credentials.proxy_injected();
    let credentials: Arc<dyn CredentialSource> =
        if attach && validated.credential.account_id_header.is_some() {
            Arc::new(AccountIdRequired {
                inner: authority.credentials.clone(),
            })
        } else {
            authority.credentials.clone()
        };
    Ok(drive(DriveRequest {
        transport,
        credentials,
        build: Box::new(move |credential: &Credential| {
            validated.to_http_request(credential, attach)
        }),
        new_parser,
        retry,
        cancel,
    }))
}

/// Fails an attempt as `Authentication` when the credential it would send has no
/// account id although the request names an account-id header. It sits in front
/// of the route's source, so the drive loop ends the stream before building the
/// request; `DriveRequest::build` stays infallible for the native adapters. The
/// credential passes through untouched, so `refresh` still receives exactly the
/// credential the provider rejected.
struct AccountIdRequired {
    inner: Arc<dyn CredentialSource>,
}

fn require_account_id(credential: Credential) -> Result<Credential, ProviderError> {
    if credential.account_id.is_some() {
        Ok(credential)
    } else {
        Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            ACCOUNT_ID_MISSING,
        ))
    }
}

impl CredentialSource for AccountIdRequired {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.inner.access().await.and_then(require_account_id) })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.inner
                .refresh(rejected)
                .await
                .and_then(require_account_id)
        })
    }

    fn proxy_injected(&self) -> bool {
        self.inner.proxy_injected()
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures_util::StreamExt;
    use p1_contracts::{
        AssistantBlock, AssistantItem, CompletedResponse, Origin, Outcome, StopReason, StreamEvent,
    };

    use super::*;
    use crate::drive::proxy_refusal_message;
    use crate::sse::SseEvent;
    use crate::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

    const ENDPOINT: &str = "https://provider.test/backend-api/codex";
    const BEARER: &str = "BEARER-SENTINEL-1";
    const REFRESHED: &str = "BEARER-SENTINEL-2";
    const ACCOUNT: &str = "ACCOUNT-SENTINEL";

    /// Understands `done` only: the broker's policy is what is under test.
    struct TestParser;

    impl ResponseParser for TestParser {
        fn on_event(&mut self, event: SseEvent) -> Vec<StreamEvent> {
            if event.data.trim() != "done" {
                return Vec::new();
            }
            vec![StreamEvent::Finished(Outcome::Completed(
                CompletedResponse {
                    item: AssistantItem {
                        origin: Origin {
                            route: "test-route".to_string(),
                            model: "test-model".to_string(),
                        },
                        blocks: vec![AssistantBlock::Text {
                            text: "ok".to_string(),
                        }],
                    },
                    stop: StopReason::EndTurn,
                    usage: None,
                },
            ))]
        }

        fn on_end(&mut self) -> Outcome {
            Outcome::Failed(ProviderError::new(
                ProviderErrorKind::Transport,
                "stream ended without a terminal event",
            ))
        }

        fn on_http_error(
            &self,
            status: u16,
            _headers: &[(String, String)],
            _body: &[u8],
        ) -> ProviderError {
            let kind = match status {
                401 | 403 => ProviderErrorKind::Authentication,
                _ => ProviderErrorKind::InvalidRequest,
            };
            ProviderError::new(kind, format!("http status {status}"))
        }
    }

    struct Source {
        initial: Credential,
        refreshed: Credential,
        proxy: bool,
        access_calls: AtomicUsize,
        refresh_calls: Mutex<Vec<Credential>>,
    }

    impl Source {
        fn new(account_id: Option<&str>) -> Arc<Self> {
            let credential = |bearer: &str| Credential {
                bearer: bearer.to_string(),
                account_id: account_id.map(str::to_string),
            };
            Arc::new(Self {
                initial: credential(BEARER),
                refreshed: credential(REFRESHED),
                proxy: false,
                access_calls: AtomicUsize::new(0),
                refresh_calls: Mutex::new(Vec::new()),
            })
        }

        /// A `[credential] kind = "none"` route: an empty placeholder bearer.
        fn proxy() -> Arc<Self> {
            let placeholder = Credential {
                bearer: String::new(),
                account_id: None,
            };
            Arc::new(Self {
                initial: placeholder.clone(),
                refreshed: placeholder,
                proxy: true,
                access_calls: AtomicUsize::new(0),
                refresh_calls: Mutex::new(Vec::new()),
            })
        }

        fn refreshes(&self) -> Vec<Credential> {
            self.refresh_calls.lock().unwrap().clone()
        }
    }

    impl CredentialSource for Source {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                self.access_calls.fetch_add(1, Ordering::SeqCst);
                Ok(self.initial.clone())
            })
        }

        fn refresh<'a>(
            &'a self,
            rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async move {
                self.refresh_calls.lock().unwrap().push(rejected.clone());
                Ok(self.refreshed.clone())
            })
        }

        fn proxy_injected(&self) -> bool {
            self.proxy
        }
    }

    fn authority(source: &Arc<Source>) -> RouteAuthority {
        let credentials: Arc<dyn CredentialSource> = source.clone();
        RouteAuthority::new(ENDPOINT, credentials).unwrap()
    }

    fn lowered(path: &str) -> LoweredHttpRequest {
        LoweredHttpRequest {
            path: path.to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "text/event-stream".to_string()),
            ],
            credential: CredentialUse {
                scheme: CredentialScheme::Bearer,
                account_id_header: None,
            },
            body: br#"{"stream":true}"#.to_vec(),
        }
    }

    fn with_account_header(mut request: LoweredHttpRequest) -> LoweredHttpRequest {
        request.credential.account_id_header = Some("chatgpt-account-id".to_string());
        request
    }

    fn status(status: u16) -> ScriptedResponse {
        ScriptedResponse {
            status,
            headers: vec![(
                "location".to_string(),
                "https://elsewhere.test/".to_string(),
            )],
            chunks: Vec::new(),
            end: BodyEnd::Eof,
        }
    }

    fn done() -> ScriptedResponse {
        ScriptedResponse::ok_sse("data: done\n\n")
    }

    fn start(
        source: &Arc<Source>,
        transport: &ScriptedTransport,
        request: &LoweredHttpRequest,
    ) -> Result<ProviderStream, ProviderError> {
        broker_drive(
            &authority(source),
            Arc::new(transport.clone()),
            request,
            Box::new(|| Box::new(TestParser) as Box<dyn ResponseParser>),
            RetryPolicy::default(),
            CancellationToken::new(),
        )
    }

    async fn outcome(mut stream: ProviderStream) -> Outcome {
        let mut last = None;
        while let Some(event) = stream.next().await {
            last = Some(event);
        }
        match last {
            Some(StreamEvent::Finished(outcome)) => outcome,
            other => panic!("the stream did not end with Finished: {other:?}"),
        }
    }

    fn failure(outcome: Outcome) -> ProviderError {
        match outcome {
            Outcome::Failed(error) => error,
            other => panic!("expected a failure, got {other:?}"),
        }
    }

    /// Asserts a refusal: `Protocol`, a constant message, no credential read and
    /// no request sent.
    fn assert_refused(request: &LoweredHttpRequest, secret: &str) {
        let source = Source::new(Some(ACCOUNT));
        let transport = ScriptedTransport::new(Vec::new());
        let error = start(&source, &transport, request)
            .err()
            .unwrap_or_else(|| panic!("{request:?} must be refused"));
        assert_eq!(error.kind, ProviderErrorKind::Protocol, "{request:?}");
        assert!(
            error.message == PATH_REFUSED || error.message == HEADERS_REFUSED,
            "{error:?}"
        );
        assert!(
            secret.is_empty() || !error.message.contains(secret),
            "{error:?}"
        );
        assert_eq!(source.access_calls.load(Ordering::SeqCst), 0);
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn each_forbidden_header_in_any_case_is_refused_before_sending() {
        for name in FORBIDDEN_HEADERS {
            for spelled in [
                name.to_string(),
                name.to_ascii_uppercase(),
                name.split('-')
                    .map(|part| {
                        let mut chars = part.chars();
                        chars.next().map_or(String::new(), |first| {
                            first.to_ascii_uppercase().to_string() + chars.as_str()
                        })
                    })
                    .collect::<Vec<_>>()
                    .join("-"),
            ] {
                let mut request = lowered("/responses");
                request
                    .headers
                    .push((spelled.clone(), "SMUGGLED-SENTINEL".to_string()));
                assert_refused(&request, &spelled);
            }
        }
    }

    #[test]
    fn paths_that_could_leave_the_endpoint_are_refused_before_sending() {
        for path in [
            "https://evil.test/responses",
            "responses",
            "//evil.test/responses",
            "/responses@evil.test",
            "/@evil.test",
            "/\\evil.test",
            "/responses\\..\\x",
            "/../responses",
            "/responses/..",
            "/a/./b",
            "/%2e%2e/responses",
            "/%2E%2e/responses",
            "/.%2e/x",
            "/%2e/x",
            "/..%2fx",
            "/..%5cx",
            "/responses#fragment",
            "/responses\r\nx-injected: 1",
            "/responses\n",
            "/responses with space",
            "/responses\t",
            "/responses\u{0}",
            "",
        ] {
            assert_refused(&lowered(path), path);
        }
    }

    #[test]
    fn a_cr_or_lf_in_a_header_is_refused_before_sending() {
        for (name, value) in [
            ("x-trace", "VALUE-SENTINEL\r\nauthorization: Bearer x"),
            ("x-trace", "VALUE-SENTINEL\nb"),
            ("x-trace", "VALUE-SENTINEL\u{7f}"),
            ("x-tr\r\nace", "VALUE-SENTINEL"),
            ("x trace", "VALUE-SENTINEL"),
            ("", "VALUE-SENTINEL"),
        ] {
            let mut request = lowered("/responses");
            request.headers.push((name.to_string(), value.to_string()));
            assert_refused(&request, "VALUE-SENTINEL");
            if !name.is_empty() {
                assert_refused(&request, name);
            }
        }
    }

    #[test]
    fn the_account_id_header_is_the_brokers_alone() {
        let mut spoofed = with_account_header(lowered("/responses"));
        spoofed
            .headers
            .push(("ChatGPT-Account-Id".to_string(), "someone-else".to_string()));
        assert_refused(&spoofed, "someone-else");

        let mut named_authorization = lowered("/responses");
        named_authorization.credential.account_id_header = Some("Authorization".to_string());
        assert_refused(&named_authorization, "Authorization");
    }

    #[tokio::test]
    async fn a_valid_request_carries_the_credential_first_once_then_the_component_headers() {
        let source = Source::new(Some(ACCOUNT));
        let transport = ScriptedTransport::new(vec![done()]);
        let request = with_account_header(lowered("/responses?stream=1"));
        let stream = start(&source, &transport, &request).unwrap();
        assert!(matches!(outcome(stream).await, Outcome::Completed(_)));

        let sent = transport.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, format!("{ENDPOINT}/responses?stream=1"));
        assert_eq!(
            sent[0].headers,
            vec![
                ("authorization".to_string(), format!("Bearer {BEARER}")),
                ("chatgpt-account-id".to_string(), ACCOUNT.to_string()),
                ("content-type".to_string(), "application/json".to_string()),
                ("accept".to_string(), "text/event-stream".to_string()),
            ]
        );
        assert_eq!(sent[0].body, request.body);
    }

    #[tokio::test]
    async fn without_an_account_id_header_only_the_bearer_is_attached() {
        let source = Source::new(Some(ACCOUNT));
        let transport = ScriptedTransport::new(vec![done()]);
        let stream = start(&source, &transport, &lowered("/responses")).unwrap();
        assert!(matches!(outcome(stream).await, Outcome::Completed(_)));
        let headers = &transport.requests()[0].headers;
        assert_eq!(headers[0].0, "authorization");
        assert_eq!(headers.len(), 3, "{:?}", transport.requests()[0]);
        assert!(!headers.iter().any(|(_, value)| value == ACCOUNT));
    }

    #[tokio::test]
    async fn a_missing_account_id_the_request_names_fails_as_authentication_unsent() {
        let source = Source::new(None);
        let transport = ScriptedTransport::new(Vec::new());
        let request = with_account_header(lowered("/responses"));
        let error = failure(outcome(start(&source, &transport, &request).unwrap()).await);
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(error.message, ACCOUNT_ID_MISSING);
        assert!(transport.requests().is_empty());
    }

    #[tokio::test]
    async fn a_proxy_injected_route_attaches_nothing_and_never_refreshes() {
        let source = Source::proxy();
        let transport = ScriptedTransport::new(vec![status(401)]);
        // The proxy route has no account id; naming a header must not fail it,
        // because nothing is attached.
        let request = with_account_header(lowered("/responses"));
        let error = failure(outcome(start(&source, &transport, &request).unwrap()).await);
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(error.message, proxy_refusal_message(401));
        assert!(source.refreshes().is_empty());
        let sent = transport.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].headers, request.headers);
    }

    #[tokio::test]
    async fn a_401_refreshes_once_with_the_rejected_credential_and_retries_with_the_new_one() {
        let source = Source::new(Some(ACCOUNT));
        let transport = ScriptedTransport::new(vec![status(401), done()]);
        let request = with_account_header(lowered("/responses"));
        let stream = start(&source, &transport, &request).unwrap();
        assert!(matches!(outcome(stream).await, Outcome::Completed(_)));

        assert_eq!(source.refreshes(), vec![source.initial.clone()]);
        let sent = transport.requests();
        assert_eq!(sent.len(), 2);
        assert_eq!(sent[0].headers[0].1, format!("Bearer {BEARER}"));
        assert_eq!(sent[1].headers[0].1, format!("Bearer {REFRESHED}"));
        assert_eq!(
            sent[1]
                .headers
                .iter()
                .filter(|(name, _)| name == "authorization")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn a_second_401_after_the_refresh_is_an_authentication_failure() {
        let source = Source::new(Some(ACCOUNT));
        let transport = ScriptedTransport::new(vec![status(401), status(401)]);
        let error = failure(outcome(start(&source, &transport, &lowered("/r")).unwrap()).await);
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(source.refreshes().len(), 1);
        assert_eq!(transport.requests().len(), 2);
    }

    #[tokio::test]
    async fn a_redirect_is_classified_not_followed() {
        for code in [301, 302, 303, 307, 308] {
            let source = Source::new(Some(ACCOUNT));
            let transport = ScriptedTransport::new(vec![status(code)]);
            let error = failure(outcome(start(&source, &transport, &lowered("/r")).unwrap()).await);
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
            assert_eq!(
                transport.requests().len(),
                1,
                "HTTP {code} must not be followed"
            );
        }
    }

    /// A lowered request names a path, headers, a credential PLACEMENT and a body —
    /// nothing that selects an endpoint or a credential source. The exhaustive
    /// destructuring stops compiling if a field is ever added, so "a component
    /// chooses another credential source" stays impossible by construction.
    #[tokio::test]
    async fn a_component_cannot_choose_another_endpoint_or_credential_source() {
        let LoweredHttpRequest {
            path: _,
            headers: _,
            credential,
            body: _,
        } = lowered("/r");
        let CredentialUse {
            scheme: CredentialScheme::Bearer,
            account_id_header: _,
        } = credential;

        let source = Source::new(None);
        let transport = ScriptedTransport::new(vec![done()]);
        let stream = start(&source, &transport, &lowered("/r")).unwrap();
        assert!(matches!(outcome(stream).await, Outcome::Completed(_)));
        let sent = &transport.requests()[0];
        assert!(sent.url.starts_with(ENDPOINT));
        assert_eq!(sent.headers[0].1, format!("Bearer {BEARER}"));
        assert_eq!(source.access_calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn debug_never_prints_a_credential_a_header_value_the_body_or_a_query() {
        let source = Source::new(Some(ACCOUNT));
        let authority = authority(&source);
        let mut request = with_account_header(lowered("/responses?key=QUERY-SENTINEL"));
        request
            .headers
            .push(("x-trace".to_string(), "HEADER-SENTINEL".to_string()));
        request.body = b"BODY-SENTINEL".to_vec();
        let validated = authority.validate(&request).unwrap();
        for debug in [
            format!("{authority:?}"),
            format!("{request:?}"),
            format!("{validated:?}"),
        ] {
            for sentinel in [
                BEARER,
                ACCOUNT,
                "QUERY-SENTINEL",
                "HEADER-SENTINEL",
                "BODY-SENTINEL",
            ] {
                assert!(!debug.contains(sentinel), "{debug}");
            }
        }
        assert!(format!("{validated:?}").contains("/backend-api/codex/responses"));
    }

    #[test]
    fn endpoints_and_the_prefix_the_path_extends() {
        let credentials: Arc<dyn CredentialSource> = Source::new(None);
        for bad in [
            "provider.test/v1",
            "ftp://provider.test/v1",
            "https://user:pass@provider.test/v1",
            "https://provider.test/v1?key=x",
            "https://provider.test/v1#x",
        ] {
            assert_eq!(
                RouteAuthority::new(bad, credentials.clone()).err(),
                Some(InvalidEndpoint),
                "{bad}"
            );
        }
        let root = RouteAuthority::new("http://127.0.0.1:8080/", credentials.clone()).unwrap();
        assert_eq!(
            root.validate(&lowered("/v1/chat")).unwrap().url,
            "http://127.0.0.1:8080/v1/chat"
        );
        let trailing = RouteAuthority::new("https://provider.test/v1/", credentials).unwrap();
        assert_eq!(
            trailing.validate(&lowered("/chat")).unwrap().url,
            "https://provider.test/v1/chat"
        );
    }
}
