//! The [`Provider`] implementation: contract in, codex stream out.
//!
//! The adapter holds one composed instance — a [`ResponsesRoute`], a wire model
//! and a [`ModelProfile`] — and one credential source, and translates a
//! [`ProviderRequest`] into a Responses request. All retry, refresh, cancellation
//! and terminal-event policy lives in [`p1_provider_http::drive`]; this module
//! only decides whether a request is buildable at all, through the same pure
//! lowering function the request builder uses (ADR-0039, spec §7.3).
//!
//! A route that asks for `transport = "websocket"` sends the same request over
//! [`crate::websocket`] instead, whose failure policy is `drive`'s re-expressed
//! (ADR-0047, `docs/design/websocket.md` §4–§5). Both transports end in the same
//! [`p1_contracts::StreamEvent`]s.

use std::sync::Arc;
use std::time::Instant;

use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, ModelOptions, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription,
};
use p1_model_profile::ModelProfile;
use p1_provider_http::ws::WsConnector;
use p1_provider_http::{
    Credential, CredentialSource, DriveRequest, HttpRequest, ResponseParser, RetryPolicy,
    Transport, drive,
};

use crate::ResponsesRoute;
use crate::parser::CodexResponseParser;
use crate::request::{
    build_headers, build_request, clamped_cache_key, lower, resolve_base_url,
    validate as validate_options,
};
use crate::websocket::{self, WebSocket};

pub use crate::websocket::Clock;

/// Namespaces the OTHER compiled adapters own inside `ModelOptions::native`. An
/// explicit option from one of them was silently dropped on a route switch
/// before; it is now an error naming the option, this route and this adapter
/// (ADR-0039). Keys in no adapter's namespace keep their meaning: ignored.
const FOREIGN_NATIVE_PREFIXES: &[&str] = &["anthropic-messages.", "openai-chat."];

/// The adapter half of this route's identity, for error messages.
const ADAPTER: &str = "openai-responses";

/// One route file composed with one profile and one credential source.
pub struct OpenAiCodexProvider {
    route: ResponsesRoute,
    wire_model: String,
    profile: Arc<ModelProfile>,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    retry: RetryPolicy,
    /// The WebSocket half, when the route asks for it (ADR-0047 §1). `None` on an
    /// SSE route, where every request takes today's `drive()` path unchanged.
    ws: Option<Arc<WebSocket>>,
}

/// The composition of one Responses provider: the constructor's inputs plus the
/// connector a route that asks for `transport = "websocket"` speaks through.
///
/// [`build`](OpenAiCodexProviderBuilder::build) is where the two halves are
/// paired, so a WebSocket route can never be composed without a connector (the
/// mistake fails at construction, never at the first request) and an SSE route can
/// never carry one.
pub struct OpenAiCodexProviderBuilder {
    route: ResponsesRoute,
    wire_model: String,
    profile: Arc<ModelProfile>,
    transport: Arc<dyn Transport>,
    credentials: Arc<dyn CredentialSource>,
    connector: Option<Arc<dyn WsConnector>>,
    clock: Clock,
}

