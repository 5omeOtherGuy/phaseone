//! S7.10-R5 (issue #394): the Responses route's WebSocket transport through the provider
//! component `p1/provider-openai` and the broker's WebSocket driver (ADR-0078 §1–§2), held
//! against the native adapter it replaces.
//!
//! Every case runs one script twice: once through the native `OpenAiCodexProvider` the host
//! built for this route until now, and once through `WasmProvider` over the built component,
//! each with its own scripted peer and scripted HTTP transport. The two must produce the same
//! stream events and put the same bytes on the wire: the same handshake URL and headers, the
//! same frames — full, continuation or after a reconnect — and, where the request falls back,
//! the same HTTP request. The one difference the parity allows is the credential header's
//! spelling on the HTTP fallback (`authorization` from the broker, `Authorization` from the
//! native header builder), so HTTP headers are compared by lowercased name.
//!
//! Fake time (`start_paused`), `ScriptedWsConnector` and `ScriptedTransport` only: no socket,
//! no network, no sleep.

// The native adapters' route values, composed from a route file exactly as the host's tests
// compose them (S7.10-R4 took them out of `p1_host::catalog`).
#[path = "../../p1-host/tests/native_routes/mod.rs"]
mod native_routes;

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use futures_util::StreamExt;
use native_routes::responses_route;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, Item, ModelOptions, Origin,
    Outcome, Provider, ProviderError, ProviderErrorKind, ProviderRequest, ProviderStream,
    StreamEvent,
};
use p1_host::routes::{RouteFile, load_route};
use p1_model_profile::ModelProfile;
use p1_module_runtime::{ExecutionLimits, LoadedModule, ProviderSettings, WasmProvider};
use p1_module_tests::{Release, fixture_dir};
use p1_provider_conformance::fixtures::responses as fixtures;
use p1_provider_http::testing::{
    ScriptedConnection, ScriptedFrame, ScriptedResponse, ScriptedTransport, ScriptedWsConnector,
};
use p1_provider_http::ws_session::Clock;
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::OpenAiCodexProvider;

const OPENAI: &str = "p1/provider-openai";
const PACKAGE: &str = "p1-module-provider-openai";
const ROUTE_FILE: &str = "openai-codex-subscription";
const PROFILE: &str = "gpt-5.6-sol";

const BEARER: &str = "WS-PARITY-FAKE-BEARER";
const REFRESHED: &str = "WS-PARITY-FAKE-REFRESHED";
const ACCOUNT: &str = "acct-ws-parity";
const CACHE_KEY: &str = "agent-ws-parity";

/// One visible delta: everything after it is a failure of THIS response (§5).
const DELTA: &str = r#"{"type":"response.output_text.delta","delta":"Hello"}"#;

// ---- the two providers -------------------------------------------------------------------

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The component as `scripts/build-modules.sh` published it, loaded once per test binary.
struct Built {
    _release: Release,
    module: LoadedModule,
}

fn module() -> &'static LoadedModule {
    static BUILT: OnceLock<Built> = OnceLock::new();
    &BUILT
        .get_or_init(|| {
            let dir = fixture_dir()
                .parent()
                .expect("the published packages directory")
                .join(PACKAGE);
            let read = |file: String| {
                let path = dir.join(file);
                std::fs::read(&path).unwrap_or_else(|error| {
                    panic!(
                        "the provider artifact {} is missing ({error}): run scripts/build-modules.sh first",
                        path.display()
                    )
                })
            };
            let manifest: Value = serde_json::from_slice(&read(format!("{PACKAGE}.manifest.json")))
                .expect("a package manifest");
            let file = OPENAI.replace('/', "-");
            let mut release = Release::empty();
            release.add(
                json!({
                    "name": OPENAI,
                    "digest": manifest["digest"],
                    "path": format!("packages/{file}/{file}.wasm"),
                    "kind": manifest["kind"],
                    "world": manifest["world"],
                    "protocol": manifest["protocol"],
                    "capabilities": manifest["capabilities"],
                    "variant": manifest["variant"],
                }),
                &read(format!("{PACKAGE}.wasm")),
            );
            let module = release
                .loader()
                .load(OPENAI)
                .unwrap_or_else(|error| panic!("load {OPENAI}: {error}"));
            Built {
                _release: release,
                module,
            }
        })
        .module
}

