//! The WebSocket transport of the Responses adapter (ADR-0047,
//! `docs/design/websocket.md` §1, §3, §4, §5), entirely offline: a scripted peer,
//! a scripted HTTP transport for the fallback arm, and an injected clock. No test
//! here touches the network or a credential file.
//!
//! Every row of §5's table is a named test, §3's handshake and frame are pinned
//! byte for byte, and §4's lifetime rules (reuse, busy, cancellation, the slot)
//! each have their own. The SSE arm of a WebSocket provider is a
//! [`ScriptedTransport`] with NO scripted response wherever a fallback must NOT
//! happen: asking for one panics, so "no fallback" is asserted, not assumed.

mod fixtures;

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, StreamEvent,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{
    ScriptedConnection, ScriptedFrame, ScriptedResponse, ScriptedTransport, ScriptedWsConnector,
};
use p1_provider_http::ws::{WsConnectError, WsConnection, WsConnector, WsError, WsHandshake};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{
    Clock, OpenAiCodexProvider, ROUTE, ResponsesAccount, ResponsesAdapterSettings, ResponsesRoute,
    ResponsesTransport,
};
use serde_json::{Value, json};

const MODEL: &str = "gpt-test";
/// The bearer the fake credential source hands out, and the one a refresh returns:
/// both are sentinels, and neither may appear in a `Debug` output or an error.
const BEARER: &str = "SENTINEL-WS-BEARER";
const REFRESHED: &str = "SENTINEL-WS-REFRESHED";
const ACCOUNT_ID: &str = "acct_test";
/// The cache key these tests send, which becomes two header values.
const CACHE_KEY: &str = "agent-a-key";
/// A refusal body: classification input, never a message.
const REFUSAL_BODY: &[u8] = b"SENTINEL-WS-BODY";

/// One visible delta, the frame that makes everything after it a failure of THIS
/// response (§5).
const DELTA: &str = r#"{"type":"response.output_text.delta","delta":"Hello"}"#;

// ---------------------------------------------------------------------- the fixtures

/// The text frames one SSE fixture becomes: §3 says each received text is ONE JSON
/// event with the vocabulary the SSE parser already dispatches on, so the same
/// transcript is the same events.
fn text_frames(sse: &str) -> Vec<ScriptedFrame> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(ScriptedFrame::text)
        .collect()
}

/// `turns` complete turns of the "no usage" transcript, for a connection that
/// serves several.
fn turn_frames(turns: usize) -> Vec<ScriptedFrame> {
    let mut frames = Vec::new();
    for _ in 0..turns {
        frames.extend(text_frames(fixtures::NO_USAGE));
    }
    frames
}

fn route(transport: ResponsesTransport) -> ResponsesRoute {
    route_at("https://example.test/backend", transport)
}

fn route_at(endpoint: &str, transport: ResponsesTransport) -> ResponsesRoute {
    ResponsesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: endpoint.to_string(),
        account: ResponsesAccount::CodexSubscription,
        transport,
    }
}

/// The model policy these tests compose with: an effort level, as the shipped GPT
/// profiles declare.
fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: MODEL.to_string(),
        revision: 1,
        model_id: MODEL.to_string(),
        family: "gpt".to_string(),
        thinking: ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::Low, Effort::Medium, Effort::High],
        default_effort: None,
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    })
}

/// The credential source: a sentinel bearer with the account id this account
/// needs, and a record of the one refresh §5 allows.
#[derive(Default)]
struct FixedCredentials {
    access_calls: AtomicUsize,
    refresh_calls: Mutex<Vec<Credential>>,
}

impl CredentialSource for FixedCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.access_calls.fetch_add(1, Ordering::SeqCst);
            Ok(Credential {
                bearer: BEARER.to_string(),
                account_id: Some(ACCOUNT_ID.to_string()),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.refresh_calls.lock().unwrap().push(rejected.clone());
            Ok(Credential {
                bearer: REFRESHED.to_string(),
                account_id: Some(ACCOUNT_ID.to_string()),
            })
        })
    }
}

/// A clock a test advances instead of sleeping (`docs/design/websocket.md` §4;
/// AGENTS.md forbids sleep-based timing assertions).
#[derive(Clone)]
struct FakeClock(Arc<Mutex<Instant>>);

