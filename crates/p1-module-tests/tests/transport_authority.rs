//! Transport authority (S4.5, issue #263): the route's endpoint and the route's
//! credential are the only authority a provider component's lowered request is sent
//! under, and no credential value crosses the boundary in either direction.
//!
//! The suite is written against the frozen `http` / `credential-control` /
//! `websocket` WIT text (`modules/wit/`, read-only here) and against S4.2's broker
//! (`p1_provider_http::broker`). A component lowers a request into a
//! [`LoweredHttpRequest`] and never sees a credential; the host binds the route to a
//! [`RouteAuthority`], which refuses anything that could leave its endpoint or
//! smuggle a credential header BEFORE any credential is read or any connection
//! opened, and which attaches the credential itself, at most once.
//!
//! The credential-shaped strings below are fabricated sentinels, so a leak has
//! something to be caught by; no real value is ever in this file.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::StreamExt;
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, CompletedResponse, Origin,
    Outcome, ProviderError, ProviderErrorKind, ProviderStream, StopReason, StreamEvent,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{
    ByteStream, Credential, CredentialScheme, CredentialSource, CredentialUse, HttpRequest,
    InvalidEndpoint, LoweredHttpRequest, ReqwestTransport, ResponseParser, RetryPolicy,
    RouteAuthority, SseEvent, Transport, broker_drive, check_lowered_headers,
};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

/// The route endpoint every case points at; the path is the component's.
const ENDPOINT: &str = "https://provider.test/backend-api/codex";

/// Fabricated sentinels: an access value, a refresh value, an account id and a
/// credential store path. None of them may reach a `Debug`, a message or a notice.
const BEARER: &str = "SENTINEL-ACCESS-VALUE";
const REFRESHED: &str = "SENTINEL-REFRESH-VALUE";
const ACCOUNT: &str = "SENTINEL-ACCOUNT-ID";
const CREDENTIAL_PATH: &str = "SENTINEL-CREDENTIAL-PATH/store.json";

/// The credential header names the frozen boundary reserves for the broker
/// (`modules/wit/transport.wit`, `http-request.headers`).
const CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "cookie",
    "x-api-key",
    "api-key",
];

/// A bound on the one real loopback exchange, so an unreachable local socket fails
/// the case instead of hanging it. It asserts nothing about a duration.
const LOOPBACK_GUARD: Duration = Duration::from_secs(10);

/// The route's credential source: scripted values, the store path a real source
/// holds, and counters, so a case can prove that a refusal read nothing and that a
/// refresh happened exactly once with exactly the rejected credential.
struct Source {
    initial: Credential,
    refreshed: Credential,
    /// Where a real source's store lives. The boundary must never print it, so the
    /// no-leak case watches for this string too.
    path: String,
    proxy_injected: bool,
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
            path: CREDENTIAL_PATH.to_string(),
            proxy_injected: false,
            access_calls: AtomicUsize::new(0),
            refresh_calls: Mutex::new(Vec::new()),
        })
    }

    /// A `[credential] kind = "none"` route: an empty placeholder that the broker
    /// never sends and the proxy injects after the request leaves the process.
    fn proxy() -> Arc<Self> {
        let placeholder = Credential {
            bearer: String::new(),
            account_id: None,
        };
        Arc::new(Self {
            initial: placeholder.clone(),
            refreshed: placeholder,
            path: CREDENTIAL_PATH.to_string(),
            proxy_injected: true,
            access_calls: AtomicUsize::new(0),
            refresh_calls: Mutex::new(Vec::new()),
        })
    }

    fn accesses(&self) -> usize {
        self.access_calls.load(Ordering::SeqCst)
    }

    /// The store path this source holds; a real one has one too, and the boundary
    /// must never print it, so the no-leak case watches it like a value.
    fn credential_path(&self) -> &str {
        &self.path
    }

    /// The credentials `refresh` was called with, in order.
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
        self.proxy_injected
    }
}

/// The component-side classifier: the broker never decides what a non-2xx status
/// means, the component's parser does. The messages are markers, so a case can prove
/// that the parser's classification — not a broker guess — reached the outcome.
struct ClassifyingParser;

