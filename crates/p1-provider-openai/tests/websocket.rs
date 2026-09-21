//! The WebSocket transport of the Responses adapter (ADR-0047,
//! `docs/design/websocket.md` §1, §3, §4, §5, §6), entirely offline: a scripted
//! peer, a scripted HTTP transport for the fallback arm, and an injected clock. No
//! test here touches the network or a credential file.
//!
//! Every row of §5's table is a named test, §3's handshake and frame are pinned
//! byte for byte, §4's lifetime rules (reuse, busy, cancellation, the slot) each
//! have their own, and §6's continuation — its one success shape, every rule that
//! turns it back into a FULL body, and the memory that goes with a dropped
//! connection — is the last section. The SSE arm of a WebSocket provider is a
//! [`ScriptedTransport`] with NO scripted response wherever a fallback must NOT
//! happen: asking for one panics, so "no fallback" is asserted, not assumed.

mod fixtures;

use std::collections::VecDeque;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use futures_util::future::{Either, select};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, DeclarationKind, Effort, Item,
    ModelOptions, Origin, Outcome, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    ProviderStream, ReplayData, StreamEvent, ToolCall, ToolDeclaration, ToolInput,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{
    ScriptedConnection, ScriptedFrame, ScriptedResponse, ScriptedTransport, ScriptedWsConnector,
};
use p1_provider_http::ws::{WsConnectError, WsConnection, WsConnector, WsError, WsHandshake};
use p1_provider_http::{Credential, CredentialSource, RetryPolicy};
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

/// `turns` complete turns of one transcript, for a connection that serves several.
fn turns_of(fixture: &str, turns: usize) -> Vec<ScriptedFrame> {
    let mut frames = Vec::new();
    for _ in 0..turns {
        frames.extend(text_frames(fixture));
    }
    frames
}

/// `turns` complete turns of the "no usage" transcript (response id `resp_no_usage`,
/// one assistant message).
fn turn_frames(turns: usize) -> Vec<ScriptedFrame> {
    turns_of(fixtures::NO_USAGE, turns)
}

/// `turns` complete turns of the tool-call transcript (response id `resp_tool`: an
/// assistant message "I will read it." and the call `call_1`).
fn tool_turn_frames(turns: usize) -> Vec<ScriptedFrame> {
    turns_of(fixtures::TOOL_CALL_TURN, turns)
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
    turn_of(provider, request()).await
}

/// One whole turn for a request this test builds itself (the §6 tests run two turns
/// of the SAME conversation through one provider).
async fn turn_of(provider: &OpenAiCodexProvider, request: ProviderRequest) -> Vec<StreamEvent> {
    collect(
        provider
            .stream(request, CancellationToken::new())
            .await
            .expect("the request is buildable"),
    )
    .await
}

/// A request with this history, the system prompt and no options: the shape every
/// §6 test varies by history alone.
fn history_request(history: Vec<Item>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history,
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_string(),
    }
}

/// The assistant item the NEXT request's history holds: the previous response as the
/// core would have recorded it.
fn assistant(blocks: Vec<AssistantBlock>) -> Item {
    Item::Assistant(AssistantItem {
        origin: Origin {
            route: ROUTE.to_string(),
            model: MODEL.to_string(),
        },
        blocks,
    })
}

fn text(text: &str) -> AssistantBlock {
    AssistantBlock::Text {
        text: text.to_string(),
    }
}

fn call(call_id: &str, name: &str, arguments: &str) -> AssistantBlock {
    AssistantBlock::ToolCall(ToolCall {
        call_id: call_id.to_string(),
        name: name.to_string(),
        input: ToolInput::Json(arguments.to_string()),
    })
}

/// One `input` item as `build_request` writes it.
fn wire_message(role: &str, text: &str) -> Value {
    let content_type = if role == "assistant" {
        "output_text"
    } else {
        "input_text"
    };
    json!({
        "type": "message",
        "role": role,
        "content": [{ "type": content_type, "text": text }],
    })
}