impl FakeClock {
    fn new() -> Self {
        Self(Arc::new(Mutex::new(Instant::now())))
    }

    fn advance(&self, by: Duration) {
        *self.0.lock().unwrap() += by;
    }

    fn handle(&self) -> Clock {
        let now = self.0.clone();
        Arc::new(move || *now.lock().unwrap())
    }
}

// ------------------------------------------------------------------- the composition

/// The composed provider: the route's transport, the connector it needs (or none),
/// the clock the reuse policy reads, and the fake credential source.
fn compose_with(
    transport: ResponsesTransport,
    sse: ScriptedTransport,
    connector: Option<Arc<dyn WsConnector>>,
    clock: Option<Clock>,
    credentials: Arc<FixedCredentials>,
) -> Result<OpenAiCodexProvider, ProviderError> {
    let mut composition = OpenAiCodexProvider::builder(
        route(transport),
        MODEL,
        profile(),
        Arc::new(sse),
        credentials,
    );
    if let Some(connector) = connector {
        composition = composition.with_ws_connector(connector);
    }
    if let Some(clock) = clock {
        composition = composition.with_clock(clock);
    }
    composition.build()
}

fn compose(
    transport: ResponsesTransport,
    sse: ScriptedTransport,
    connector: Option<Arc<dyn WsConnector>>,
) -> Result<OpenAiCodexProvider, ProviderError> {
    compose_with(
        transport,
        sse,
        connector,
        None,
        Arc::new(FixedCredentials::default()),
    )
}

/// A provider over a scripted peer. `sse` is the fallback arm: script a response
/// for a test where a fallback is EXPECTED, and none where it must not happen.
fn websocket_provider(
    connections: Vec<ScriptedConnection>,
    sse: ScriptedTransport,
) -> (OpenAiCodexProvider, ScriptedWsConnector) {
    let connector = ScriptedWsConnector::new(connections);
    let provider = compose(
        ResponsesTransport::Websocket,
        sse,
        Some(Arc::new(connector.clone())),
    )
    .expect("a websocket route composes with its connector");
    (provider, connector)
}

fn request() -> ProviderRequest {
    request_with(ModelOptions::default())
}

fn request_with(options: ModelOptions) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history: vec![Item::User {
            text: "hi".to_string(),
        }],
        tools: Vec::new(),
        options,
    }
}

async fn collect(mut stream: ProviderStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

/// One whole turn through a provider.
async fn turn(provider: &OpenAiCodexProvider) -> Vec<StreamEvent> {
    collect(
        provider
            .stream(request(), CancellationToken::new())
            .await
            .expect("the request is buildable"),
    )
    .await
}

fn finished_count(events: &[StreamEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Finished(_)))
        .count()
}

fn terminal(events: &[StreamEvent]) -> &Outcome {
    assert_eq!(finished_count(events), 1, "{events:?}");
    match events.last() {
        Some(StreamEvent::Finished(outcome)) => outcome,
        other => panic!("the terminal event is not last: {other:?}"),
    }
}

fn completed(events: &[StreamEvent]) {
    assert!(
        matches!(terminal(events), Outcome::Completed(_)),
        "{events:?}"
    );
}

fn failed(events: &[StreamEvent]) -> ProviderError {
    match terminal(events) {
        Outcome::Failed(error) => error.clone(),
        other => panic!("expected a failure, got {other:?}"),
    }
}

/// Poll a stream once, expecting it to be held open by a peer that never answers.
/// This is how a test gets a request into the middle of its connection's life.
async fn poll_once(stream: &mut ProviderStream) {
    let poll = stream.next();
    let yield_now = std::pin::pin!(tokio::task::yield_now());
    assert!(
        matches!(select(poll, yield_now).await, Either::Right(_)),
        "the stalled peer must not produce an event"
    );
}

/// A peer that never answers, in the three shapes a stalled socket has: the
/// handshake itself, the one frame, or every read. No network.
#[derive(Default)]
struct PendingState {
    hang_connect: bool,
    hang_send: bool,
    handshakes: AtomicUsize,
    dropped: AtomicUsize,
    sent: Mutex<Vec<String>>,
}

#[derive(Clone, Default)]
struct PendingPeer {
    state: Arc<PendingState>,
}