impl ResponseParser for ClassifyingParser {
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
        body: &[u8],
    ) -> ProviderError {
        // Body markers stand in for the routes' own error payloads (ADR-0046/0062).
        match body {
            b"no-balance" => ProviderError::new(
                ProviderErrorKind::InsufficientBalance,
                "the account has no balance",
            ),
            b"not-entitled" => ProviderError::new(
                ProviderErrorKind::NotEntitled,
                "the plan does not allow this model",
            ),
            b"usage-limit" => ProviderError::new(
                ProviderErrorKind::UsageLimitExhausted,
                "the usage allowance is used up",
            ),
            _ => match status {
                301..=308 => ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("redirect {status} not followed"),
                ),
                401 | 403 => ProviderError::new(
                    ProviderErrorKind::Authentication,
                    format!("http status {status}"),
                ),
                _ => ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("http status {status}"),
                ),
            },
        }
    }
}

/// The host's route binding: the endpoint and the credential source the route file
/// named. Nothing a component sends can change either.
fn authority(endpoint: &str, source: &Arc<Source>) -> RouteAuthority {
    let credentials: Arc<dyn CredentialSource> = source.clone();
    RouteAuthority::new(endpoint, credentials).expect("a valid route endpoint")
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

fn start(
    route: &RouteAuthority,
    transport: &ScriptedTransport,
    request: &LoweredHttpRequest,
) -> Result<ProviderStream, ProviderError> {
    start_with_retry(route, transport, request, RetryPolicy::default())
}

fn start_with_retry(
    route: &RouteAuthority,
    transport: &ScriptedTransport,
    request: &LoweredHttpRequest,
    retry: RetryPolicy,
) -> Result<ProviderStream, ProviderError> {
    broker_drive(
        route,
        Arc::new(transport.clone()),
        request,
        Box::new(|| Box::new(ClassifyingParser) as Box<dyn ResponseParser>),
        retry,
        CancellationToken::new(),
    )
}

/// The retry policy of the notice case: one retry with no wait, so the notice text
/// is inspected without a real backoff.
fn no_delay_policy() -> RetryPolicy {
    RetryPolicy {
        max_retries: 1,
        base: Duration::ZERO,
        cap: Duration::ZERO,
        jitter: Duration::ZERO,
    }
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

async fn events(mut stream: ProviderStream) -> Vec<StreamEvent> {
    let mut collected = Vec::new();
    while let Some(event) = stream.next().await {
        collected.push(event);
    }
    collected
}

fn failure(outcome: Outcome) -> ProviderError {
    match outcome {
        Outcome::Failed(error) => error,
        other => panic!("expected a failure, got {other:?}"),
    }
}

fn status_response(status: u16) -> ScriptedResponse {
    // A 3xx carries a `Location`, so a transport that followed one would have a
    // different host to visit and the assertion on the sent URL would catch it.
    let headers = if (300..=399).contains(&status) {
        vec![(
            "location".to_string(),
            "https://elsewhere.test/".to_string(),
        )]
    } else {
        Vec::new()
    };
    ScriptedResponse {
        status,
        headers,
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }
}

fn status_with_body(status: u16, body: &str) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: vec![body.as_bytes().to_vec()],
        end: BodyEnd::Eof,
    }
}

fn done() -> ScriptedResponse {
    ScriptedResponse::ok_sse("data: done\n\n")
}

fn title_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            chars.next().map_or(String::new(), |first| {
                first.to_ascii_uppercase().to_string() + chars.as_str()
            })
        })
        .collect::<Vec<_>>()
        .join("-")
}

// ------------------------------------------------------------------ origin

/// A path that is absolute, scheme-relative, carries userinfo, a backslash, a `..`
/// or `%2e%2e`, a fragment or a CR/LF cannot be a path under the route endpoint. It
/// is refused as a protocol error BEFORE the route's credential is read and before
/// the transport sees a request.
#[tokio::test]
async fn a_path_that_could_leave_the_endpoint_is_refused_before_any_credential_is_read() {
    for path in [
        "https://evil.example/responses",
        "//evil.example/responses",
        "/responses/@evil.example",
        "/@evil.example",
        "/responses\\..\\responses",
        "/../responses",
        "/responses/..",
        "/a/./b",
        "/%2e%2e/responses",
        "/%2E%2e/responses",
        "/responses#fragment",
        "/responses\r\nx-injected: 1",
        "/responses\n",
        "/responses with space",
        "responses",
        "",
    ] {
        let source = Source::new(Some(ACCOUNT));
        let route = authority(ENDPOINT, &source);
        let transport = ScriptedTransport::new(Vec::new());
        let request = lowered(path);

        let refused = route
            .validate(&request)
            .err()
            .unwrap_or_else(|| panic!("{path:?} must be refused"));
        assert_eq!(refused.kind, ProviderErrorKind::Protocol, "{path:?}");

        let refused = start(&route, &transport, &request)
            .err()
            .unwrap_or_else(|| panic!("{path:?} must be refused"));
        assert_eq!(refused.kind, ProviderErrorKind::Protocol, "{path:?}");
        assert_eq!(
            source.accesses(),
            0,
            "{path:?}: a refused request reads no credential"
        );
        assert!(
            transport.requests().is_empty(),
            "{path:?}: a refused request is never sent"
        );
        // The refusal text is a constant: it never echoes what the component sent.
        assert!(
            !refused.message.contains("evil.example"),
            "{refused:?} must not echo the path"
        );
    }
}