/// The `index`-th text frame the `connection`-th accepted connection sent.
fn frame(sent: &[Vec<String>], connection: usize, index: usize) -> Value {
    serde_json::from_str(&sent[connection][index]).expect("a frame is JSON")
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

/// A peer whose connections replay their frames and then go SILENT — a read that
/// never answers — so a test can cancel a turn in the middle of its read. The
/// scripted connector cannot do that: an exhausted script is a close.
struct StallAfterFrames {
    state: Arc<Mutex<StallState>>,
}

#[derive(Default)]
struct StallState {
    /// One script per accepted connection, consumed in connect order.
    scripts: VecDeque<Vec<ScriptedFrame>>,
    /// Texts sent, per accepted connection.
    sent: Vec<Vec<String>>,
    dropped: usize,
}

impl StallAfterFrames {
    fn new(scripts: Vec<Vec<ScriptedFrame>>) -> Arc<Self> {
        Arc::new(Self {
            state: Arc::new(Mutex::new(StallState {
                scripts: scripts.into(),
                ..StallState::default()
            })),
        })
    }

    fn sent(&self) -> Vec<Vec<String>> {
        self.state.lock().unwrap().sent.clone()
    }

    fn dropped(&self) -> usize {
        self.state.lock().unwrap().dropped
    }
}

impl WsConnector for StallAfterFrames {
    fn connect<'a>(
        &'a self,
        _request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>> {
        let (index, frames) = {
            let mut state = self.state.lock().unwrap();
            state.sent.push(Vec::new());
            let frames = state
                .scripts
                .pop_front()
                .expect("StallAfterFrames: one script per connect");
            (state.sent.len() - 1, frames)
        };
        let state = self.state.clone();
        Box::pin(async move {
            Ok(Box::new(StalledConnection {
                state,
                index,
                frames: frames.into(),
            }) as Box<dyn WsConnection>)
        })
    }
}

struct StalledConnection {
    state: Arc<Mutex<StallState>>,
    index: usize,
    frames: VecDeque<ScriptedFrame>,
}

impl WsConnection for StalledConnection {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>> {
        Box::pin(async move {
            self.state.lock().unwrap().sent[self.index].push(text);
            Ok(())
        })
    }

    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>> {
        match self.frames.pop_front() {
            Some(ScriptedFrame::Text(text)) => Box::pin(async move { Ok(Some(text)) }),
            Some(ScriptedFrame::Error(message)) => Box::pin(async move { Err(WsError(message)) }),
            // Past its script this connection never answers again.
            Some(ScriptedFrame::Close) | None => Box::pin(std::future::pending()),
        }
    }
}

impl Drop for StalledConnection {
    fn drop(&mut self) {
        self.state.lock().unwrap().dropped += 1;
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
        peer.handshakes(),
        4,
        "a connect timeout is the transient row: the first attempt and the three \
         reconnects the default policy's max_retries allows"
    );
    assert_eq!(
        sse.requests().len(),
        1,
        "the retry budget spent, the connect bound fell back to SSE"
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
    assert_eq!(
        peer.handshakes(),
        4,
        "a send bound is the transient row: the first attempt and the three reconnects"
    );
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

#[tokio::test(start_paused = true)]
async fn an_upgrade_refused_with_another_status_falls_back_to_sse() {
    for status in [400, 404, 500, 503] {
        let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
        let (provider, connector) = websocket_provider(
            vec![ScriptedConnection::refuse(status, REFUSAL_BODY)],
            sse.clone(),
        );
        // ONE scripted connection only: a reconnect would panic the peer's script.
        let start = tokio::time::Instant::now();
        let events = turn(&provider).await;
        completed(&events);
        assert_eq!(sse.requests().len(), 1, "{status}: fell back to SSE");
        assert_eq!(connector.handshakes().len(), 1, "{status}: no reconnect");
        assert_eq!(
            start.elapsed(),
            Duration::ZERO,
            "{status}: the endpoint says no — SSE is next, without even a backoff wait"
        );
    }
}

/// §5's transient row: a connect error reconnects with the FULL body for the retry
/// policy's `max_retries` (3 by default), waiting the policy's backoff between
/// attempts; the budget spent, the SSE path serves the request.
#[tokio::test(start_paused = true)]
async fn a_connect_error_retries_within_the_budget_then_falls_back_to_sse() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    // One scripted connection per attempt: the first and the three `max_retries`
    // allows. A fourth reconnect would panic the peer's script.
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::fail("dns"),
            ScriptedConnection::fail("dns"),
            ScriptedConnection::fail("dns"),
            ScriptedConnection::fail("dns"),
        ],
        sse.clone(),
    );
    let start = tokio::time::Instant::now();
    let events = turn(&provider).await;
    completed(&events);

    assert_eq!(
        connector.handshakes().len(),
        4,
        "the first attempt and max_retries (3) reconnects"
    );
    assert_eq!(sse.requests().len(), 1, "the budget spent, SSE serves it");
    let policy = RetryPolicy::default();
    assert_eq!(
        start.elapsed(),
        policy.delay(1, None) + policy.delay(2, None) + policy.delay(3, None),
        "the waits are the retry policy's backoff on the paused clock, not real sleeps"
    );
}