impl OpenAiCodexProvider {
    /// Compose the adapter from its three inputs (ADR-0039). A profile whose
    /// thinking policy the Responses wire cannot express fails HERE, before any
    /// request exists; so does a route that asks for WebSocket, which needs its
    /// connector (see [`OpenAiCodexProvider::builder`]).
    ///
    /// Construction reads no credential file: the token file is only opened by
    /// the first `access` call, inside the driver. The credential source is
    /// wrapped so a credential without the ChatGPT account id this account needs
    /// is reported as an authentication failure, never a malformed request.
    pub fn new(
        route: ResponsesRoute,
        wire_model: &str,
        profile: Arc<ModelProfile>,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> Result<Self, ProviderError> {
        Self::builder(route, wire_model, profile, transport, credentials).build()
    }

    /// The composition builder: the same inputs as [`OpenAiCodexProvider::new`],
    /// plus the connector a `websocket` route needs. Everything the constructor
    /// checks is checked by [`build`](OpenAiCodexProviderBuilder::build).
    pub fn builder(
        route: ResponsesRoute,
        wire_model: &str,
        profile: Arc<ModelProfile>,
        transport: Arc<dyn Transport>,
        credentials: Arc<dyn CredentialSource>,
    ) -> OpenAiCodexProviderBuilder {
        OpenAiCodexProviderBuilder {
            route,
            wire_model: wire_model.to_string(),
            profile,
            transport,
            credentials,
            connector: None,
            clock: Arc::new(Instant::now),
        }
    }

    /// Override the transient retry policy (tests and hosts).
    pub fn with_retry(mut self, retry: RetryPolicy) -> Self {
        self.retry = retry;
        self
    }

    /// Override the endpoint base (embedding and tests). The route data of a
    /// composed provider comes from its file; the `/codex/responses` path is
    /// appended when the base does not already end with it.
    pub fn with_base_url(mut self, url: &str) -> Self {
        self.route.endpoint = url.to_string();
        self
    }

    /// Today's path for one request, as something callable more than once: an SSE
    /// route returns it directly, and the WebSocket arm runs it for the request
    /// that fell back (§5). Nothing in it depends on the transport the route asks
    /// for, which is why the SSE arm stays byte for byte what it was.
    fn sse_path(
        &self,
        url: &str,
        body: Vec<u8>,
        cache_key: Option<String>,
        cancel: CancellationToken,
    ) -> Box<dyn Fn() -> ProviderStream + Send> {
        let transport = self.transport.clone();
        let credentials = self.credentials.clone();
        let model = self.wire_model.clone();
        let account = self.route.account;
        let origin_route = self.route.origin_route.clone();
        let retry = self.retry;
        let url = url.to_string();
        Box::new(move || {
            let url = url.clone();
            let body = body.clone();
            let cache_key = cache_key.clone();
            let origin_route = origin_route.clone();
            let model = model.clone();
            drive(DriveRequest {
                transport: transport.clone(),
                credentials: credentials.clone(),
                build: Box::new(move |credential: &Credential| {
                    // Unreachable by construction: `AccountIdGuard` turns a
                    // credential without an account id into an authentication
                    // failure before the driver can build a request.
                    let headers = build_headers(account, credential, cache_key.as_deref())
                        .expect("the credential guard guarantees a ChatGPT account id");
                    HttpRequest {
                        url: url.clone(),
                        headers,
                        body: body.clone(),
                    }
                }),
                new_parser: Box::new(move || {
                    Box::new(CodexResponseParser::new(&origin_route, &model))
                        as Box<dyn ResponseParser>
                }),
                retry,
                cancel: cancel.clone(),
            })
        })
    }
}

impl OpenAiCodexProviderBuilder {
    /// Hand the provider the connector its route asks for (ADR-0047 §1). A route
    /// that does not ask for WebSocket is refused at
    /// [`build`](OpenAiCodexProviderBuilder::build): the connector belongs to the
    /// route's transport, so the two cannot drift apart.
    pub fn with_ws_connector(mut self, connector: Arc<dyn WsConnector>) -> Self {
        self.connector = Some(connector);
        self
    }

    /// The clock the WebSocket connection-reuse policy reads
    /// (`docs/design/websocket.md` §4). Tests inject one; the default is the real
    /// clock.
    pub fn with_clock(mut self, clock: Clock) -> Self {
        self.clock = clock;
        self
    }

    /// Finish the composition, pairing the route's transport with its connector.
    pub fn build(self) -> Result<OpenAiCodexProvider, ProviderError> {
        let Self {
            route,
            wire_model,
            profile,
            transport,
            credentials,
            connector,
            clock,
        } = self;
        validate_composition(&route, &wire_model, &profile)?;
        let ws = match (route.transport, connector) {
            (crate::ResponsesTransport::Sse, None) => None,
            (crate::ResponsesTransport::Websocket, Some(connector)) => {
                Some(Arc::new(WebSocket::new(connector, clock)))
            }
            (crate::ResponsesTransport::Websocket, None) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "the route asks for `transport = \"websocket\"`, which needs a WebSocket \
                     connector: compose this provider with `with_ws_connector`",
                ));
            }
            (crate::ResponsesTransport::Sse, Some(_)) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "this route speaks SSE, so it takes no WebSocket connector: write \
                     `transport = \"websocket\"` in its route file first",
                ));
            }
        };
        let credentials = if route.account.requires_account_id() {
            Arc::new(AccountIdGuard { inner: credentials }) as Arc<dyn CredentialSource>
        } else {
            credentials
        };
        Ok(OpenAiCodexProvider {
            route,
            wire_model,
            profile,
            transport,
            credentials,
            retry: RetryPolicy::default(),
            ws,
        })
    }
}

