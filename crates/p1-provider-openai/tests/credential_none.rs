//! Issue #134: a route whose credential an egress proxy injects sends NO
//! authentication header. This is the Responses adapter's half of the claim — the
//! source says `proxy_injected()`, and neither transport sends a credential:
//!
//! - the SSE request carries no `Authorization` header (and, on this account, no
//!   account-id header either, because the proxy supplies the whole credential);
//! - the WebSocket handshake carries neither, and a 401/403 that refuses the upgrade
//!   ends the turn at once — no refresh, no reconnect, one handshake.
//!
//! Every case has its contrast: a route that resolves its own credential still sends
//! both headers, and still refreshes once when the upgrade is refused. Everything is
//! offline: a scripted HTTP transport, a scripted WebSocket peer, no credential file.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, StreamEvent,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_conformance::fixtures::responses as responses_fixtures;
use p1_provider_http::testing::{
    BodyEnd, ScriptedConnection, ScriptedFrame, ScriptedResponse, ScriptedTransport,
    ScriptedWsConnector,
};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{
    OpenAiCodexProvider, ResponsesAccount, ResponsesRoute, ResponsesTransport,
};

const BEARER: &str = "NONE-TEST-FAKE-BEARER";
const ACCOUNT_ID: &str = "NONE-TEST-FAKE-ACCOUNT";
/// A refused upgrade's body: classification input, never a message.
const REFUSAL_BODY: &[u8] = b"NONE-TEST-FAKE-REFUSAL-BODY";

/// Every header name that carries (or names) a credential.
const CREDENTIAL_HEADERS: [&str; 5] = [
    "authorization",
    "proxy-authorization",
    "x-api-key",
    "api-key",
    "chatgpt-account-id",
];

/// A source that resolves a credential of its own, account id included.
struct Resolved;

impl CredentialSource for Resolved {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: Some(ACCOUNT_ID.into()),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        self.access()
    }
}

/// The source of a route whose credential an egress proxy injects: the placeholder
/// `p1_auth::resolve` hands every adapter for `kind = "none"`. It has no account id
/// (the account-id guard must let it through unchanged), and every refresh it is
/// asked for is counted — on this route there must be none.
#[derive(Default)]
struct ProxyInjected {
    refresh_calls: AtomicUsize,
}

impl ProxyInjected {
    fn refresh_calls(&self) -> usize {
        self.refresh_calls.load(Ordering::SeqCst)
    }
}

impl CredentialSource for ProxyInjected {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: String::new(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        self.refresh_calls.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {
            Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                "a route that sends no credential has nothing to refresh",
            ))
        })
    }

    fn proxy_injected(&self) -> bool {
        true
    }
}

fn codex_route(transport: ResponsesTransport) -> ResponsesRoute {
    ResponsesRoute {
        origin_route: p1_provider_openai::ROUTE.to_string(),
        endpoint: "https://chatgpt.com/backend-api".to_string(),
        account: ResponsesAccount::CodexSubscription,
        transport,
    }
}

fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: "gpt-example".into(),
        revision: 1,
        model_id: "gpt-example".into(),
        family: "gpt".into(),
        thinking: ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::High],
        default_effort: Some(Effort::High),
        thinking_budgets: BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    })
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![],
        options: ModelOptions::default(),
    }
}

/// The provider over a scripted HTTP transport, for the SSE cases.
fn sse_provider(
    credentials: Arc<dyn CredentialSource>,
    transport: ScriptedTransport,
) -> OpenAiCodexProvider {
    OpenAiCodexProvider::new(
        codex_route(ResponsesTransport::Sse),
        "gpt-example",
        profile(),
        Arc::new(transport),
        credentials,
    )
    .expect("the route and profile compose")
}

/// The provider over a scripted WebSocket peer. The SSE transport is scripted with
/// NO response, so a fallback would panic: "no fallback" is asserted, not assumed.
fn websocket_provider(
    credentials: Arc<dyn CredentialSource>,
    connections: Vec<ScriptedConnection>,
) -> (OpenAiCodexProvider, ScriptedWsConnector) {
    let connector = ScriptedWsConnector::new(connections);
    let provider = OpenAiCodexProvider::builder(
        codex_route(ResponsesTransport::Websocket),
        "gpt-example",
        profile(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        credentials,
    )
    .with_ws_connector(Arc::new(connector.clone()))
    .build()
    .expect("a websocket route composes with its connector");
    (provider, connector)
}

/// One WebSocket turn's text frames: each SSE `data:` line is ONE text frame
/// (`docs/design/websocket.md` §3), so the same transcript is the same events.
fn text_frames(sse: &str) -> Vec<ScriptedFrame> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(ScriptedFrame::text)
        .collect()
}