/// §5's transient row, and its happy shape: ONE connect error, then the reconnect
/// succeeds over WebSocket — no fallback — and the frame that goes out again is the
/// FULL body (§6: `previous_response_id` is scoped to the connection).
#[tokio::test(start_paused = true)]
async fn a_transient_connect_error_reconnects_and_the_retry_succeeds() {
    let sse = ScriptedTransport::new(Vec::new());
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::fail("dns"),
            ScriptedConnection::accept(text_frames(fixtures::TEXT_TURN)),
        ],
        sse.clone(),
    );
    let start = tokio::time::Instant::now();
    let events = turn(&provider).await;
    completed(&events);

    assert_eq!(connector.handshakes().len(), 2, "one reconnect");
    assert_eq!(
        sse.requests().len(),
        0,
        "the retry recovered it: no fallback"
    );
    let policy = RetryPolicy::default();
    assert_eq!(
        start.elapsed(),
        policy.delay(1, None),
        "the wait is the first backoff of the retry policy on the paused clock"
    );
    // Stream rule 4: the back-off yields `Activity`, so the consumer sees life
    // before the first content event of the retry — exactly as `drive` does.
    let first_activity = events
        .iter()
        .position(|event| matches!(event, StreamEvent::Activity))
        .expect("a back-off activity event");
    let first_content = events
        .iter()
        .position(|event| matches!(event, StreamEvent::TextDelta { .. }))
        .expect("the retry's content");
    assert!(first_activity < first_content, "{events:?}");
    // The failed attempt sent nothing, so the only texts are the retry's one frame.
    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 1, "only the accepted connection was written to");
    assert!(
        frame(&sent, 0, 0).get("previous_response_id").is_none(),
        "the FULL body goes out again"
    );
}

/// §5's transient row for a read that fails before any output: retry inside the
/// budget, then SSE.
#[tokio::test(start_paused = true)]
async fn a_read_error_before_any_output_retries_within_the_budget_then_falls_back_to_sse() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::accept(vec![ScriptedFrame::error("reset")]),
            ScriptedConnection::accept(vec![ScriptedFrame::error("reset")]),
            ScriptedConnection::accept(vec![ScriptedFrame::error("reset")]),
            ScriptedConnection::accept(vec![ScriptedFrame::error("reset")]),
        ],
        sse.clone(),
    );
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(connector.handshakes().len(), 4);
    assert_eq!(sse.requests().len(), 1, "the budget spent, SSE serves it");
}

/// The same for a close before any output — and on a FRESH connection, where §5 has
/// no "once" row: a reused socket closing before its first frame is the once row
/// (its own test), every later close is the transient row.
#[tokio::test(start_paused = true)]
async fn a_close_before_any_output_retries_within_the_budget_then_falls_back_to_sse() {
    let sse = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::accept(vec![ScriptedFrame::close()]),
            ScriptedConnection::accept(vec![ScriptedFrame::close()]),
            ScriptedConnection::accept(vec![ScriptedFrame::close()]),
            ScriptedConnection::accept(vec![ScriptedFrame::close()]),
        ],
        sse.clone(),
    );
    let events = turn(&provider).await;
    completed(&events);
    assert_eq!(connector.handshakes().len(), 4);
    assert_eq!(sse.requests().len(), 1, "the budget spent, SSE serves it");
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