impl PendingPeer {
    fn stalling_read() -> Arc<Self> {
        Self::new(false, false)
    }

    fn stalling_connect() -> Arc<Self> {
        Self::new(true, false)
    }

    fn stalling_send() -> Arc<Self> {
        Self::new(false, true)
    }

    fn new(hang_connect: bool, hang_send: bool) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(PendingState {
                hang_connect,
                hang_send,
                ..PendingState::default()
            }),
        })
    }

    fn handshakes(&self) -> usize {
        self.state.handshakes.load(Ordering::SeqCst)
    }

    fn dropped(&self) -> usize {
        self.state.dropped.load(Ordering::SeqCst)
    }

    fn sent(&self) -> Vec<String> {
        self.state.sent.lock().unwrap().clone()
    }
}

impl WsConnector for PendingPeer {
    fn connect<'a>(
        &'a self,
        _request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>> {
        self.state.handshakes.fetch_add(1, Ordering::SeqCst);
        let state = self.state.clone();
        Box::pin(async move {
            if state.hang_connect {
                std::future::pending::<()>().await;
            }
            Ok(Box::new(PendingConnection { state }) as Box<dyn WsConnection>)
        })
    }
}

struct PendingConnection {
    state: Arc<PendingState>,
}

impl WsConnection for PendingConnection {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>> {
        Box::pin(async move {
            self.state.sent.lock().unwrap().push(text);
            if self.state.hang_send {
                std::future::pending::<()>().await;
            }
            Ok(())
        })
    }

    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>> {
        Box::pin(std::future::pending())
    }
}

impl Drop for PendingConnection {
    fn drop(&mut self) {
        self.state.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

// -------------------------------------------------------------- §1: the route setting

#[test]
fn the_transport_setting_defaults_to_sse() {
    let settings: ResponsesAdapterSettings =
        serde_json::from_value(json!({ "account": "codex-subscription" }))
            .expect("the key is optional");
    assert_eq!(settings.transport, ResponsesTransport::Sse);

    for (value, expected) in [
        ("sse", ResponsesTransport::Sse),
        ("websocket", ResponsesTransport::Websocket),
    ] {
        let settings: ResponsesAdapterSettings =
            serde_json::from_value(json!({ "account": "codex-subscription", "transport": value }))
                .unwrap_or_else(|error| panic!("{value}: {error}"));
        assert_eq!(settings.transport, expected, "{value}");
    }
}

#[test]
fn an_unknown_transport_is_a_route_file_error() {
    for value in ["quic", "WebSocket", "ws", ""] {
        let error = serde_json::from_value::<ResponsesAdapterSettings>(
            json!({ "account": "codex-subscription", "transport": value }),
        )
        .expect_err("only the two documented values load");
        let message = error.to_string();
        assert!(
            message.contains("sse") && message.contains("websocket"),
            "{value}: the error names the values that exist: {message}"
        );
    }
}

#[test]
fn a_websocket_route_without_a_connector_is_a_composition_error() {
    // At construction, never at the first request: the provider refuses to exist
    // without the connector its route asks for.
    let error = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        None,
    )
    .expect_err("a websocket route needs its connector");
    assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
    for part in ["websocket", "connector", "with_ws_connector"] {
        assert!(error.message.contains(part), "{}: {part}", error.message);
    }

    // And the reverse: a connector belongs to a route that asks for one.
    let error = compose(
        ResponsesTransport::Sse,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(ScriptedWsConnector::new(Vec::new()))),
    )
    .expect_err("an SSE route takes no connector");
    assert!(error.message.contains("SSE"), "{}", error.message);
}

// ------------------------------------------------------------ §3: handshake and frame