/// The one composition check, shared by the constructor and the pure request
/// builder: the route data is usable, the profile is valid, and the profile's
/// policy has an encoding here. Nothing is decided by a second, parallel table.
pub(crate) fn validate_composition(
    route: &ResponsesRoute,
    wire_model: &str,
    profile: &ModelProfile,
) -> Result<(), ProviderError> {
    route.validate()?;
    profile.validate()?;
    if wire_model.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidRequest,
            "wire model must be nonempty",
        ));
    }
    // The pure lowering decides: a `budget`, `enabled` or `preserved` profile has
    // no Responses encoding, so it is refused here, at construction.
    lower(profile, &ModelOptions::default())?;
    Ok(())
}

impl Provider for OpenAiCodexProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.route.origin(&self.wire_model),
            supports_freeform_tools: true,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            // The request builder sends `options.cache_key` as the body's
            // `prompt_cache_key` and the session identity headers.
            cache_key: CacheKeySupport::Optional,
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        for key in request.options.native.keys() {
            if FOREIGN_NATIVE_PREFIXES
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!(
                        "option \"{key}\" is not consumed by route \"{}\" \
                         (adapter {ADAPTER}): it belongs to another adapter's namespace",
                        self.route.origin_route
                    ),
                ));
            }
        }
        if request.options.cache_key.as_deref() == Some("") {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "cache_key must not be empty: set a stable nonempty key or leave it unset",
            ));
        }
        // The model policy: the same lowering the request builder runs, so
        // `validate` can never accept a request the builder would reject.
        validate_options(self.route.account, &self.profile, &request.options)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            // These are the only setup failures: unsupported explicit options or
            // a request the route cannot encode. Everything network-shaped is a
            // terminal event inside the returned stream.
            self.validate(&request)?;
            let url = resolve_base_url(&self.route.endpoint)?;
            let body = build_request(&self.route, &self.wire_model, &self.profile, &request)?;
            let body_bytes = serde_json::to_vec(&body).map_err(|_| {
                ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "failed to serialize the request body",
                )
            })?;
            // The same clamped key as the body's `prompt_cache_key`, sent as
            // the session identity headers.
            let cache_key = clamped_cache_key(&request.options);

            // Today's request, byte for byte: the same body bytes, the same
            // headers and the same driver. A WebSocket failure before any output
            // runs exactly this for the request that hit it (§5).
            let sse = self.sse_path(&url, body_bytes, cache_key.clone(), cancel.clone());

            // §1/§4: WebSocket only when the route asks for it, this instance has
            // not fallen back already, and the connection is not busy with another
            // request. A busy connection is never waited for and never doubled.
            let Some(ws) = self.ws.as_ref() else {
                return Ok(sse());
            };
            if ws.is_disabled() {
                return Ok(sse());
            }
            let Some(slot) = ws.try_take() else {
                return Ok(sse());
            };
            Ok(websocket::stream(
                websocket::WebSocketRequest {
                    ws: ws.clone(),
                    url,
                    body,
                    account: self.route.account,
                    cache_key,
                    credentials: self.credentials.clone(),
                    origin_route: self.route.origin_route.clone(),
                    model: self.wire_model.clone(),
                    sse,
                    cancel,
                },
                slot,
            ))
        })
    }
}

impl std::fmt::Debug for OpenAiCodexProvider {
    /// Never prints the credential source; only the route identity is safe.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCodexProvider")
            .field("route", &self.route)
            .field("wire_model", &self.wire_model)
            .finish_non_exhaustive()
    }
}

/// Reject a credential that cannot name the ChatGPT account. This is the one
/// enforcement point for [`build_headers`]' account-id requirement on the live
/// path, so the header builder itself is infallible once a credential exists.
struct AccountIdGuard {
    inner: Arc<dyn CredentialSource>,
}

impl CredentialSource for AccountIdGuard {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let credential = self.inner.access().await?;
            require_account_id(credential)
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let credential = self.inner.refresh(rejected).await?;
            require_account_id(credential)
        })
    }
}