/// A valid path lands on exactly `endpoint + path`, with no trailing-slash doubling
/// and with the body the component lowered kept byte for byte.
#[tokio::test]
async fn a_valid_path_lands_on_exactly_the_endpoint_plus_the_path() {
    for (endpoint, path, url) in [
        (
            ENDPOINT,
            "/responses?stream=1",
            "https://provider.test/backend-api/codex/responses?stream=1",
        ),
        (
            "https://provider.test/v1/",
            "/chat",
            "https://provider.test/v1/chat",
        ),
        (
            "http://127.0.0.1:8080/",
            "/v1/chat",
            "http://127.0.0.1:8080/v1/chat",
        ),
    ] {
        let source = Source::new(None);
        let route = authority(endpoint, &source);
        let transport = ScriptedTransport::new(vec![done()]);
        let request = lowered(path);
        assert!(route.validate(&request).is_ok(), "{path}");
        let stream = start(&route, &transport, &request).expect("a valid request is sent");
        assert!(matches!(outcome(stream).await, Outcome::Completed(_)));

        let sent = transport.requests();
        assert_eq!(sent.len(), 1);
        assert_eq!(sent[0].url, url, "{endpoint} + {path}");
        assert_eq!(
            sent[0].body, request.body,
            "{path}: the body travels unchanged"
        );
        assert_eq!(source.accesses(), 1, "{path}");
    }
}

/// An endpoint that is not an absolute `http`/`https` URL with a host, or that
/// carries userinfo, a query or a fragment, is not a route.
#[test]
fn an_endpoint_that_could_carry_a_credential_or_a_query_is_not_a_route() {
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
}

// ---------------------------------------------------------------- redirect

/// A 301/302/307/308 is classified by the parser and never followed: the transport
/// sees exactly one request, and it is the route's own URL, not the `Location`.
#[tokio::test]
async fn a_scripted_redirect_is_classified_by_the_parser_and_never_followed() {
    for status in [301_u16, 302, 307, 308] {
        let source = Source::new(Some(ACCOUNT));
        let route = authority(ENDPOINT, &source);
        let transport = ScriptedTransport::new(vec![status_response(status)]);
        let stream = start(&route, &transport, &lowered("/responses")).expect("a valid request");
        let error = failure(outcome(stream).await);

        assert_eq!(
            error.kind,
            ProviderErrorKind::InvalidRequest,
            "HTTP {status}"
        );
        assert_eq!(error.message, format!("redirect {status} not followed"));
        let sent = transport.requests();
        assert_eq!(sent.len(), 1, "HTTP {status} must not be followed");
        assert_eq!(
            sent[0].url,
            format!("{ENDPOINT}/responses"),
            "HTTP {status}: the Location is never visited"
        );
        assert!(source.refreshes().is_empty(), "HTTP {status}");
        assert_eq!(source.accesses(), 1, "HTTP {status}");
    }
}

/// The production transport really is built with redirects off. The endpoint and the
/// redirect target are two listeners inside THIS test process (no external network).
/// The target answers a 200 of its own and counts what it accepted, so a followed
/// redirect would be visible twice over: a different status and body, and a
/// connection on the target.
#[tokio::test]
async fn the_production_transport_is_built_with_redirects_off() {
    let target = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the redirect target");
    let target_addr = target.local_addr().expect("the target's address");
    let followed = Arc::new(AtomicUsize::new(0));
    let target_server = tokio::spawn({
        let followed = Arc::clone(&followed);
        async move { answer_200(target, followed).await }
    });

    let endpoint = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind the endpoint");
    let endpoint_addr = endpoint.local_addr().expect("the endpoint's address");
    let location = format!("http://{target_addr}/followed");
    let endpoint_server = tokio::spawn(async move { answer_302(endpoint, location).await });

    let transport = ReqwestTransport::new();
    let post = transport.post(HttpRequest {
        url: format!("http://{endpoint_addr}/start"),
        headers: Vec::new(),
        body: Vec::new(),
    });
    let response = tokio::time::timeout(LOOPBACK_GUARD, post)
        .await
        .expect("the loopback POST finished")
        .expect("the endpoint answered");
    endpoint_server.await.expect("the endpoint's task");

    assert_eq!(
        response.status, 302,
        "the redirect is handed back, not followed"
    );
    assert_eq!(
        collect_body(response.body).await,
        b"AT-THE-ENDPOINT",
        "the answer is the endpoint's, not the redirect target's"
    );
    assert_eq!(
        followed.load(Ordering::SeqCst),
        0,
        "the redirect target must see no connection"
    );
    target_server.abort();
}