#[tokio::test]
async fn the_handshake_url_and_headers_are_exact() {
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(1))],
        ScriptedTransport::new(Vec::new()),
    );
    let options = ModelOptions {
        cache_key: Some(CACHE_KEY.to_string()),
        ..ModelOptions::default()
    };
    let events = collect(
        provider
            .stream(request_with(options), CancellationToken::new())
            .await
            .expect("the request is buildable"),
    )
    .await;
    completed(&events);

    let handshakes = connector.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert_eq!(
        handshakes[0].url,
        "wss://example.test/backend/codex/responses"
    );
    let names: Vec<&str> = handshakes[0]
        .headers
        .iter()
        .map(|(name, _)| name.as_str())
        .collect();
    assert_eq!(
        names,
        [
            "Authorization",
            "chatgpt-account-id",
            "originator",
            "User-Agent",
            "OpenAI-Beta",
            "session-id",
            "x-client-request-id",
        ],
        "the header NAMES, in order (§3)"
    );
    let value = |name: &str| {
        handshakes[0]
            .headers
            .iter()
            .find(|(candidate, _)| candidate == name)
            .map(|(_, value)| value.as_str())
    };
    assert_eq!(
        value("Authorization"),
        Some(&format!("Bearer {BEARER}")[..])
    );
    assert_eq!(value("chatgpt-account-id"), Some(ACCOUNT_ID));
    assert_eq!(value("originator"), Some("p1"));
    assert_eq!(
        value("User-Agent"),
        Some(concat!("p1/", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(
        value("OpenAI-Beta"),
        Some("responses_websockets=2026-02-06"),
        "the WebSocket beta value, not the SSE one"
    );
    assert_eq!(value("session-id"), Some(CACHE_KEY));
    assert_eq!(
        value("x-client-request-id"),
        Some(&format!("p1-{CACHE_KEY}")[..])
    );
    assert!(value("Content-Type").is_none(), "one text frame, no body");
    assert!(
        value("Accept").is_none(),
        "the answer is not an event stream"
    );
}

#[tokio::test]
async fn the_shipped_endpoint_becomes_the_documented_websocket_url() {
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(turn_frames(1))]);
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");
    let provider = provider.with_base_url("https://chatgpt.com/backend-api");
    let events = turn(&provider).await;
    completed(&events);

    assert_eq!(
        connector.handshakes()[0].url,
        "wss://chatgpt.com/backend-api/codex/responses"
    );
}

#[tokio::test]
async fn the_session_headers_need_a_cache_key() {
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(1))],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;
    completed(&events);

    let names: Vec<String> = connector.handshakes()[0]
        .headers
        .iter()
        .map(|(name, _)| name.clone())
        .collect();
    assert_eq!(
        names,
        [
            "Authorization",
            "chatgpt-account-id",
            "originator",
            "User-Agent",
            "OpenAI-Beta",
        ],
        "without a cache key the handshake carries no session identity"
    );
}

#[tokio::test]
async fn the_frame_is_the_sse_body_without_stream_and_background_plus_the_type() {
    // The SSE arm first: the body this route sends over HTTPS.
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let sse_provider =
        compose(ResponsesTransport::Sse, sse.clone(), None).expect("the route composes");
    let events = turn(&sse_provider).await;
    completed(&events);
    let sse_body: Value =
        serde_json::from_slice(&sse.requests()[0].body).expect("the body is JSON");
    assert_eq!(sse_body["stream"], json!(true));

    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(1))],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;
    completed(&events);

    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 1, "one accepted connection");
    assert_eq!(sent[0].len(), 1, "one request is ONE text frame");
    let frame: Value = serde_json::from_str(&sent[0][0]).expect("the frame is JSON");
    let mut expected = sse_body.clone();
    expected.as_object_mut().unwrap().remove("stream");
    expected["type"] = json!("response.create");
    assert_eq!(frame, expected, "the frame is the SSE body, re-shaped");
    assert!(frame.get("stream").is_none());
    assert!(frame.get("background").is_none());
}

#[tokio::test]
async fn the_websocket_events_are_the_sse_events_of_the_same_transcript() {
    for fixture in [
        fixtures::TEXT_TURN,
        fixtures::TOOL_CALL_TURN,
        fixtures::TWO_TOOL_CALLS,
        fixtures::TRUNCATED_TOOL_CALL,
        fixtures::INVALID_TOOL_JSON,
        fixtures::ERROR_EVENT,
        fixtures::NO_USAGE,
        fixtures::REASONING_TURN,
        fixtures::EVENTS_AFTER_TERMINAL,
    ] {
        let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixture)]);
        let sse_provider = compose(ResponsesTransport::Sse, sse, None).expect("the route composes");
        let expected = turn(&sse_provider).await;

        // The SAME transcript as text frames, through the same parser.
        let (provider, _) = websocket_provider(
            vec![ScriptedConnection::accept(text_frames(fixture))],
            ScriptedTransport::new(Vec::new()),
        );
        let events = turn(&provider).await;
        assert_eq!(events, expected, "fixture: {fixture}");
    }
}