/// The shipped route, which asks for `transport = "websocket"`.
fn route() -> RouteFile {
    let path = repo_root()
        .join("routes")
        .join(format!("{ROUTE_FILE}.toml"));
    load_route(&path).unwrap_or_else(|error| panic!("{error}"))
}

fn profile_text() -> String {
    let path = repo_root().join("profiles").join(format!("{PROFILE}.toml"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| panic!("{}: {error}", path.display()))
}

/// The credential source: a fake bearer with the account id this account needs, and a
/// record of every refresh.
#[derive(Default)]
struct Credentials {
    refreshed: Mutex<Vec<String>>,
}

impl CredentialSource for Credentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: Some(ACCOUNT.into()),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.refreshed.lock().unwrap().push(rejected.bearer.clone());
            Ok(Credential {
                bearer: REFRESHED.into(),
                account_id: Some(ACCOUNT.into()),
            })
        })
    }
}

/// The session's reuse clock is the paused Tokio clock, so fake time drives it too.
fn clock() -> Clock {
    Arc::new(|| tokio::time::Instant::now().into_std())
}

/// What one side of a case ran on, and what it recorded.
struct Side {
    provider: Arc<dyn Provider>,
    peer: ScriptedWsConnector,
    http: ScriptedTransport,
    credentials: Arc<Credentials>,
}

/// One script: the peer's connections, and the HTTP answers a fallback gets.
#[derive(Clone)]
struct Script {
    connections: Vec<ScriptedConnection>,
    http: Vec<ScriptedResponse>,
}

impl Script {
    fn peer(connections: Vec<ScriptedConnection>) -> Self {
        Self {
            connections,
            http: Vec::new(),
        }
    }

    fn with_http(mut self, http: Vec<ScriptedResponse>) -> Self {
        self.http = http;
        self
    }

    /// The native adapter the host built for this route until S7.10-R5.
    fn native(&self) -> Side {
        let route = route();
        let binding = route.binding(PROFILE).expect("a bound profile");
        let profile =
            ModelProfile::from_toml(PROFILE, &profile_text()).expect("the shipped profile");
        let peer = ScriptedWsConnector::new(self.connections.clone());
        let http = ScriptedTransport::new(self.http.clone());
        let credentials = Arc::new(Credentials::default());
        let provider = OpenAiCodexProvider::builder(
            responses_route(&route).expect("a Responses route"),
            &binding.wire_model,
            Arc::new(profile),
            Arc::new(http.clone()),
            credentials.clone(),
        )
        .with_ws_connector(Arc::new(peer.clone()))
        .with_clock(clock())
        .build()
        .expect("the native adapter composes");
        Side {
            provider: Arc::new(provider),
            peer,
            http,
            credentials,
        }
    }

    /// The component, configured as the host's activation configures it.
    fn component(&self) -> Side {
        let route = route();
        let binding = route.binding(PROFILE).expect("a bound profile");
        let settings = ProviderSettings {
            origin_route: route.origin_route.clone(),
            endpoint: route.endpoint.clone(),
            model: PROFILE.to_owned(),
            wire_model: binding.wire_model.clone(),
            adapter_settings: route.component_adapter_settings(binding, PROFILE, &profile_text()),
        };
        let peer = ScriptedWsConnector::new(self.connections.clone());
        let http = ScriptedTransport::new(self.http.clone());
        let credentials = Arc::new(Credentials::default());
        let provider = WasmProvider::new(
            module(),
            settings,
            credentials.clone(),
            Arc::new(http.clone()),
            ExecutionLimits::default(),
        )
        .expect("the component activates")
        .with_websocket(Arc::new(peer.clone()), clock());
        Side {
            provider: Arc::new(provider),
            peer,
            http,
            credentials,
        }
    }
}

// ---- requests and streams ----------------------------------------------------------------

fn user(text: &str) -> Item {
    Item::User { text: text.into() }
}

fn request(history: Vec<Item>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".into(),
        history,
        tools: Vec::new(),
        options: ModelOptions {
            cache_key: Some(CACHE_KEY.into()),
            ..ModelOptions::default()
        },
    }
}