/// Answer one request with a 302 pointing at `location`, then close. It reads the
/// request head first, so the peer sees a complete request before the answer.
async fn answer_302(listener: TcpListener, location: String) {
    let (mut socket, _) = listener.accept().await.expect("the transport connects");
    let mut head = Vec::new();
    let mut buffer = [0_u8; 1024];
    while !head.windows(4).any(|window| window == b"\r\n\r\n") {
        let read = socket.read(&mut buffer).await.expect("the request head");
        if read == 0 {
            break;
        }
        head.extend_from_slice(&buffer[..read]);
    }
    let body = "AT-THE-ENDPOINT";
    let answer = format!(
        "HTTP/1.1 302 Found\r\nLocation: {location}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(answer.as_bytes()).await.expect("the 302");
    socket.shutdown().await.expect("close the connection");
}

/// Answer every connection with a 200 of its own, counting what it accepted.
async fn answer_200(listener: TcpListener, accepted: Arc<AtomicUsize>) {
    while let Ok((mut socket, _)) = listener.accept().await {
        accepted.fetch_add(1, Ordering::SeqCst);
        let body = "AT-THE-TARGET";
        let answer = format!(
            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        let _ = socket.write_all(answer.as_bytes()).await;
    }
}

async fn collect_body(mut body: ByteStream) -> Vec<u8> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        bytes.extend_from_slice(&chunk.expect("no body error"));
    }
    bytes
}

// ------------------------------------------------------------------ header

/// Every credential header name, in any letter case, is refused before a credential
/// is read and before anything is sent.
#[tokio::test]
async fn each_credential_header_in_any_letter_case_is_refused_before_sending() {
    for name in CREDENTIAL_HEADERS {
        for spelled in [
            name.to_string(),
            name.to_ascii_uppercase(),
            title_case(name),
        ] {
            let mut request = lowered("/responses");
            request
                .headers
                .push((spelled.clone(), "SENTINEL-SMUGGLED-VALUE".to_string()));

            // The header rule alone, without a route.
            let refused = check_lowered_headers(&request.headers, &request.credential)
                .expect_err("a credential header is never the component's");
            assert_eq!(refused.kind, ProviderErrorKind::Protocol, "{spelled}");

            let source = Source::new(Some(ACCOUNT));
            let route = authority(ENDPOINT, &source);
            let transport = ScriptedTransport::new(Vec::new());
            let refused = start(&route, &transport, &request)
                .err()
                .unwrap_or_else(|| panic!("{spelled} must be refused"));
            assert_eq!(refused.kind, ProviderErrorKind::Protocol, "{spelled}");
            assert!(
                !refused.message.contains(&spelled),
                "{refused:?} must not echo the header name"
            );
            assert_eq!(source.accesses(), 0, "{spelled}: no credential is read");
            assert!(
                transport.requests().is_empty(),
                "{spelled}: nothing is sent"
            );
        }
    }
}

/// A header name or value that is not a valid HTTP token/value — CR, LF or a control
/// byte in either — is refused before sending.
#[tokio::test]
async fn a_header_name_or_value_with_cr_or_lf_is_refused_before_sending() {
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
        let source = Source::new(Some(ACCOUNT));
        let route = authority(ENDPOINT, &source);
        let transport = ScriptedTransport::new(Vec::new());
        let refused = start(&route, &transport, &request)
            .err()
            .unwrap_or_else(|| panic!("{name:?}: {value:?} must be refused"));
        assert_eq!(refused.kind, ProviderErrorKind::Protocol, "{name:?}");
        assert!(!refused.message.contains("VALUE-SENTINEL"), "{refused:?}");
        assert_eq!(source.accesses(), 0);
        assert!(transport.requests().is_empty());
    }
}