#[tokio::test]
async fn previous_response_id_is_never_sent_on_either_transport() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let sse_provider =
        compose(ResponsesTransport::Sse, sse.clone(), None).expect("the route composes");
    let _ = turn(&sse_provider).await;
    let body: Value = serde_json::from_slice(&sse.requests()[0].body).unwrap();
    assert!(body.get("previous_response_id").is_none());

    // Including after a reconnect, which re-sends the FULL body (§5, §6).
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::accept(vec![ScriptedFrame::text(
                r#"{"type":"error","error":{"code":"previous_response_not_found"}}"#,
            )]),
            ScriptedConnection::accept(turn_frames(1)),
        ],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;
    completed(&events);
    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 2, "the reconnect sent the full body again");
    for frame in sent.iter().flatten() {
        let frame: Value = serde_json::from_str(frame).unwrap();
        assert!(
            frame.get("previous_response_id").is_none(),
            "this stage never sends a continuation: {frame}"
        );
        assert_eq!(frame["type"], json!("response.create"));
    }
}

// ---------------------------------------------------------- §4: connection lifetime

#[tokio::test]
async fn a_busy_connection_makes_the_concurrent_request_use_sse() {
    let peer = PendingPeer::stalling_read();
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(peer.clone()),
    )
    .expect("the route composes");

    // The first request owns the slot, with the connection open and its frame sent.
    let mut first = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    poll_once(&mut first).await;
    assert_eq!(peer.handshakes(), 1);
    assert_eq!(peer.sent().len(), 1);

    // A concurrent request is SSE: never a second socket, never a wait.
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(peer.handshakes(), 1, "no second socket");
    assert_eq!(sse.requests().len(), 1, "the concurrent request used SSE");

    drop(first);
    assert_eq!(peer.dropped(), 1, "dropping the stream drops the socket");
}

#[tokio::test]
async fn the_connection_is_reused_across_turns_within_the_bounds() {
    let clock = FakeClock::new();
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(turn_frames(3))]);
    let provider = compose_with(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
        Some(clock.handle()),
        Arc::new(FixedCredentials::default()),
    )
    .expect("the route composes");

    for _ in 0..3 {
        let events = turn(&provider).await;
        completed(&events);
        // 4 minutes idle: inside §4's 5-minute bound.
        clock.advance(Duration::from_secs(4 * 60));
    }
    assert_eq!(
        connector.handshakes().len(),
        1,
        "one connection, three turns"
    );
    assert_eq!(connector.sent_texts().len(), 1);
    assert_eq!(connector.sent_texts()[0].len(), 3);
}

#[tokio::test]
async fn an_idle_connection_is_dropped_and_reconnected() {
    let clock = FakeClock::new();
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(turn_frames(1)),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let provider = compose_with(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
        Some(clock.handle()),
        Arc::new(FixedCredentials::default()),
    )
    .expect("the route composes");

    completed(&turn(&provider).await);
    clock.advance(Duration::from_secs(6 * 60));
    completed(&turn(&provider).await);

    assert_eq!(
        connector.handshakes().len(),
        2,
        "idle for more than 5 minutes: connect anew"
    );
}

#[tokio::test]
async fn a_connection_older_than_55_minutes_is_dropped_and_reconnected() {
    // Four minutes between turns keeps every turn inside the idle bound, so the
    // 15th turn is the first whose connection is older than 55 minutes.
    let clock = FakeClock::new();
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(turn_frames(14)),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let provider = compose_with(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
        Some(clock.handle()),
        Arc::new(FixedCredentials::default()),
    )
    .expect("the route composes");

    for _ in 0..14 {
        completed(&turn(&provider).await);
        clock.advance(Duration::from_secs(4 * 60));
    }
    assert_eq!(connector.handshakes().len(), 1, "14 turns, one connection");
    completed(&turn(&provider).await);
    assert_eq!(
        connector.handshakes().len(),
        2,
        "56 minutes old: connect anew"
    );
}