/// The second turn of a conversation whose first response was the `NO_USAGE` transcript's
/// one assistant message, exactly as the next request encodes it.
fn continued() -> Vec<Item> {
    vec![
        user("hi"),
        Item::Assistant(AssistantItem {
            origin: Origin {
                route: route().origin_route,
                model: route()
                    .binding(PROFILE)
                    .expect("a bound profile")
                    .wire_model
                    .clone(),
            },
            blocks: vec![AssistantBlock::Text { text: "ok".into() }],
        }),
        user("again"),
    ]
}

async fn collect(mut stream: ProviderStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

async fn turn(side: &Side, request: ProviderRequest) -> Vec<StreamEvent> {
    let stream = side
        .provider
        .stream(request, CancellationToken::new())
        .await
        .expect("the request lowers");
    collect(stream).await
}

/// The text frames one SSE fixture becomes: each received text is ONE JSON event.
fn text_frames(sse: &str) -> Vec<ScriptedFrame> {
    sse.lines()
        .filter_map(|line| line.strip_prefix("data: "))
        .map(ScriptedFrame::text)
        .collect()
}

fn turn_frames(turns: usize) -> Vec<ScriptedFrame> {
    (0..turns)
        .flat_map(|_| text_frames(fixtures::NO_USAGE))
        .collect()
}

fn terminal(events: &[StreamEvent]) -> &Outcome {
    let terminals: Vec<&Outcome> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Finished(outcome) => Some(outcome),
            _ => None,
        })
        .collect();
    assert_eq!(terminals.len(), 1, "exactly one terminal event: {events:?}");
    assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
    terminals[0]
}