/// The account-id header a credential use names belongs to the broker, exactly like
/// a credential header: a component may neither send one itself nor name a
/// credential header as the account-id header.
#[tokio::test]
async fn the_account_id_header_is_the_brokers_alone() {
    let source = Source::new(Some(ACCOUNT));
    let route = authority(ENDPOINT, &source);

    let mut spoofed = with_account_header(lowered("/responses"));
    spoofed
        .headers
        .push(("ChatGPT-Account-Id".to_string(), "someone-else".to_string()));
    let transport = ScriptedTransport::new(Vec::new());
    let refused = start(&route, &transport, &spoofed)
        .err()
        .expect("a component may not send the account-id header itself");
    assert_eq!(refused.kind, ProviderErrorKind::Protocol);
    assert!(!refused.message.contains("someone-else"), "{refused:?}");
    assert_eq!(source.accesses(), 0);
    assert!(transport.requests().is_empty());

    let mut named_credential = lowered("/responses");
    named_credential.credential.account_id_header = Some("Authorization".to_string());
    assert_eq!(
        check_lowered_headers(&named_credential.headers, &named_credential.credential)
            .expect_err("the credential use may not name a credential header")
            .kind,
        ProviderErrorKind::Protocol
    );
    let refused = start(&route, &transport, &named_credential)
        .err()
        .expect("a credential header is never the account-id header");
    assert_eq!(refused.kind, ProviderErrorKind::Protocol);
    assert_eq!(source.accesses(), 0);
}

/// On a valid request the broker attaches the credential header exactly once, first,
/// and the component's own headers follow unchanged.
#[tokio::test]
async fn the_credential_header_is_first_exactly_once_and_component_headers_follow_unchanged() {
    let source = Source::new(Some(ACCOUNT));
    let route = authority(ENDPOINT, &source);
    let transport = ScriptedTransport::new(vec![done()]);
    let mut request = with_account_header(lowered("/responses"));
    request
        .headers
        .push(("x-trace".to_string(), "trace-value".to_string()));
    let stream = start(&route, &transport, &request).expect("a valid request is sent");
    assert!(matches!(outcome(stream).await, Outcome::Completed(_)));

    let sent = transport.requests();
    assert_eq!(sent.len(), 1);
    assert_eq!(
        sent[0].headers,
        vec![
            ("authorization".to_string(), format!("Bearer {BEARER}")),
            ("chatgpt-account-id".to_string(), ACCOUNT.to_string()),
            ("content-type".to_string(), "application/json".to_string()),
            ("accept".to_string(), "text/event-stream".to_string()),
            ("x-trace".to_string(), "trace-value".to_string()),
        ]
    );
    assert_eq!(
        sent[0]
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .count(),
        1
    );
    assert_eq!(source.accesses(), 1);

    // Without a named account-id header only the bearer is attached, and the
    // component's headers are still exactly what it lowered.
    let plain = Source::new(Some(ACCOUNT));
    let plain_route = authority(ENDPOINT, &plain);
    let plain_transport = ScriptedTransport::new(vec![done()]);
    let plain_request = lowered("/responses");
    let stream = start(&plain_route, &plain_transport, &plain_request).expect("a valid request");
    assert!(matches!(outcome(stream).await, Outcome::Completed(_)));
    let sent = plain_transport.requests();
    assert_eq!(sent[0].headers[0].0, "authorization");
    assert_eq!(sent[0].headers.len(), plain_request.headers.len() + 1);
    assert!(
        !sent[0].headers.iter().any(|(_, value)| value == ACCOUNT),
        "an account id the request did not name is never attached"
    );
}

// ----------------------------------------------------------------- refresh

/// A 401 or a 403 refreshes exactly once, with exactly the credential the provider
/// rejected, and the retry carries the refreshed one — still exactly once, still
/// first.
#[tokio::test]
async fn a_401_or_403_refreshes_once_with_the_rejected_credential() {
    for status in [401_u16, 403] {
        let source = Source::new(Some(ACCOUNT));
        let route = authority(ENDPOINT, &source);
        let transport = ScriptedTransport::new(vec![status_response(status), done()]);
        let request = with_account_header(lowered("/responses"));
        let stream = start(&route, &transport, &request).expect("a valid request");
        assert!(
            matches!(outcome(stream).await, Outcome::Completed(_)),
            "HTTP {status}: the refreshed credential is retried once"
        );

        assert_eq!(
            source.refreshes(),
            vec![source.initial.clone()],
            "HTTP {status}: exactly one refresh, with the rejected credential"
        );
        assert_eq!(source.accesses(), 1, "HTTP {status}: the retry reuses it");
        let sent = transport.requests();
        assert_eq!(sent.len(), 2, "HTTP {status}");
        assert_eq!(
            sent[0].headers[0],
            ("authorization".to_string(), format!("Bearer {BEARER}"))
        );
        assert_eq!(
            sent[1].headers[0],
            ("authorization".to_string(), format!("Bearer {REFRESHED}")),
            "HTTP {status}: the retry carries the refreshed credential"
        );
        assert_eq!(
            sent[1]
                .headers
                .iter()
                .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
                .count(),
            1
        );
    }
}