/// §5 budgets reconnects per ROW: the reused socket that closes before its first
/// frame spends its one "once" reconnect, and the FRESH sockets that follow it —
/// no "once" row covers those — are the transient row, three of them, before the
/// budget is spent and SSE takes over.
#[tokio::test(start_paused = true)]
async fn a_reused_close_spends_its_once_row_and_the_fresh_ones_the_transient_budget() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(turn_frames(1)),
        // The reconnect the "once" row buys.
        ScriptedConnection::accept(Vec::new()),
        // The three the transient row buys.
        ScriptedConnection::accept(Vec::new()),
        ScriptedConnection::accept(Vec::new()),
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
    // The first connection's script is exhausted, so the reused socket is gone
    // before the second turn's first frame: §5 reconnects once for it, and each
    // fresh socket after that closes too.
    let events = turn(&provider).await;
    completed(&events);

    assert_eq!(
        connector.handshakes().len(),
        5,
        "the first connection, the once row and max_retries (3)"
    );
    assert_eq!(
        sse.requests().len(),
        1,
        "the transient budget spent, SSE serves the request"
    );
}

/// §5's "once" rows stay once: the SAME error event on the reconnected socket is
/// not another reconnect — that row's allowance is spent, so the event is exactly
/// what the existing parser makes of it (and no fallback happens: this is a
/// response-level failure now).
#[tokio::test]
async fn a_connection_error_row_reconnects_once_and_the_second_time_is_the_parsers() {
    let code = "previous_response_not_found";
    let (provider, connector) = websocket_provider(
        vec![
            ScriptedConnection::accept(vec![ScriptedFrame::text(format!(
                r#"{{"type":"error","error":{{"code":"{code}"}}}}"#
            ))]),
            ScriptedConnection::accept(vec![ScriptedFrame::text(format!(
                r#"{{"type":"error","error":{{"code":"{code}"}}}}"#
            ))]),
        ],
        ScriptedTransport::new(Vec::new()),
    );
    let events = turn(&provider).await;

    assert_eq!(
        failed(&events).kind,
        ProviderErrorKind::Transport,
        "{events:?}"
    );
    assert_eq!(
        connector.handshakes().len(),
        2,
        "the row's ONE reconnect, and no third connection"
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

// ------------------------------------------------------------------ §6: continuation
//
// `NO_USAGE` is one complete turn: response id `resp_no_usage`, one output item —
// the assistant message "ok". Two turns of a conversation therefore look like: the
// first request carries `[user "hi"]`, and the second one the SAME item, the
// assistant message the response completed, and one new user item.

/// The second turn of that conversation, exactly as the next request encodes the
/// first response's output item.
fn continued() -> Vec<Item> {
    vec![user("hi"), assistant(vec![text("ok")]), user("again")]
}

#[tokio::test]
async fn a_second_turn_on_one_connection_sends_only_the_new_items() {
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(2))],
        ScriptedTransport::new(Vec::new()),
    );

    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    completed(&turn_of(&provider, history_request(continued())).await);

    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 1, "one connection carried both turns");
    assert_eq!(sent[0].len(), 2, "one frame per turn");
    assert_eq!(
        connector.handshakes().len(),
        1,
        "a continuation is not a new connection"
    );

    let first = frame(&sent, 0, 0);
    assert!(first.get("previous_response_id").is_none(), "{first}");
    assert_eq!(first["input"], json!([wire_message("user", "hi")]));

    let second = frame(&sent, 0, 1);
    assert_eq!(
        second["type"],
        json!("response.create"),
        "a continuation is still a response.create"
    );
    assert_eq!(
        second["previous_response_id"],
        json!("resp_no_usage"),
        "the id of the response this input continues (§6 rule 1)"
    );
    assert_eq!(
        second["input"],
        json!([wire_message("user", "again")]),
        "ONLY the items after the echoed output item"
    );
    for field in ["model", "instructions", "text"] {
        assert_eq!(
            second[field], first[field],
            "{field} is the full body's, unchanged (§6 rule 2)"
        );
    }
    assert!(second.get("stream").is_none() && second.get("background").is_none());
}