fn require_account_id(credential: Credential) -> Result<Credential, ProviderError> {
    match credential.account_id.as_deref() {
        Some(account_id) if !account_id.is_empty() => Ok(credential),
        _ => Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Codex credential has no ChatGPT account id",
        )),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use futures_util::StreamExt;
    use p1_contracts::{Effort, ModelOptions, Outcome, Provider};
    use p1_model_profile::ThinkingPolicy;
    use p1_provider_http::testing::ScriptedTransport;

    use super::*;

    struct NoCredentials;

    impl CredentialSource for NoCredentials {
        fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async {
                Ok(Credential {
                    bearer: "SENTINEL-ACCESS".to_string(),
                    account_id: None,
                })
            })
        }

        fn refresh<'a>(
            &'a self,
            _rejected: &'a Credential,
        ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
            Box::pin(async {
                Ok(Credential {
                    bearer: "SENTINEL-ACCESS".to_string(),
                    account_id: None,
                })
            })
        }
    }

    /// The route data these expectations were recorded with (spec §7.2).
    fn route() -> ResponsesRoute {
        ResponsesRoute {
            origin_route: crate::ROUTE.to_string(),
            endpoint: "https://chatgpt.com/backend-api".to_string(),
            account: crate::ResponsesAccount::CodexSubscription,
            transport: crate::ResponsesTransport::Sse,
        }
    }

    /// The model policy these expectations were recorded with: any model name took
    /// an effort level, and only `low`/`medium`/`high` (spec §7.1).
    fn profile() -> Arc<ModelProfile> {
        Arc::new(ModelProfile {
            id: "gpt-test".to_string(),
            revision: 1,
            model_id: "gpt-test".to_string(),
            family: "gpt".to_string(),
            thinking: ThinkingPolicy::EffortLevel,
            efforts: vec![Effort::Low, Effort::Medium, Effort::High],
            default_effort: None,
            thinking_budgets: BTreeMap::new(),
            context_tokens: None,
            max_output_tokens: None,
        })
    }

    fn provider() -> OpenAiCodexProvider {
        OpenAiCodexProvider::new(
            route(),
            "gpt-test",
            profile(),
            Arc::new(ScriptedTransport::new(Vec::new())),
            Arc::new(NoCredentials),
        )
        .expect("the profile is expressible on the Responses wire")
        .with_base_url("https://example.test/backend")
    }

    fn request(options: ModelOptions) -> ProviderRequest {
        ProviderRequest {
            system_prompt: String::new(),
            history: Vec::new(),
            tools: Vec::new(),
            options,
        }
    }

    #[test]
    fn describe_reports_the_route_and_freeform_support() {
        let description = provider().describe();
        assert_eq!(description.origin.route, crate::ROUTE);
        assert_eq!(description.origin.model, "gpt-test");
        assert!(description.supports_freeform_tools);
        assert!(description.mandatory_prompt_prefix.is_none());
        assert!(!description.reports_cost);
        assert_eq!(description.cache_key, CacheKeySupport::Optional);
    }

    #[test]
    fn validate_rejects_max_output_tokens() {
        let options = ModelOptions {
            max_output_tokens: Some(1),
            ..ModelOptions::default()
        };
        assert_eq!(
            provider().validate(&request(options)).unwrap_err().kind,
            ProviderErrorKind::InvalidRequest
        );
    }

    #[test]
    fn an_empty_explicit_cache_key_is_rejected_not_sent() {
        let options = ModelOptions {
            cache_key: Some(String::new()),
            ..ModelOptions::default()
        };
        let error = provider().validate(&request(options)).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
        assert!(error.message.contains("cache_key"), "{}", error);
    }

    #[test]
    fn a_native_option_in_another_adapters_namespace_is_an_error() {
        for key in ["anthropic-messages.thinking", "openai-chat.future_flag"] {
            let mut options = ModelOptions::default();
            options
                .native
                .insert(key.to_string(), serde_json::json!(true));
            let error = provider().validate(&request(options)).unwrap_err();
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest, "{key}");
            for part in [
                format!("option \"{key}\""),
                format!("route \"{}\"", crate::ROUTE),
                format!("(adapter {ADAPTER})"),
            ] {
                assert!(error.message.contains(&part), "{}: {}", error, part);
            }
        }
    }

    #[test]
    fn a_native_option_in_no_adapters_namespace_keeps_its_meaning() {
        let mut options = ModelOptions::default();
        options
            .native
            .insert("unrelated.option".to_string(), serde_json::json!(1));
        assert!(provider().validate(&request(options)).is_ok());
    }

    #[tokio::test]
    async fn a_credential_without_an_account_id_fails_authentication() {
        let provider = provider();
        let mut stream = provider
            .stream(request(ModelOptions::default()), CancellationToken::new())
            .await
            .expect("setup succeeds");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            events.push(event);
        }
        match events.last() {
            Some(p1_contracts::StreamEvent::Finished(Outcome::Failed(error))) => {
                assert_eq!(error.kind, ProviderErrorKind::Authentication);
            }
            other => panic!("expected authentication failure, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn debug_does_not_print_credentials() {
        let provider = provider();
        let text = format!("{provider:?}");
        assert!(!text.contains("SENTINEL-ACCESS"));
        assert!(text.contains("gpt-test"));
    }
}