async fn collect(mut stream: ProviderStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

async fn turn(provider: &OpenAiCodexProvider) -> Vec<StreamEvent> {
    collect(
        provider
            .stream(request(), CancellationToken::new())
            .await
            .expect("the request is buildable"),
    )
    .await
}

fn terminal(events: &[StreamEvent]) -> &Outcome {
    match events.last() {
        Some(StreamEvent::Finished(outcome)) => outcome,
        other => panic!("the stream did not end with Finished: {other:?}"),
    }
}

fn completed(events: &[StreamEvent]) {
    assert!(
        matches!(terminal(events), Outcome::Completed(_)),
        "{events:?}"
    );
}

fn header<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(sent, _)| sent.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

fn header_names(headers: &[(String, String)]) -> Vec<&str> {
    headers.iter().map(|(name, _)| name.as_str()).collect()
}

/// One request's headers, as the transport recorded them. The scripted body ends
/// right after, so the first poll reaches the post and nothing is retried.
async fn posted_headers(credentials: Arc<dyn CredentialSource>) -> Vec<(String, String)> {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = sse_provider(credentials, transport.clone());
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    let _ = stream.next().await;

    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.into_iter().next().unwrap().headers
}

// ------------------------------------------------------------------ the SSE request

#[tokio::test]
async fn a_proxy_injected_route_sends_no_authentication_header() {
    let headers = posted_headers(Arc::new(ProxyInjected::default())).await;
    for name in CREDENTIAL_HEADERS {
        assert!(
            header(&headers, name).is_none(),
            "{name} was sent: {headers:?}"
        );
    }
    // The client identity every request carries is unchanged.
    assert_eq!(header(&headers, "originator"), Some("p1"));
}

#[tokio::test]
async fn a_route_that_resolves_its_own_credential_still_sends_it() {
    let headers = posted_headers(Arc::new(Resolved)).await;
    assert_eq!(
        header(&headers, "authorization"),
        Some(&format!("Bearer {BEARER}")[..]),
        "{headers:?}"
    );
    assert_eq!(
        header(&headers, "chatgpt-account-id"),
        Some(ACCOUNT_ID),
        "{headers:?}"
    );
}

// ------------------------------------------------------- the WebSocket handshake

/// `docs/design/websocket.md` §3's header list, minus the two credential headers: an
/// egress proxy injects the credential, so p1 sends none — and the list is otherwise
/// byte for byte the resolving route's.
#[tokio::test]
async fn a_proxy_injected_handshake_carries_no_authentication_header() {
    let (provider, connector) = websocket_provider(
        Arc::new(ProxyInjected::default()),
        vec![ScriptedConnection::accept(text_frames(
            responses_fixtures::NO_USAGE,
        ))],
    );
    let events = turn(&provider).await;
    completed(&events);

    let handshakes = connector.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert_eq!(
        header_names(&handshakes[0].headers),
        ["originator", "User-Agent", "OpenAI-Beta"],
        "§3's list minus Authorization and chatgpt-account-id"
    );
    for name in CREDENTIAL_HEADERS {
        assert!(
            header(&handshakes[0].headers, name).is_none(),
            "{name} was sent: {:?}",
            handshakes[0].headers
        );
    }
    assert_eq!(header(&handshakes[0].headers, "originator"), Some("p1"));
    assert_eq!(
        header(&handshakes[0].headers, "OpenAI-Beta"),
        Some("responses_websockets=2026-02-06"),
        "the WebSocket beta value is not a credential: it stays"
    );
}

/// The contrast that makes the list above mean something: the very same handshake
/// code, with a source that resolves its own credential, sends both headers.
#[tokio::test]
async fn a_resolving_credential_still_reaches_the_handshake() {
    let (provider, connector) = websocket_provider(
        Arc::new(Resolved),
        vec![ScriptedConnection::accept(text_frames(
            responses_fixtures::NO_USAGE,
        ))],
    );
    let events = turn(&provider).await;
    completed(&events);

    let handshakes = connector.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert_eq!(
        header_names(&handshakes[0].headers),
        [
            "Authorization",
            "chatgpt-account-id",
            "originator",
            "User-Agent",
            "OpenAI-Beta",
        ],
        "the same list WITH the credential headers"
    );
    assert_eq!(
        header(&handshakes[0].headers, "Authorization"),
        Some(&format!("Bearer {BEARER}")[..])
    );
    assert_eq!(
        header(&handshakes[0].headers, "chatgpt-account-id"),
        Some(ACCOUNT_ID)
    );
}

// -------------------------------------------------------- the refused upgrade (401)

/// A 401 that refuses the upgrade on a proxy-injected route is the EGRESS PROXY's
/// refusal, not a rejected key: the turn ends at once, with the driver's own message,
/// and `refresh` is never called.
#[tokio::test]
async fn a_refused_upgrade_never_refreshes_and_names_the_proxy_credential() {
    let credentials = Arc::new(ProxyInjected::default());
    let (provider, connector) = websocket_provider(
        credentials.clone(),
        vec![ScriptedConnection::refuse(401, REFUSAL_BODY)],
    );
    let events = turn(&provider).await;

    match terminal(&events) {
        Outcome::Failed(error) => {
            assert_eq!(error.kind, ProviderErrorKind::Authentication);
            for part in ["proxy credential", "kind = \"none\"", "HTTP 401"] {
                assert!(error.message.contains(part), "{}: {part}", error.message);
            }
            assert!(
                !error.message.contains("nothing to refresh"),
                "the source's own refusal must not surface: {}",
                error.message
            );
        }
        other => panic!("expected an authentication failure, got {other:?}"),
    }
    assert_eq!(
        credentials.refresh_calls(),
        0,
        "a route that sends no credential has nothing to refresh"
    );
    assert_eq!(
        connector.handshakes().len(),
        1,
        "one handshake: no refresh, no reconnect"
    );
}

/// The contrast: the same refusal on a route that resolves its own credential still
/// refreshes once and reconnects, so the case above is the proxy-injected route's
/// behaviour and not "a refused upgrade never refreshes".
#[tokio::test]
async fn a_refused_upgrade_on_a_resolving_route_still_refreshes_and_reconnects() {
    let (provider, connector) = websocket_provider(
        Arc::new(Resolved),
        vec![
            ScriptedConnection::refuse(401, REFUSAL_BODY),
            ScriptedConnection::accept(text_frames(responses_fixtures::NO_USAGE)),
        ],
    );
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(
        connector.handshakes().len(),
        2,
        "the refusal refreshed the credential and reconnected once"
    );
}