#[tokio::test]
async fn a_changed_top_level_field_sends_the_full_body() {
    // One variation of the second request (a plain fn pointer so the table has one
    // type).
    type Change = fn(&mut ProviderRequest);
    // (a) of §6's rule 2: anything but `input` differing — here the instructions,
    // and a field that APPEARS (tools) where the remembered body had none.
    let changes: [(&str, Change); 2] = [
        ("instructions", |request| {
            request.system_prompt = "SYS-CHANGED".to_string();
        }),
        ("tools", |request| {
            request.tools = vec![ToolDeclaration {
                name: "read".to_string(),
                description: "Read a file".to_string(),
                kind: DeclarationKind::Function {
                    input_schema: json!({ "type": "object" }),
                },
            }];
        }),
    ];
    for (label, change) in changes {
        let (provider, connector) = websocket_provider(
            vec![ScriptedConnection::accept(turn_frames(2))],
            ScriptedTransport::new(Vec::new()),
        );
        completed(&turn_of(&provider, history_request(vec![user("hi")])).await);

        let mut second = history_request(continued());
        change(&mut second);
        completed(&turn_of(&provider, second).await);

        let sent = connector.sent_texts();
        assert_eq!(sent[0].len(), 2, "{label}: one frame, no reconnect");
        let second = frame(&sent, 0, 1);
        assert!(
            second.get("previous_response_id").is_none(),
            "{label}: {second}"
        );
        assert_eq!(
            second["input"].as_array().unwrap().len(),
            3,
            "{label}: the WHOLE context goes out"
        );
    }
}