#[tokio::test]
async fn a_failed_response_never_returns_its_connection_to_the_slot() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(vec![
            ScriptedFrame::text(DELTA),
            ScriptedFrame::error("reset"),
        ]),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    let events = turn(&provider).await;
    assert_eq!(failed(&events).kind, ProviderErrorKind::Transport);

    // WebSocket stays on (no fallback after output), and the next turn opens a new
    // socket rather than reusing the broken one.
    completed(&turn(&provider).await);
    assert_eq!(connector.handshakes().len(), 2);
}

#[tokio::test]
async fn cancelling_a_request_drops_the_connection() {
    let peer = PendingPeer::stalling_read();
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(peer.clone()),
    )
    .expect("the route composes");

    let cancel = CancellationToken::new();
    let mut stream = provider
        .stream(request(), cancel.clone())
        .await
        .expect("the request is buildable");
    poll_once(&mut stream).await;
    assert_eq!(peer.dropped(), 0);

    cancel.cancel();
    let events = collect(stream).await;
    assert!(
        matches!(terminal(&events), Outcome::Cancelled),
        "{events:?}"
    );
    assert_eq!(peer.dropped(), 1, "a cancelled request drops the socket");
}

#[tokio::test]
async fn cancelling_while_connecting_stops_without_a_fallback() {
    let peer = PendingPeer::stalling_connect();
    // No SSE response is scripted: a fallback would panic rather than pass.
    let sse = ScriptedTransport::new(Vec::new());
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(peer.clone()),
    )
    .expect("the route composes");

    let cancel = CancellationToken::new();
    let mut stream = provider
        .stream(request(), cancel.clone())
        .await
        .expect("the request is buildable");
    poll_once(&mut stream).await;
    assert_eq!(peer.handshakes(), 1, "the handshake is in flight");

    cancel.cancel();
    let events = collect(stream).await;
    assert!(
        matches!(terminal(&events), Outcome::Cancelled),
        "{events:?}"
    );
    assert_eq!(
        sse.requests().len(),
        0,
        "a cancelled connect is not a fallback"
    );
}

#[tokio::test]
async fn a_request_cancelled_before_it_starts_never_connects() {
    // The peer panics if it is asked for a connection, so this also proves that no
    // socket is opened for a request that was already cancelled.
    let (provider, connector) = websocket_provider(Vec::new(), ScriptedTransport::new(Vec::new()));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let events = collect(
        provider
            .stream(request(), cancel)
            .await
            .expect("the request is buildable"),
    )
    .await;
    assert!(
        matches!(terminal(&events), Outcome::Cancelled),
        "{events:?}"
    );
    assert_eq!(connector.handshakes().len(), 0);
}

#[tokio::test]
async fn dropping_the_returned_stream_is_a_cancellation() {
    let peer = PendingPeer::stalling_read();
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(peer.clone()),
    )
    .expect("the route composes");

    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    poll_once(&mut stream).await;
    drop(stream);

    assert_eq!(peer.dropped(), 1, "dropping the stream drops the socket");
    // The slot is free again, so the next request opens a NEW connection rather
    // than waiting for the dropped one.
    let mut second = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    poll_once(&mut second).await;
    assert_eq!(peer.handshakes(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_connect_that_never_answers_is_bounded_and_falls_back() {
    let peer = PendingPeer::stalling_connect();
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(peer.clone()),
    )
    .expect("the route composes");

    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(
        sse.requests().len(),
        1,
        "the connect bound fell back to SSE"
    );
}

#[tokio::test(start_paused = true)]
async fn a_send_that_never_answers_is_bounded_and_falls_back() {
    let peer = PendingPeer::stalling_send();
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(peer.clone()),
    )
    .expect("the route composes");

    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(sse.requests().len(), 1, "the send bound fell back to SSE");
}

// ------------------------------------------------------------- §5: the failure policy

#[tokio::test]
async fn an_upgrade_refused_with_401_refreshes_once_and_reconnects() {
    let credentials = Arc::new(FixedCredentials::default());
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::refuse(401, REFUSAL_BODY),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let sse = ScriptedTransport::new(Vec::new());
    let provider = compose_with(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(Arc::new(connector.clone())),
        None,
        credentials.clone(),
    )
    .expect("the route composes");

    let events = turn(&provider).await;
    completed(&events);

    let refreshes = credentials.refresh_calls.lock().unwrap();
    assert_eq!(refreshes.len(), 1, "ONE forced refresh");
    assert_eq!(refreshes[0].bearer, BEARER, "of the rejected credential");
    assert_eq!(connector.handshakes().len(), 2, "reconnect once");
    assert_eq!(
        connector.handshakes()[1].headers[0].1,
        format!("Bearer {REFRESHED}"),
        "the reconnect carries the refreshed credential"
    );
    assert_eq!(sse.requests().len(), 0, "the refresh fixed it: no fallback");
}