fn notices(events: &[StreamEvent]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Notice { text } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn frame(side: &Side, connection: usize, index: usize) -> Value {
    serde_json::from_str(&side.peer.sent_texts()[connection][index]).expect("a JSON frame")
}

/// Both sides put the same bytes on the wire: handshakes (URL, header names and values, in
/// order), every frame of every connection, and every HTTP request (URL, body, headers by
/// lowercased name).
fn assert_same_wire(native: &Side, component: &Side) {
    let handshakes = |side: &Side| {
        side.peer
            .handshakes()
            .into_iter()
            .map(|handshake| (handshake.url, handshake.headers))
            .collect::<Vec<_>>()
    };
    assert_eq!(handshakes(component), handshakes(native), "handshakes");
    let frames = |side: &Side| -> Vec<Vec<Value>> {
        side.peer
            .sent_texts()
            .iter()
            .map(|texts| {
                texts
                    .iter()
                    .map(|text| serde_json::from_str(text).expect("a JSON frame"))
                    .collect()
            })
            .collect()
    };
    assert_eq!(frames(component), frames(native), "frames");
    let requests = |side: &Side| {
        side.http
            .requests()
            .into_iter()
            .map(|request| {
                let headers: Vec<(String, String)> = request
                    .headers
                    .into_iter()
                    .map(|(name, value)| (name.to_ascii_lowercase(), value))
                    .collect();
                let body: Value = serde_json::from_slice(&request.body).expect("a JSON body");
                (request.url, headers, body)
            })
            .collect::<Vec<_>>()
    };
    assert_eq!(requests(component), requests(native), "HTTP requests");
}

/// Runs `requests` in order on both sides, asserts the same events and the same wire, and
/// returns both sides with the component's events.
async fn parity(
    script: Script,
    requests: Vec<ProviderRequest>,
) -> (Side, Side, Vec<Vec<StreamEvent>>) {
    let native = script.native();
    let component = script.component();
    let mut turns = Vec::new();
    for request in requests {
        let expected = turn(&native, request.clone()).await;
        let actual = turn(&component, request).await;
        assert_eq!(
            actual, expected,
            "the component's events are the native adapter's"
        );
        turns.push(actual);
    }
    assert_same_wire(&native, &component);
    assert_eq!(
        *component.credentials.refreshed.lock().unwrap(),
        *native.credentials.refreshed.lock().unwrap(),
        "refreshes"
    );
    (native, component, turns)
}

// ---- the cases ---------------------------------------------------------------------------

#[tokio::test(start_paused = true)]
async fn a_first_request_opens_the_native_handshake_and_sends_the_native_frame() {
    let script = Script::peer(vec![ScriptedConnection::accept(turn_frames(1))]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    assert!(matches!(terminal(&turns[0]), Outcome::Completed(_)));
    let handshakes = component.peer.handshakes();
    assert_eq!(handshakes.len(), 1);
    assert_eq!(
        handshakes[0].url,
        "wss://chatgpt.com/backend-api/codex/responses"
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
        ]
    );
    let first = frame(&component, 0, 0);
    assert_eq!(first["type"], "response.create");
    assert!(first.get("previous_response_id").is_none(), "{first}");
    assert!(component.http.requests().is_empty(), "no HTTP request");
}

#[tokio::test(start_paused = true)]
async fn a_second_turn_on_the_open_connection_sends_only_the_new_items() {
    let script = Script::peer(vec![ScriptedConnection::accept(turn_frames(2))]);
    let (_, component, turns) = parity(
        script,
        vec![request(vec![user("hi")]), request(continued())],
    )
    .await;
    for events in &turns {
        assert!(matches!(terminal(events), Outcome::Completed(_)));
    }
    assert_eq!(component.peer.handshakes().len(), 1, "one connection");
    let second = frame(&component, 0, 1);
    assert_eq!(second["previous_response_id"], json!("resp_no_usage"));
    assert_eq!(
        second["input"].as_array().map(Vec::len),
        Some(1),
        "{second}"
    );
}

#[tokio::test(start_paused = true)]
async fn a_refused_upgrade_falls_back_to_http_and_turns_websocket_off() {
    let script =
        Script::peer(vec![ScriptedConnection::refuse(404, b"no".to_vec())]).with_http(vec![
            ScriptedResponse::ok_sse(fixtures::NO_USAGE),
            ScriptedResponse::ok_sse(fixtures::NO_USAGE),
        ]);
    let (_, component, turns) = parity(
        script,
        vec![request(vec![user("hi")]), request(continued())],
    )
    .await;
    assert_eq!(
        notices(&turns[0]),
        [
            "transport: WebSocket unavailable (HTTP 404) — using HTTP (SSE) for the rest of this session"
        ]
    );
    assert!(matches!(terminal(&turns[0]), Outcome::Completed(_)));
    assert!(
        notices(&turns[1]).is_empty(),
        "a later request is already on HTTP"
    );
    assert_eq!(
        component.peer.handshakes().len(),
        1,
        "never again WebSocket"
    );
    assert_eq!(component.http.requests().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn a_connect_failure_retries_within_the_budget_then_falls_back() {
    let script = Script::peer(vec![
        ScriptedConnection::fail("dns"),
        ScriptedConnection::fail("dns"),
        ScriptedConnection::fail("dns"),
        ScriptedConnection::fail("dns"),
    ])
    .with_http(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    assert_eq!(
        component.peer.handshakes().len(),
        4,
        "the first try and 3 retries"
    );
    assert_eq!(
        notices(&turns[0]),
        [
            "transport: WebSocket unavailable (connection failed) — using HTTP (SSE) for the rest of this session"
        ]
    );
    assert!(matches!(terminal(&turns[0]), Outcome::Completed(_)));
}

#[tokio::test(start_paused = true)]
async fn a_transient_connect_error_reconnects_and_the_retry_succeeds() {
    let script = Script::peer(vec![
        ScriptedConnection::fail("reset"),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    assert!(matches!(terminal(&turns[0]), Outcome::Completed(_)));
    assert!(
        turns[0].contains(&StreamEvent::Activity),
        "the backoff shows life"
    );
    assert!(component.http.requests().is_empty(), "no fallback");
}

#[tokio::test(start_paused = true)]
async fn an_upgrade_refused_with_401_refreshes_once_and_reconnects() {
    let script = Script::peer(vec![
        ScriptedConnection::refuse(401, b"expired".to_vec()),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    assert!(matches!(terminal(&turns[0]), Outcome::Completed(_)));
    assert_eq!(*component.credentials.refreshed.lock().unwrap(), [BEARER]);
    let handshakes = component.peer.handshakes();
    assert_eq!(handshakes.len(), 2);
    assert_eq!(handshakes[1].headers[0].1, format!("Bearer {REFRESHED}"));
}

#[tokio::test(start_paused = true)]
async fn a_second_refusal_after_the_refresh_is_authentication() {
    let script = Script::peer(vec![
        ScriptedConnection::refuse(401, b"expired".to_vec()),
        ScriptedConnection::refuse(401, b"expired".to_vec()),
    ]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    match terminal(&turns[0]) {
        Outcome::Failed(error) => assert_eq!(error.kind, ProviderErrorKind::Authentication),
        other => panic!("expected an authentication failure, saw {other:?}"),
    }
    assert!(component.http.requests().is_empty(), "no fallback");
}

#[tokio::test(start_paused = true)]
async fn an_upgrade_refused_with_429_is_rate_limited_without_fallback() {
    let script = Script::peer(vec![ScriptedConnection::refuse(429, b"slow".to_vec())]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    assert!(matches!(terminal(&turns[0]), Outcome::Failed(_)));
    assert!(component.http.requests().is_empty(), "no fallback");
}

#[tokio::test(start_paused = true)]
async fn a_failure_after_output_is_not_retried() {
    let script = Script::peer(vec![ScriptedConnection::accept(vec![
        ScriptedFrame::text(r#"{"type":"response.created","response":{"id":"resp_1"}}"#),
        ScriptedFrame::text(DELTA),
        ScriptedFrame::close(),
    ])]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    match terminal(&turns[0]) {
        Outcome::Failed(error) => assert_eq!(error.kind, ProviderErrorKind::Transport),
        other => panic!("expected a transport failure, saw {other:?}"),
    }
    assert_eq!(component.peer.handshakes().len(), 1, "no reconnect");
    assert!(component.http.requests().is_empty(), "no fallback");
}

#[tokio::test(start_paused = true)]
async fn a_close_before_any_output_reconnects_within_the_budget() {
    let script = Script::peer(vec![
        ScriptedConnection::accept(vec![ScriptedFrame::close()]),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (_, component, turns) = parity(script, vec![request(vec![user("hi")])]).await;
    assert!(matches!(terminal(&turns[0]), Outcome::Completed(_)));
    assert_eq!(component.peer.handshakes().len(), 2);
}

#[tokio::test(start_paused = true)]
async fn previous_response_not_found_reconnects_once_and_sends_the_full_body() {
    let script = Script::peer(vec![
        ScriptedConnection::accept(
            turn_frames(1)
                .into_iter()
                .chain([ScriptedFrame::text(
                    r#"{"type":"error","error":{"code":"previous_response_not_found"}}"#,
                )])
                .collect(),
        ),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (_, component, turns) = parity(
        script,
        vec![request(vec![user("hi")]), request(continued())],
    )
    .await;
    assert!(matches!(terminal(&turns[1]), Outcome::Completed(_)));
    assert_eq!(component.peer.handshakes().len(), 2);
    let continuation = frame(&component, 0, 1);
    assert_eq!(continuation["previous_response_id"], json!("resp_no_usage"));
    let full = frame(&component, 1, 0);
    assert!(full.get("previous_response_id").is_none(), "{full}");
}

#[tokio::test(start_paused = true)]
async fn a_reused_socket_that_closes_before_its_first_frame_reconnects_once() {
    let script = Script::peer(vec![
        ScriptedConnection::accept(
            turn_frames(1)
                .into_iter()
                .chain([ScriptedFrame::close()])
                .collect(),
        ),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (_, component, turns) = parity(
        script,
        vec![request(vec![user("hi")]), request(continued())],
    )
    .await;
    assert!(matches!(terminal(&turns[1]), Outcome::Completed(_)));
    assert_eq!(component.peer.handshakes().len(), 2);
    assert!(
        !turns[1].contains(&StreamEvent::Activity),
        "a once row reconnects without the backoff"
    );
}

#[tokio::test(start_paused = true)]
async fn an_idle_connection_is_dropped_and_the_next_request_reconnects_in_full() {
    let native = Script::peer(vec![
        ScriptedConnection::accept(turn_frames(1)),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (native, component) = (native.native(), native.component());
    for side in [&native, &component] {
        assert!(matches!(
            terminal(&turn(side, request(vec![user("hi")])).await),
            Outcome::Completed(_)
        ));
    }
    tokio::time::advance(Duration::from_secs(5 * 60)).await;
    let expected = turn(&native, request(continued())).await;
    let actual = turn(&component, request(continued())).await;
    assert_eq!(actual, expected);
    assert_same_wire(&native, &component);
    assert_eq!(
        component.peer.handshakes().len(),
        2,
        "§4: idle past 5 minutes"
    );
    assert!(
        frame(&component, 1, 0)
            .get("previous_response_id")
            .is_none()
    );
}

/// Cancelling a request in the middle of its response drops its socket and settles the call
/// as cancelled, and the next request opens a new connection with the full body.
#[tokio::test(start_paused = true)]
async fn cancellation_closes_the_socket_and_a_later_request_reconnects() {
    let script = Script::peer(vec![
        ScriptedConnection::accept(vec![
            ScriptedFrame::text(r#"{"type":"response.created","response":{"id":"resp_1"}}"#),
            ScriptedFrame::text(DELTA),
            ScriptedFrame::wait(Duration::from_secs(60)),
        ]),
        ScriptedConnection::accept(turn_frames(1)),
    ]);
    let (native, component) = (script.native(), script.component());
    for side in [&native, &component] {
        let cancel = CancellationToken::new();
        let mut stream = side
            .provider
            .stream(request(vec![user("hi")]), cancel.clone())
            .await
            .expect("the request lowers");
        let mut events = Vec::new();
        while let Some(event) = stream.next().await {
            let delta = matches!(event, StreamEvent::TextDelta { .. });
            events.push(event);
            if delta {
                break;
            }
        }
        cancel.cancel();
        events.extend(collect(stream).await);
        assert_eq!(terminal(&events), &Outcome::Cancelled);
        let events = turn(side, request(vec![user("hi")])).await;
        assert!(matches!(terminal(&events), Outcome::Completed(_)));
    }
    assert_same_wire(&native, &component);
    assert_eq!(component.peer.handshakes().len(), 2, "a new connection");
    assert!(
        frame(&component, 1, 0)
            .get("previous_response_id")
            .is_none()
    );
}

/// A request that arrives while another request's response holds the connection waits for
/// it, then continues on the same connection: the frozen `connection-state` has no fact for
/// a busy session and a second socket is never opened (the native adapter sent such a request
/// over HTTP instead).
#[tokio::test(start_paused = true)]
async fn a_request_waits_while_another_response_holds_the_connection() {
    let script = Script::peer(vec![ScriptedConnection::accept(
        std::iter::once(ScriptedFrame::wait(Duration::from_secs(10)))
            .chain(turn_frames(2))
            .collect(),
    )]);
    let component = script.component();
    let first = component
        .provider
        .stream(request(vec![user("hi")]), CancellationToken::new())
        .await
        .expect("the first request lowers");
    let second = component
        .provider
        .stream(request(continued()), CancellationToken::new());
    let (first, second) = tokio::join!(collect(first), async {
        collect(second.await.expect("the second request lowers")).await
    });
    assert!(matches!(terminal(&first), Outcome::Completed(_)));
    assert!(matches!(terminal(&second), Outcome::Completed(_)));
    assert_eq!(component.peer.handshakes().len(), 1, "one connection");
    assert!(component.http.requests().is_empty(), "no HTTP request");
    assert_eq!(
        frame(&component, 0, 1)["previous_response_id"],
        json!("resp_no_usage")
    );
}

/// A provider given no session refuses a WebSocket lowering before anything is sent.
#[tokio::test(start_paused = true)]
async fn without_a_session_a_websocket_lowering_is_refused() {
    let route = route();
    let binding = route.binding(PROFILE).expect("a bound profile");
    let http = ScriptedTransport::new(Vec::new());
    let provider = WasmProvider::new(
        module(),
        ProviderSettings {
            origin_route: route.origin_route.clone(),
            endpoint: route.endpoint.clone(),
            model: PROFILE.to_owned(),
            wire_model: binding.wire_model.clone(),
            adapter_settings: route.component_adapter_settings(binding, PROFILE, &profile_text()),
        },
        Arc::new(Credentials::default()),
        Arc::new(http.clone()),
        ExecutionLimits::default(),
    )
    .expect("the component activates");
    let refused = provider
        .stream(request(vec![user("hi")]), CancellationToken::new())
        .await
        .err()
        .expect("a WebSocket lowering without a session is refused");
    assert_eq!(refused.kind, ProviderErrorKind::Protocol);
    assert!(http.requests().is_empty());
}