#[tokio::test]
async fn a_context_replacement_sends_the_full_body() {
    // (b): the new input does not start with the remembered one — a different first
    // user item, so rule 3 fails by construction and the full body goes out.
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(2))],
        ScriptedTransport::new(Vec::new()),
    );
    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    completed(
        &turn_of(
            &provider,
            history_request(vec![
                user("a different start"),
                assistant(vec![text("ok")]),
                user("again"),
            ]),
        )
        .await,
    );

    let sent = connector.sent_texts();
    let second = frame(&sent, 0, 1);
    assert!(
        second.get("previous_response_id").is_none(),
        "a replaced context is never continued: {second}"
    );
    assert_eq!(
        second["input"][0],
        wire_message("user", "a different start")
    );
    assert_eq!(second["input"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn an_input_with_nothing_after_the_echo_sends_the_full_body() {
    // (c): the history ends where the response did, so there is no NEW item — §6
    // requires at least one more.
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(2))],
        ScriptedTransport::new(Vec::new()),
    );
    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    completed(
        &turn_of(
            &provider,
            history_request(vec![user("hi"), assistant(vec![text("ok")])]),
        )
        .await,
    );

    let sent = connector.sent_texts();
    let second = frame(&sent, 0, 1);
    assert!(
        second.get("previous_response_id").is_none(),
        "nothing new to send: the FULL body: {second}"
    );
    assert_eq!(second["input"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn echoed_items_that_do_not_match_the_response_send_the_full_body() {
    // (d): the items after the remembered input are not the output items the stream
    // reported. `TOOL_CALL_TURN` completes an assistant message "I will read it."
    // and the call `call_1`; each history below echoes something else, and the third
    // element is the size of the FULL input it must send instead.
    type History = fn() -> Vec<Item>;
    let mismatches: [(&str, History, usize); 3] = [
        (
            "a different call id",
            || {
                vec![
                    user("hi"),
                    assistant(vec![
                        text("I will read it."),
                        call("call_other", "read", "{}"),
                    ]),
                    user("again"),
                ]
            },
            4,
        ),
        (
            "the echoed message dropped",
            || {
                vec![
                    user("hi"),
                    assistant(vec![call("call_1", "read", r#"{"path":"a.txt"}"#)]),
                    user("again"),
                ]
            },
            3,
        ),
        (
            "the echoed items swapped",
            || {
                vec![
                    user("hi"),
                    assistant(vec![
                        call("call_1", "read", r#"{"path":"a.txt"}"#),
                        text("I will read it."),
                    ]),
                    user("again"),
                ]
            },
            4,
        ),
    ];
    for (label, history, input_items) in mismatches {
        let (provider, connector) = websocket_provider(
            vec![ScriptedConnection::accept(tool_turn_frames(2))],
            ScriptedTransport::new(Vec::new()),
        );
        completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
        completed(&turn_of(&provider, history_request(history())).await);

        let sent = connector.sent_texts();
        let second = frame(&sent, 0, 1);
        assert!(
            second.get("previous_response_id").is_none(),
            "{label}: {second}"
        );
        assert_eq!(
            second["input"].as_array().unwrap().len(),
            input_items,
            "{label}: the whole context"
        );
    }

    // The control: the SAME two turns with the response's own items echoed do
    // continue, so none of the rejections above is vacuous.
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(tool_turn_frames(2))],
        ScriptedTransport::new(Vec::new()),
    );
    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    completed(
        &turn_of(
            &provider,
            history_request(vec![
                user("hi"),
                assistant(vec![
                    text("I will read it."),
                    call("call_1", "read", r#"{"path":"a.txt"}"#),
                ]),
                user("again"),
            ]),
        )
        .await,
    );
    let second = frame(&connector.sent_texts(), 0, 1);
    assert_eq!(second["previous_response_id"], json!("resp_tool"));
    assert_eq!(second["input"], json!([wire_message("user", "again")]));
}

#[tokio::test]
async fn a_reasoning_item_is_echoed_through_its_replay_payload() {
    // The GPT route replays reasoning as `{type: reasoning, encrypted_content,
    // summary: []}`, which is exactly the item rule 3 has to recognise in the echo.
    let connector = ScriptedWsConnector::new(vec![ScriptedConnection::accept(turns_of(
        fixtures::REASONING_TURN,
        2,
    ))]);
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    let history = || {
        vec![
            user("hi"),
            assistant(vec![
                AssistantBlock::Reasoning {
                    text: "first part\n\nsecond part".to_string(),
                    replay: Some(ReplayData {
                        origin: Origin {
                            route: ROUTE.to_string(),
                            model: MODEL.to_string(),
                        },
                        version: 1,
                        payload: json!({
                            "type": "reasoning",
                            "encrypted_content": "enc-1",
                        }),
                    }),
                },
                text("answer"),
            ]),
            user("again"),
        ]
    };
    // An effort level on BOTH turns: it is what puts `reasoning` and `include` in the
    // body, so rule 2 holds and the echo below is the only thing being tested.
    let with_effort = |history| {
        let mut request = history_request(history);
        request.options.reasoning_effort = Some(Effort::Low);
        request
    };
    completed(&turn_of(&provider, with_effort(vec![user("hi")])).await);
    completed(&turn_of(&provider, with_effort(history())).await);

    let sent = connector.sent_texts();
    let second = frame(&sent, 0, 1);
    assert_eq!(
        second["previous_response_id"],
        json!("resp_reasoning"),
        "the reasoning item and the message after it are the echo, both of them"
    );
    assert_eq!(second["input"], json!([wire_message("user", "again")]));
    assert_eq!(
        second["include"],
        json!(["reasoning.encrypted_content"]),
        "the full body's own fields are untouched"
    );
}

#[tokio::test]
async fn an_idle_connection_never_continues() {
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

    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    clock.advance(Duration::from_secs(6 * 60));
    completed(&turn_of(&provider, history_request(continued())).await);

    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 2, "past the idle bound: connect anew");
    let first_of_new = frame(&sent, 1, 0);
    assert!(
        first_of_new.get("previous_response_id").is_none(),
        "a new connection remembers nothing: {first_of_new}"
    );
    assert_eq!(
        first_of_new["input"].as_array().unwrap().len(),
        3,
        "the FULL body"
    );
}

#[tokio::test]
async fn a_connection_past_its_max_age_never_continues() {
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
        completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
        clock.advance(Duration::from_secs(4 * 60));
    }
    assert_eq!(connector.handshakes().len(), 1, "14 turns, one connection");
    completed(&turn_of(&provider, history_request(continued())).await);

    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 2, "56 minutes old: connect anew");
    let first_of_new = frame(&sent, 1, 0);
    assert!(
        first_of_new.get("previous_response_id").is_none(),
        "the expired connection's memory died with it: {first_of_new}"
    );
    assert_eq!(first_of_new["input"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn a_failed_turn_clears_the_continuation() {
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(
            turn_frames(1)
                .into_iter()
                .chain([ScriptedFrame::text(DELTA), ScriptedFrame::error("reset")])
                .collect(),
        ),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    // The second turn IS a continuation, and its response fails after output: §4
    // drops the connection, so §6's memory goes with it.
    let events = turn_of(&provider, history_request(continued())).await;
    assert_eq!(failed(&events).kind, ProviderErrorKind::Transport);

    // The retry of that turn opens a NEW connection and sends the FULL body.
    completed(&turn_of(&provider, history_request(continued())).await);

    let sent = connector.sent_texts();
    assert_eq!(sent.len(), 2, "the failed response dropped its socket");
    assert_eq!(
        frame(&sent, 0, 1)["previous_response_id"],
        json!("resp_no_usage"),
        "the failed turn had been a continuation"
    );
    let retry = frame(&sent, 1, 0);
    assert!(
        retry.get("previous_response_id").is_none(),
        "a fresh connection starts from the FULL body: {retry}"
    );
    assert_eq!(retry["input"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn a_cancelled_turn_clears_the_continuation() {
    let peer = StallAfterFrames::new(vec![turn_frames(1), turn_frames(1)]);
    let connector: Arc<dyn WsConnector> = peer.clone();
    let provider = compose(
        ResponsesTransport::Websocket,
        ScriptedTransport::new(Vec::new()),
        Some(connector),
    )
    .expect("the route composes");

    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);

    // The second turn is a continuation whose read never answers; cancelling it
    // drops the socket (§4) and clears the continuation (§6).
    let cancel = CancellationToken::new();
    let mut stream = provider
        .stream(history_request(continued()), cancel.clone())
        .await
        .expect("the request is buildable");
    poll_once(&mut stream).await;
    cancel.cancel();
    let events = collect(stream).await;
    assert!(
        matches!(terminal(&events), Outcome::Cancelled),
        "{events:?}"
    );
    assert_eq!(peer.dropped(), 1, "the cancelled turn dropped its socket");

    completed(&turn_of(&provider, history_request(continued())).await);

    let sent = peer.sent();
    assert_eq!(sent.len(), 2);
    assert_eq!(
        frame(&sent, 0, 1)["previous_response_id"],
        json!("resp_no_usage"),
        "the cancelled turn had been a continuation"
    );
    let after = frame(&sent, 1, 0);
    assert!(
        after.get("previous_response_id").is_none(),
        "the cancelled turn's memory died with its socket: {after}"
    );
    assert_eq!(after["input"].as_array().unwrap().len(), 3);
}

#[tokio::test(start_paused = true)]
async fn a_fallback_to_sse_clears_the_continuation() {
    // The first connection answers turn 1 and then closes, so the reused socket is
    // gone before turn 2's first frame (§5's one "once" reconnect); every fresh
    // socket after that closes too, so the transient row's budget is spent and the
    // last row of §5's table falls back to SSE — which drops all of them.
    let sse = ScriptedTransport::new(vec![
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
    ]);
    let connector = ScriptedWsConnector::new(vec![
        ScriptedConnection::accept(
            turn_frames(1)
                .into_iter()
                .chain([ScriptedFrame::close()])
                .collect(),
        ),
        ScriptedConnection::accept(vec![ScriptedFrame::close()]),
        ScriptedConnection::accept(vec![ScriptedFrame::close()]),
        ScriptedConnection::accept(vec![ScriptedFrame::close()]),
        ScriptedConnection::accept(vec![ScriptedFrame::close()]),
    ]);
    let provider = compose(
        ResponsesTransport::Websocket,
        sse.clone(),
        Some(Arc::new(connector.clone())),
    )
    .expect("the route composes");

    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    completed(&turn_of(&provider, history_request(continued())).await);

    let sent = connector.sent_texts();
    assert_eq!(
        frame(&sent, 0, 1)["previous_response_id"],
        json!("resp_no_usage"),
        "turn 2 started as a continuation"
    );
    let resent = frame(&sent, 1, 0);
    assert!(
        resent.get("previous_response_id").is_none(),
        "the reconnect after the close sends the FULL body: {resent}"
    );

    // The SSE request that actually produced the turn, and the next turn (WebSocket
    // is off for this instance now), both carry the whole context and no id.
    assert_eq!(sse.requests().len(), 1, "the fallback ran the turn");
    completed(&turn_of(&provider, history_request(continued())).await);
    let requests = sse.requests();
    assert_eq!(requests.len(), 2);
    for (index, request) in requests.iter().enumerate() {
        let body: Value = serde_json::from_slice(&request.body).expect("the body is JSON");
        assert!(
            body.get("previous_response_id").is_none(),
            "request {index}: {body}"
        );
        assert_eq!(
            body["input"].as_array().unwrap().len(),
            3,
            "request {index}"
        );
    }
}

#[tokio::test]
async fn the_connection_error_rows_on_a_continuation_reconnect_and_resend_the_full_body_once() {
    for code in [
        "previous_response_not_found",
        "websocket_connection_limit_reached",
    ] {
        let (provider, connector) = websocket_provider(
            vec![
                ScriptedConnection::accept(
                    turn_frames(1)
                        .into_iter()
                        .chain([ScriptedFrame::text(format!(
                            r#"{{"type":"error","error":{{"code":"{code}"}}}}"#
                        ))])
                        .collect(),
                ),
                ScriptedConnection::accept(turn_frames(1)),
            ],
            ScriptedTransport::new(Vec::new()),
        );
        completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
        completed(&turn_of(&provider, history_request(continued())).await);

        assert_eq!(connector.handshakes().len(), 2, "{code}: reconnect once");
        let sent = connector.sent_texts();
        assert_eq!(sent.len(), 2, "{code}");
        assert_eq!(
            frame(&sent, 0, 1)["previous_response_id"],
            json!("resp_no_usage"),
            "{code}: the frame that got the error was a continuation"
        );
        let resent = frame(&sent, 1, 0);
        assert!(
            resent.get("previous_response_id").is_none(),
            "{code}: the FULL body goes out again: {resent}"
        );
        assert_eq!(resent["input"].as_array().unwrap().len(), 3, "{code}");
        // The resend succeeded: `completed` above pinned the terminal event.
    }
}

#[tokio::test]
async fn the_sse_arm_never_continues_across_turns() {
    // The frozen assertion: whatever the history looks like, the SSE path sends the
    // whole context and no `previous_response_id`.
    let sse = ScriptedTransport::new(vec![
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
    ]);
    let provider = compose(ResponsesTransport::Sse, sse.clone(), None).expect("the route composes");
    completed(&turn_of(&provider, history_request(vec![user("hi")])).await);
    completed(&turn_of(&provider, history_request(continued())).await);

    let requests = sse.requests();
    assert_eq!(requests.len(), 2);
    for (index, expected_items) in [(0, 1), (1, 3)] {
        let body: Value = serde_json::from_slice(&requests[index].body).expect("the body is JSON");
        assert!(
            body.get("previous_response_id").is_none(),
            "request {index}: {body}"
        );
        assert_eq!(
            body["input"].as_array().unwrap().len(),
            expected_items,
            "request {index}: the SSE body always carries the whole context"
        );
    }
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

#[tokio::test]
async fn no_debug_output_carries_remembered_message_content() {
    let (provider, connector) = websocket_provider(
        vec![ScriptedConnection::accept(turn_frames(2))],
        ScriptedTransport::new(Vec::new()),
    );
    let mut first = history_request(vec![user("hi")]);
    first.system_prompt = "SENTINEL-INSTRUCTIONS".to_string();
    completed(&turn_of(&provider, first).await);
    let mut second = history_request(vec![
        user("hi"),
        assistant(vec![text("ok")]),
        user("SENTINEL-NEW-ITEM"),
    ]);
    second.system_prompt = "SENTINEL-INSTRUCTIONS".to_string();
    let events = turn_of(&provider, second).await;
    completed(&events);

    // The continuation happened, so the connection is holding the remembered body.
    assert_eq!(
        frame(&connector.sent_texts(), 0, 1)["previous_response_id"],
        json!("resp_no_usage")
    );
    for text in [format!("{provider:?}"), format!("{events:?}")] {
        for sentinel in ["SENTINEL-INSTRUCTIONS", "SENTINEL-NEW-ITEM"] {
            assert!(!text.contains(sentinel), "{sentinel} leaked into: {text}");
        }
    }
}