/// A second rejection after the one refresh is an authentication failure, whatever
/// statuses the two requests answered.
#[tokio::test]
async fn a_second_rejection_after_the_one_refresh_is_an_authentication_failure() {
    let source = Source::new(Some(ACCOUNT));
    let route = authority(ENDPOINT, &source);
    let transport = ScriptedTransport::new(vec![status_response(401), status_response(403)]);
    let stream = start(&route, &transport, &lowered("/responses")).expect("a valid request");
    let error = failure(outcome(stream).await);

    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(source.refreshes().len(), 1, "the one refresh is spent");
    assert_eq!(
        transport.requests().len(),
        2,
        "the request is not sent a third time"
    );
}

/// An exhausted account, a plan that does not allow the model and a used-up usage
/// allowance are diagnoses about the account, not a rejected credential: they are
/// never refreshed and never retried (ADR-0046 / ADR-0062).
#[tokio::test]
async fn account_diagnoses_are_never_refreshed() {
    for (status, body, kind) in [
        (
            401_u16,
            "no-balance",
            ProviderErrorKind::InsufficientBalance,
        ),
        (403, "not-entitled", ProviderErrorKind::NotEntitled),
        (429, "usage-limit", ProviderErrorKind::UsageLimitExhausted),
    ] {
        let source = Source::new(Some(ACCOUNT));
        let route = authority(ENDPOINT, &source);
        let transport = ScriptedTransport::new(vec![status_with_body(status, body)]);
        let stream = start(&route, &transport, &lowered("/responses")).expect("a valid request");
        let error = failure(outcome(stream).await);

        assert_eq!(error.kind, kind, "HTTP {status} {body}");
        assert!(
            source.refreshes().is_empty(),
            "HTTP {status} {body}: a fresh credential cannot help"
        );
        assert_eq!(
            transport.requests().len(),
            1,
            "HTTP {status} {body}: the answer is terminal"
        );
    }
}

/// A `kind = "none"` route attaches nothing at all — not even the empty placeholder
/// — and never refreshes: the refusal is the egress proxy's (ADR-0070, issue #134).
#[tokio::test]
async fn a_proxy_injected_route_attaches_nothing_and_never_refreshes() {
    let source = Source::proxy();
    let route = authority(ENDPOINT, &source);
    let transport = ScriptedTransport::new(vec![status_response(401)]);
    // The placeholder has no account id; naming the header must not fail a request
    // that attaches nothing at all.
    let request = with_account_header(lowered("/responses"));
    let stream = start(&route, &transport, &request).expect("a valid request");
    let error = failure(outcome(stream).await);

    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(error.message, p1_provider_http::proxy_refusal_message(401));
    assert!(source.refreshes().is_empty(), "there is nothing to refresh");
    assert_eq!(source.accesses(), 1);
    let sent = transport.requests();
    assert_eq!(sent.len(), 1, "a refusal that needs the proxy is terminal");
    assert_eq!(
        sent[0].headers, request.headers,
        "the component's headers go out alone"
    );
}

// --------------------------------------------------------- opaque authority

/// The frozen boundary text, read here and never written.
const TRANSPORT_WIT: &str = include_str!("../../../modules/wit/transport.wit");
const WORLDS_WIT: &str = include_str!("../../../modules/wit/worlds.wit");

/// The declaration text of the first block opened by `header` (`"interface http {"`),
/// with comment lines dropped so an explanatory comment is never read as a
/// declaration. Braces are matched, so nested blocks come back whole.
fn block<'a>(text: &'a str, header: &str) -> &'a str {
    let start = text
        .find(header)
        .unwrap_or_else(|| panic!("{header} is not declared in the frozen text"));
    let open = start + header.len();
    let mut depth = 1_usize;
    for (offset, byte) in text[open..].bytes().enumerate() {
        match byte {
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    return &text[open..open + offset];
                }
            }
            _ => {}
        }
    }
    panic!("{header} is not closed");
}