#[tokio::test]
async fn a_second_refusal_after_the_refresh_is_authentication() {
    let credentials = Arc::new(FixedCredentials::default());
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::refuse(401, REFUSAL_BODY),
        ScriptedConnection::refuse(403, REFUSAL_BODY),
    ]);
    let sse = ScriptedTransport::new(Vec::new());
    let provider = compose_with(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(Arc::new(connector.clone())),
        None,
        credentials.clone(),
    )
    .expect("the route composes");

    let events = turn(&provider).await;
    assert_eq!(failed(&events).kind, ProviderErrorKind::Authentication);
    assert_eq!(credentials.refresh_calls.lock().unwrap().len(), 1);
    assert_eq!(connector.handshakes().len(), 2, "at most one reconnect");
    assert_eq!(
        sse.requests().len(),
        0,
        "a refused key is not an SSE problem"
    );
}

#[tokio::test]
async fn an_upgrade_refused_with_429_is_rate_limited_without_fallback() {
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::refuse(429, REFUSAL_BODY)],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;
    assert_eq!(failed(&events).kind, ProviderErrorKind::RateLimited);
    assert_eq!(connector.handshakes().len(), 1);
}

#[tokio::test]
async fn an_upgrade_refused_with_another_status_falls_back_to_sse() {
    for status in [400, 404, 500, 503] {
        let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
        let (provider, connector) = websocket_provider(
            vec![ScriptedConnection::refuse(status, REFUSAL_BODY)],
            sse.clone(),
        );
        let events = turn(&provider).await;
        completed(&events);
        assert_eq!(sse.requests().len(), 1, "{status}: fell back to SSE");
        assert_eq!(connector.handshakes().len(), 1, "{status}");
    }
}

#[tokio::test]
async fn a_connect_error_falls_back_to_sse() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let (provider, connector) =
        websocket_provider(vec![ScriptedConnection::fail("dns")], sse.clone());
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(sse.requests().len(), 1);
    assert_eq!(connector.handshakes().len(), 1);
}

#[tokio::test]
async fn a_read_error_before_any_output_falls_back_to_sse() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    // One connection only: a reconnect would panic the scripted peer.
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(vec![ScriptedFrame::error(
            "reset",
        )])],
        sse.clone(),
    );
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(sse.requests().len(), 1);
    assert_eq!(connector.handshakes().len(), 1);
}

#[tokio::test]
async fn a_close_before_any_output_falls_back_to_sse() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let (provider, _) = websocket_provider(
        vec![ScriptedConnection::accept(vec![ScriptedFrame::close()])],
        sse.clone(),
    );
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(sse.requests().len(), 1);
}

#[tokio::test]
async fn a_failure_after_model_visible_output_is_a_transport_failure_with_no_retry_and_no_fallback()
{
    // One connection and NO SSE response are scripted: neither a retry nor a
    // fallback can happen without the test noticing.
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(vec![
            ScriptedFrame::text(DELTA),
            ScriptedFrame::error("reset"),
        ])],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;

    assert_eq!(failed(&events).kind, ProviderErrorKind::Transport);
    assert_eq!(connector.handshakes().len(), 1, "no retry");
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { text, .. } if text == "Hello")),
        "the visible output is preserved: {events:?}"
    );
}

#[tokio::test]
async fn a_reconnectable_error_after_output_is_an_ordinary_failure() {
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(vec![
            ScriptedFrame::text(DELTA),
            ScriptedFrame::text(
                r#"{"type":"error","error":{"code":"previous_response_not_found"}}"#,
            ),
        ])],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;

    assert_eq!(failed(&events).kind, ProviderErrorKind::Transport);
    assert_eq!(connector.handshakes().len(), 1, "no reconnect after output");
}

#[tokio::test]
async fn previous_response_not_found_reconnects_once_and_sends_the_full_body() {
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::accept(vec![ScriptedFrame::text(
                r#"{"type":"error","error":{"code":"previous_response_not_found"}}"#,
            )]),
            ScriptedConnection::accept(turn_frames(1)),
        ],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;
    completed(&events);

    assert_eq!(connector.handshakes().len(), 2, "reconnect once");
    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0], sent[1], "the FULL body goes out again");
}

#[tokio::test]
async fn websocket_connection_limit_reached_reconnects_once() {
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::accept(vec![ScriptedFrame::text(
                r#"{"type":"error","error":{"code":"websocket_connection_limit_reached"}}"#,
            )]),
            ScriptedConnection::accept(turn_frames(1)),
        ],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(connector.handshakes().len(), 2);
}

#[tokio::test]
async fn a_reused_socket_that_closes_before_its_first_frame_reconnects_once() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(turn_frames(1)),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let sse = ScriptedTransport::new(Vec::new());
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    completed(&turn(&provider).await);
    // The first connection's script is exhausted, so the reused socket is gone
    // before the second turn's first frame: §5 reconnects once.
    completed(&turn(&provider).await);

    assert_eq!(connector.handshakes().len(), 2);
    assert_eq!(sse.requests().len(), 0, "the reconnect recovered it");
}

#[tokio::test]
async fn at_most_one_reconnect_per_request() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(turn_frames(1)),
        // The reconnect's socket also closes before its first frame.
        ScriptedConnection::accept(Vec::new()),
    ]);
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    completed(&turn(&provider).await);
    let events = turn(&provider).await;
    completed(&events);

    assert_eq!(connector.handshakes().len(), 2, "one reconnect, no more");
    assert_eq!(
        sse.requests().len(),
        1,
        "the second failure fell back to SSE"
    );
}

#[tokio::test]
async fn another_error_event_is_what_the_existing_parser_makes_of_it() {
    for (code, kind) in [
        ("rate_limit_exceeded", ProviderErrorKind::RateLimited),
        ("invalid_request_error", ProviderErrorKind::InvalidRequest),
        ("token_expired", ProviderErrorKind::Authentication),
        ("weird_thing", ProviderErrorKind::Transport),
    ] {
        let (provider, connector) = websocket_provider(
            vec![ScriptedConnection::accept(vec![ScriptedFrame::text(
                format!(r#"{{"type":"error","error":{{"code":"{code}"}}}}"#),
            )])],
            ScriptedTransport::new(Vec::new()),
        );
        let events = turn(&provider).await;
        assert_eq!(failed(&events).kind, kind, "{code}");
        assert_eq!(connector.handshakes().len(), 1, "{code}: no reconnect");
    }
}

#[tokio::test]
async fn fallback_turns_websocket_off_for_this_provider_instance() {
    let sse = ScriptedTransport::new(vec![
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
    ]);
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::refuse(500, REFUSAL_BODY)],
        sse.clone(),
    );

    completed(&turn(&provider).await);
    completed(&turn(&provider).await);

    assert_eq!(
        connector.handshakes().len(),
        1,
        "no connect after a fallback"
    );
    assert_eq!(sse.requests().len(), 2, "both requests used SSE");
}

// ----------------------------------------------------------------- no leaked values

#[tokio::test]
async fn no_debug_or_error_text_carries_a_header_value() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::refuse(401, REFUSAL_BODY),
        ScriptedConnection::refuse(401, REFUSAL_BODY),
    ]);
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    let options = ModelOptions {
        cache_key: Some(CACHE_KEY.to_string()),
        ..ModelOptions::default()
    };
    let events = collect(
        provider
            .stream(request_with(options), CancellationToken::new())
            .await
            .expect("the request is buildable"),
    )
    .await;
    let error = failed(&events);
    assert_eq!(error.kind, ProviderErrorKind::Authentication);

    for text in [
        format!("{provider:?}"),
        format!("{events:?}"),
        format!("{error:?}"),
        error.to_string(),
        format!("{:?}", connector.handshakes()),
    ] {
        for sentinel in [BEARER, REFRESHED, ACCOUNT_ID, CACHE_KEY, "SENTINEL-WS-BODY"] {
            assert!(!text.contains(sentinel), "{sentinel} leaked into: {text}");
        }
    }
}