/// A copy of `text` without its comment lines.
fn declared(text: &str) -> String {
    text.lines()
        .map(str::trim_end)
        .filter(|line| {
            let trimmed = line.trim_start();
            !(trimmed.starts_with("//") || trimmed.starts_with("///"))
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// The field names of a record body, one per `name: type` line.
fn field_names(body: &str) -> Vec<&str> {
    body.lines()
        .filter_map(|line| {
            let (name, _) = line.trim().split_once(':')?;
            let name = name.trim();
            let kebab = !name.is_empty()
                && name
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
            kebab.then_some(name)
        })
        .collect()
}

/// The boundary's three credential-shaped interfaces are vocabulary, not accessors:
/// a component can name how the route's credential is attached but can never obtain
/// one. Their text is parsed here, so an added `func` (or a record field named like a
/// credential in the provider world's settings or request) fails this case.
#[test]
fn the_boundary_hands_out_no_credential_and_no_record_names_one() {
    let transport = declared(TRANSPORT_WIT);
    let worlds = declared(WORLDS_WIT);

    for (interface, anchor) in [
        ("interface credential-control {", "record credential-use"),
        ("interface http {", "record http-request"),
        ("interface websocket {", "record websocket-send"),
    ] {
        let body = block(&transport, interface);
        assert!(
            body.contains(anchor),
            "{interface} must still declare {anchor}"
        );
        assert!(
            !body.contains("func"),
            "{interface} must declare no function: nothing here returns a credential"
        );
        assert!(
            !body.contains("->"),
            "{interface} must have no return type: nothing here returns a credential"
        );
    }

    let world = block(&worlds, "world provider {");
    let settings = block(world, "record provider-settings {");
    let request = block(world, "record provider-request {");
    // The settings a component is configured with are exactly these five fields, and
    // the request exactly these four: a credential-shaped field cannot be added
    // without failing here.
    assert_eq!(
        field_names(settings),
        vec![
            "origin-route",
            "endpoint",
            "model",
            "wire-model",
            "adapter-settings"
        ]
    );
    assert_eq!(
        field_names(request),
        vec!["system-prompt", "history", "tools", "options"]
    );
    for (record, body) in [
        ("provider-settings", settings),
        ("provider-request", request),
    ] {
        for name in field_names(body) {
            for forbidden in [
                "token",
                "secret",
                "bearer",
                "api-key",
                "credential",
                "password",
                "refresh",
            ] {
                assert!(
                    !name.contains(forbidden),
                    "{record}.{name} names a credential"
                );
            }
        }
    }
}

/// A lowered request carries a path, headers, a credential PLACEMENT and a body —
/// nothing that selects an endpoint or a credential source. The exhaustive
/// destructuring stops this file compiling if such a field is ever added, and the
/// same request sent on two routes proves the authority alone decides both.
#[tokio::test]
async fn a_lowered_request_cannot_choose_an_endpoint_or_a_credential_source() {
    let LoweredHttpRequest {
        path,
        headers,
        credential,
        body,
    } = lowered("/responses");
    let CredentialUse {
        scheme: CredentialScheme::Bearer,
        account_id_header,
    } = credential;
    assert_eq!(path, "/responses");
    assert_eq!(headers.len(), 2);
    assert!(account_id_header.is_none());
    assert_eq!(body, br#"{"stream":true}"#.to_vec());

    let request = lowered("/responses");
    let attaching = Source::new(None);
    let attaching_route = authority("https://a.test/v1", &attaching);
    let silent = Source::proxy();
    let silent_route = authority("https://b.test/v2", &silent);

    let first_transport = ScriptedTransport::new(vec![done()]);
    let first = start(&attaching_route, &first_transport, &request).expect("a valid request");
    assert!(matches!(outcome(first).await, Outcome::Completed(_)));
    let second_transport = ScriptedTransport::new(vec![done()]);
    let second = start(&silent_route, &second_transport, &request).expect("a valid request");
    assert!(matches!(outcome(second).await, Outcome::Completed(_)));

    let first_sent = first_transport.requests();
    let second_sent = second_transport.requests();
    assert_eq!(first_sent[0].url, "https://a.test/v1/responses");
    assert_eq!(second_sent[0].url, "https://b.test/v2/responses");
    assert_eq!(
        first_sent[0].headers[0],
        ("authorization".to_string(), format!("Bearer {BEARER}")),
        "the credential is the route's, attached by the broker"
    );
    assert_eq!(
        second_sent[0].headers, request.headers,
        "a route that attaches nothing sends exactly the lowered headers"
    );
}

/// No credential value and no credential path reaches a `Debug`, a message or a
/// notice: not the route's, not the lowered or validated request's, not a sent
/// request's, not the broker's own failures and not the stream the operator reads.
#[tokio::test]
async fn no_credential_reaches_a_debug_a_message_or_a_notice() {
    // The authority, the lowered request and the validated request.
    let source = Source::new(Some(ACCOUNT));
    let route = authority(ENDPOINT, &source);
    // The source's own store path is watched like any credential value it holds.
    let sentinels = [BEARER, REFRESHED, ACCOUNT, source.credential_path()];
    let assert_clean = |label: &str, text: &str| {
        for sentinel in sentinels {
            assert!(
                !text.contains(sentinel),
                "{label} carries {sentinel}: {text}"
            );
        }
    };

    let request = with_account_header(lowered("/responses?key=QUERY-SENTINEL"));
    let validated = route.validate(&request).expect("a valid request");
    let authority_debug = format!("{route:?}");
    assert_clean("the authority's Debug", &authority_debug);
    assert_clean("the lowered request's Debug", &format!("{request:?}"));
    assert_clean("the validated request's Debug", &format!("{validated:?}"));
    assert!(
        authority_debug.contains("provider.test"),
        "the endpoint stays diagnostic: {authority_debug}"
    );
    assert!(
        !authority_debug.contains("QUERY-SENTINEL"),
        "a query string never reaches a Debug: {authority_debug}"
    );

    // A refusal of a header that carries a credential value.
    let mut refused = lowered("/responses");
    refused
        .headers
        .push(("authorization".to_string(), format!("Bearer {BEARER}")));
    let error = start(&route, &ScriptedTransport::new(Vec::new()), &refused)
        .err()
        .expect("the credential header is the broker's");
    assert_clean("a refusal's message", &format!("{error:?}"));

    // A retrying route: the notice the operator reads and every event of the stream.
    let retry_source = Source::new(Some(ACCOUNT));
    let retry_route = authority(ENDPOINT, &retry_source);
    let retry_transport = ScriptedTransport::new(vec![status_response(500), done()]);
    let collected = events(
        start_with_retry(
            &retry_route,
            &retry_transport,
            &lowered("/responses"),
            no_delay_policy(),
        )
        .expect("a valid request"),
    )
    .await;
    assert!(
        collected
            .iter()
            .any(|event| matches!(event, StreamEvent::Notice { .. })),
        "the retry notice is a surface under test: {collected:?}"
    );
    assert_clean("the stream's events", &format!("{collected:?}"));
    for sent in retry_transport.requests() {
        assert_clean("a sent request's Debug", &format!("{sent:?}"));
    }

    // A refresh: the refreshed value never reaches a Debug, a message or an event.
    let refresh_source = Source::new(Some(ACCOUNT));
    let refresh_route = authority(ENDPOINT, &refresh_source);
    let refresh_transport =
        ScriptedTransport::new(vec![status_response(401), status_response(401)]);
    let collected = events(
        start(&refresh_route, &refresh_transport, &lowered("/responses")).expect("a valid request"),
    )
    .await;
    assert_clean("the events after a refresh", &format!("{collected:?}"));
    for sent in refresh_transport.requests() {
        assert_clean("a retried request's Debug", &format!("{sent:?}"));
    }

    // The broker's own failure for a request that names an account-id header the
    // route's credential cannot fill.
    let no_account = Source::new(None);
    let no_account_route = authority(ENDPOINT, &no_account);
    let error = failure(
        outcome(
            start(
                &no_account_route,
                &ScriptedTransport::new(Vec::new()),
                &with_account_header(lowered("/responses")),
            )
            .expect("a valid request"),
        )
        .await,
    );
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_clean("the account-id failure", &format!("{error:?}"));

    // A proxy-injected route's refusal.
    let proxy = Source::proxy();
    let proxy_route = authority(ENDPOINT, &proxy);
    let error = failure(
        outcome(
            start(
                &proxy_route,
                &ScriptedTransport::new(vec![status_response(401)]),
                &lowered("/responses"),
            )
            .expect("a valid request"),
        )
        .await,
    );
    assert_clean("the proxy refusal", &format!("{error:?}"));
}
