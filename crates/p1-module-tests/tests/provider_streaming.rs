//! S4.8: the streaming suite — what a provider component costs while a stream runs, and
//! whether the stream stays incremental.
//!
//! Each case drives one component through `WasmProvider` (S4.7) over a scripted transport
//! whose body hands out one SSE block per poll, one event per block, and counts a poll that
//! asks for block `k` before the consumer acknowledged block `k-1`'s event as read-ahead. That
//! handshake is the incrementality proof: a path that read the body ahead of its consumer —
//! buffered it, or pre-fetched it — shows up in that counter on the very event it passed, and
//! every case asserts the counter is zero and the body handed out no more blocks than the
//! consumer received events. A recorded violation is used rather than a hold, so the case
//! fails with a message instead of hanging. Every case also checks the answer the path must
//! produce, so a broken path fails the case instead of reporting a time for something that did
//! not work.
//!
//! `scripts/bench-modules.sh --suite streaming` runs these cases in the release profile, one
//! process per case, and owns the thresholds: PLAN §10's `provider-events` row (p95 added
//! latency ≤ 2 ms, p99 ≤ 10 ms) is the only PLAN threshold this suite applies. Every other row
//! is a measured fact with no threshold (ANSWERS S4-B6/D076). Nothing here asserts on a time:
//! `cargo test` runs every case at the small default size, and the timing rows are compared by
//! the script.
//!
//! Sizes come from the environment: `P1_STREAM_EVENTS` (default [`DEFAULT_EVENTS`]) is how many
//! text deltas a stream carries. The memory case is read in a process of its own, and its peak
//! RSS is reset once the provider and its instance exist and immediately before its stream, so
//! its row is that stream's own cost and not the instance's setup.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use futures_util::StreamExt;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AssistantBlock, BoxFuture, CancellationToken, Item, ModelOptions, Outcome, Provider,
    ProviderError, ProviderRequest, ProviderStream, StreamEvent, ToolCall, ToolInput,
};
use p1_host::routes::{RouteFile, load_route};
use p1_module_runtime::{ExecutionLimits, LoadedModule, ProviderSettings, WasmProvider};
use p1_module_tests::{Release, fixture_dir, within_deadline};
use p1_provider_http::{
    ByteStream, Credential, CredentialSource, HttpRequest, HttpResponse, Transport, TransportError,
};

/// The three shipped provider components, by the manifest name they are released as.
const ANTHROPIC: &str = "p1/provider-anthropic";
const OPENAI: &str = "p1/provider-openai";
const OPENAI_CHAT: &str = "p1/provider-openai-chat";

const BEARER: &str = "STREAMING-FAKE-BEARER";
const ACCOUNT: &str = "acct-streaming";

/// Text deltas per stream when the environment says nothing: enough for the handshake to be
/// real, small enough that `cargo test` stays fast. The suite sets the measured workload.
const DEFAULT_EVENTS: usize = 1000;
const EVENTS_ENV: &str = "P1_STREAM_EVENTS";

/// The text one delta carries: one byte, so the answer the component accumulates never
/// dominates the peak RSS the memory case reads.
const DELTA: &str = "d";

/// The slow consumer's gate: yields between two events, so a reader of its own would have the
/// chance to run ahead while the consumer is not asking for the next event.
const GATE_YIELDS: usize = 8;

/// The tool-call stream's call, split into the two argument fragments the stream carries.
const TOOL_CALL_ID: &str = "call_1";
const TOOL_NAME: &str = "read";
const TOOL_INPUT_FIRST: &str = "{\"path\":";
const TOOL_INPUT_SECOND: &str = "\"a.txt\"}";

/// The text deltas a stream carries, as the environment asks.
fn events() -> usize {
    match std::env::var(EVENTS_ENV) {
        Ok(text) => text
            .parse()
            .unwrap_or_else(|_| panic!("{EVENTS_ENV} is not a count: {text}")),
        Err(_) => DEFAULT_EVENTS,
    }
}

// ---- the components under test ---------------------------------------------------------

/// The wire dialect a case's synthetic SSE blocks are shaped in: one shipped adapter each.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Dialect {
    /// `anthropic-messages`, the Messages event anatomy.
    Anthropic,
    /// `openai-responses`, the Codex Responses event anatomy.
    Responses,
    /// `openai-chat`, the Chat Completions chunk anatomy.
    Chat,
}

/// One component under test: the shipped route file and model profile it is composed with,
/// which is what its `configure` parses.
struct Case {
    /// The short name the rows carry.
    key: &'static str,
    /// The route and profile the case runs, as a row's detail names it.
    name: &'static str,
    component: &'static str,
    /// The route file's stem under `routes/`.
    route_file: &'static str,
    profile: &'static str,
    dialect: Dialect,
    /// The `[adapter_settings]` the route is composed with here. The Responses route ships
    /// `transport = "websocket"`, which this broker does not send yet, so the case composes it
    /// for SSE exactly as the conformance suite does.
    settings: Option<Value>,
}

fn anthropic_case() -> Case {
    Case {
        key: "anthropic",
        name: "anthropic-messages/claude-subscription",
        component: ANTHROPIC,
        route_file: "anthropic-subscription",
        profile: "claude-sonnet-5",
        dialect: Dialect::Anthropic,
        settings: None,
    }
}

fn responses_case() -> Case {
    Case {
        key: "responses",
        name: "openai-responses/codex-subscription",
        component: OPENAI,
        route_file: "openai-codex-subscription",
        profile: "gpt-5.6-sol",
        dialect: Dialect::Responses,
        settings: Some(json!({"account": "codex-subscription", "transport": "sse"})),
    }
}

fn chat_case() -> Case {
    Case {
        key: "chat",
        name: "openai-chat/glm-subscription",
        component: OPENAI_CHAT,
        route_file: "glm-subscription",
        profile: "glm-5.3",
        dialect: Dialect::Chat,
        settings: None,
    }
}

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The provider components as `scripts/build-modules.sh` published them, loaded once for the
/// whole test binary: a component compile per case would otherwise dominate every reading.
struct Built {
    /// Kept alive: the loader reads the components out of the release laid out here.
    _release: Release,
    modules: Vec<(&'static str, LoadedModule)>,
}

fn built() -> &'static Built {
    static BUILT: OnceLock<Built> = OnceLock::new();
    BUILT.get_or_init(|| {
        let published = fixture_dir()
            .parent()
            .expect("the published packages directory")
            .to_path_buf();
        let mut release = Release::empty();
        for (name, package) in [
            (ANTHROPIC, "p1-module-provider-anthropic"),
            (OPENAI, "p1-module-provider-openai"),
            (OPENAI_CHAT, "p1-module-provider-openai-chat"),
        ] {
            let dir = published.join(package);
            let read = |file: String| {
                let path = dir.join(file);
                std::fs::read(&path).unwrap_or_else(|error| {
                    panic!(
                        "the provider artifact {} is missing ({error}): run scripts/build-modules.sh first",
                        path.display()
                    )
                })
            };
            let manifest: Value =
                serde_json::from_slice(&read(format!("{package}.manifest.json")))
                    .expect("a package manifest");
            assert_eq!(manifest["name"], name, "{package}");
            let file = name.replace('/', "-");
            let entry = json!({
                "name": name,
                "digest": manifest["digest"],
                "path": format!("packages/{file}/{file}.wasm"),
                "kind": manifest["kind"],
                "world": manifest["world"],
                "protocol": manifest["protocol"],
                "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            });
            release.add(entry, &read(format!("{package}.wasm")));
        }
        let loader = release.loader();
        let modules = [ANTHROPIC, OPENAI, OPENAI_CHAT]
            .into_iter()
            .map(|name| {
                let module = loader
                    .load(name)
                    .unwrap_or_else(|error| panic!("load {name}: {error}"));
                (name, module)
            })
            .collect();
        Built {
            _release: release,
            modules,
        }
    })
}

fn module(name: &str) -> &'static LoadedModule {
    built()
        .modules
        .iter()
        .find(|(module, _)| *module == name)
        .map(|(_, module)| module)
        .unwrap_or_else(|| panic!("{name} is not built"))
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
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
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: Some(ACCOUNT.into()),
            })
        })
    }
}

impl Case {
    /// The route file as this case composes it.
    fn route(&self) -> RouteFile {
        let path = repo_root()
            .join("routes")
            .join(format!("{}.toml", self.route_file));
        let mut route = load_route(&path).unwrap_or_else(|error| panic!("{error}"));
        if let Some(settings) = &self.settings {
            route.adapter_settings =
                Some(serde_json::from_value(settings.clone()).expect("an adapter settings table"));
        }
        route
    }

    /// The settings the component is configured with: the route's `[adapter_settings]` plus the
    /// reserved keys of ADR-0086, exactly as the host composes them.
    fn settings(&self, route: &RouteFile) -> ProviderSettings {
        let binding = route.binding(self.profile).expect("a bound profile");
        let profile = repo_root()
            .join("profiles")
            .join(format!("{}.toml", self.profile));
        let text = std::fs::read_to_string(&profile)
            .unwrap_or_else(|error| panic!("{}: {error}", profile.display()));
        ProviderSettings {
            origin_route: route.origin_route.clone(),
            endpoint: route.endpoint.clone(),
            model: self.profile.to_owned(),
            wire_model: binding.wire_model.clone(),
            adapter_settings: route.component_adapter_settings(binding, self.profile, &text),
        }
    }

    fn provider(&self, transport: Arc<dyn Transport>) -> Arc<dyn Provider> {
        let route = self.route();
        let settings = self.settings(&route);
        let provider = WasmProvider::new(
            module(self.component),
            settings,
            Arc::new(FixedCredentials),
            transport,
            ExecutionLimits::default(),
        )
        .unwrap_or_else(|error| panic!("[{}] {error}", self.name));
        Arc::new(provider)
    }
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "streaming prompt".into(),
        history: vec![Item::User {
            text: "stream".into(),
        }],
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

// ---- the scripted body -----------------------------------------------------------------

/// One SSE frame. The chat dialect names no event; the other two do.
fn sse(event: Option<&str>, data: &Value) -> String {
    match event {
        Some(name) => format!("event: {name}\ndata: {data}\n\n"),
        None => format!("data: {data}\n\n"),
    }
}

/// The blocks that open the Messages text stream: the usage envelope, then the text block the
/// deltas extend. Neither produces an event, so they travel with the first delta.
fn anthropic_open() -> Vec<String> {
    vec![
        sse(
            Some("message_start"),
            &json!({"type":"message_start",
                "message":{"id":"msg_stream","type":"message","role":"assistant",
                    "model":"claude-sonnet-5","content":[],"stop_reason":null,
                    "usage":{"input_tokens":10,"output_tokens":1}}}),
        ),
        sse(
            Some("content_block_start"),
            &json!({"type":"content_block_start","index":0,
                "content_block":{"type":"text","text":""}}),
        ),
    ]
}

/// The one block a text-delta chunk carries.
fn delta_block(dialect: Dialect) -> String {
    match dialect {
        Dialect::Anthropic => sse(
            Some("content_block_delta"),
            &json!({"type":"content_block_delta","index":0,
                "delta":{"type":"text_delta","text":DELTA}}),
        ),
        Dialect::Responses => sse(
            Some("response.output_text.delta"),
            &json!({"type":"response.output_text.delta","item_id":"msg_stream","output_index":0,
                "content_index":0,"delta":DELTA}),
        ),
        Dialect::Chat => sse(
            None,
            &json!({"choices":[{"index":0,"delta":{"content":DELTA},"finish_reason":null}]}),
        ),
    }
}

/// The blocks that end the text stream: the answer the deltas spelled out, and one terminal
/// event.
fn trailer(dialect: Dialect, n: usize) -> Vec<u8> {
    let answer = DELTA.repeat(n);
    let body = match dialect {
        Dialect::Anthropic => [
            sse(
                Some("content_block_stop"),
                &json!({"type":"content_block_stop","index":0}),
            ),
            sse(
                Some("message_delta"),
                &json!({"type":"message_delta",
                    "delta":{"stop_reason":"end_turn","stop_sequence":null},
                    "usage":{"output_tokens":n}}),
            ),
            sse(Some("message_stop"), &json!({"type":"message_stop"})),
        ]
        .concat(),
        Dialect::Responses => [
            sse(
                Some("response.output_item.done"),
                &json!({"type":"response.output_item.done","output_index":0,
                    "item":{"id":"msg_stream","type":"message","role":"assistant",
                        "content":[{"type":"output_text","text":answer}]}}),
            ),
            sse(
                Some("response.completed"),
                &json!({"type":"response.completed",
                    "response":{"id":"resp_stream",
                        "usage":{"input_tokens":10,"output_tokens":n}}}),
            ),
        ]
        .concat(),
        Dialect::Chat => [
            sse(
                None,
                &json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}),
            ),
            "data: [DONE]\n\n".to_owned(),
        ]
        .concat(),
    };
    body.into_bytes()
}

/// The three chunks of a tool-call stream: the two argument fragments and the block that
/// carries the complete call and ends the stream.
fn tool_chunks(dialect: Dialect) -> Vec<Vec<u8>> {
    let chunks: Vec<String> = match dialect {
        Dialect::Anthropic => vec![
            [
                sse(
                    Some("message_start"),
                    &json!({"type":"message_start",
                        "message":{"id":"msg_tool","type":"message","role":"assistant",
                            "model":"claude-sonnet-5","content":[],"stop_reason":null,
                            "usage":{"input_tokens":10,"output_tokens":1}}}),
                ),
                sse(
                    Some("content_block_start"),
                    &json!({"type":"content_block_start","index":0,
                        "content_block":{"type":"tool_use","id":TOOL_CALL_ID,"name":TOOL_NAME}}),
                ),
                sse(
                    Some("content_block_delta"),
                    &json!({"type":"content_block_delta","index":0,
                        "delta":{"type":"input_json_delta","partial_json":TOOL_INPUT_FIRST}}),
                ),
            ]
            .concat(),
            sse(
                Some("content_block_delta"),
                &json!({"type":"content_block_delta","index":0,
                    "delta":{"type":"input_json_delta","partial_json":TOOL_INPUT_SECOND}}),
            ),
            [
                sse(
                    Some("content_block_stop"),
                    &json!({"type":"content_block_stop","index":0}),
                ),
                sse(
                    Some("message_delta"),
                    &json!({"type":"message_delta",
                        "delta":{"stop_reason":"tool_use","stop_sequence":null},
                        "usage":{"output_tokens":8}}),
                ),
                sse(Some("message_stop"), &json!({"type":"message_stop"})),
            ]
            .concat(),
        ],
        Dialect::Responses => vec![
            sse(
                Some("response.function_call_arguments.delta"),
                &json!({"type":"response.function_call_arguments.delta","item_id":TOOL_CALL_ID,
                    "delta":TOOL_INPUT_FIRST}),
            ),
            sse(
                Some("response.function_call_arguments.delta"),
                &json!({"type":"response.function_call_arguments.delta","item_id":TOOL_CALL_ID,
                    "delta":TOOL_INPUT_SECOND}),
            ),
            [
                sse(
                    Some("response.output_item.done"),
                    &json!({"type":"response.output_item.done","output_index":0,
                        "item":{"id":"fc_1","type":"function_call","call_id":TOOL_CALL_ID,
                            "name":TOOL_NAME,
                            "arguments":format!("{TOOL_INPUT_FIRST}{TOOL_INPUT_SECOND}")}}),
                ),
                sse(
                    Some("response.completed"),
                    &json!({"type":"response.completed","response":{"id":"resp_tool"}}),
                ),
            ]
            .concat(),
        ],
        Dialect::Chat => vec![
            sse(
                None,
                &json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,
                    "id":TOOL_CALL_ID,"type":"function",
                    "function":{"name":TOOL_NAME,"arguments":TOOL_INPUT_FIRST}}]},
                    "finish_reason":null}]}),
            ),
            sse(
                None,
                &json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,
                    "function":{"arguments":TOOL_INPUT_SECOND}}]},"finish_reason":null}]}),
            ),
            [
                sse(
                    None,
                    &json!({"choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}]}),
                ),
                "data: [DONE]\n\n".to_owned(),
            ]
            .concat(),
        ],
    };
    chunks.into_iter().map(String::into_bytes).collect()
}

/// The scripted body: the chunk at an index, and how many chunks there are. Every chunk makes
/// exactly one event, so the consumer's event count is the body's chunk count.
#[derive(Clone)]
enum Body {
    /// `n` text deltas, one block each, then the blocks that end the stream. The chunks are
    /// three shared templates, so a stream of a million deltas allocates three of them.
    Text {
        first: Arc<Vec<u8>>,
        delta: Arc<Vec<u8>>,
        trailer: Arc<Vec<u8>>,
        n: usize,
    },
    /// A tool-call stream: two argument fragments and the block that ends the stream.
    Tool { chunks: Vec<Arc<Vec<u8>>> },
}

impl Body {
    fn text(dialect: Dialect, n: usize) -> Self {
        let mut first = match dialect {
            Dialect::Anthropic => anthropic_open(),
            Dialect::Responses | Dialect::Chat => Vec::new(),
        };
        first.push(delta_block(dialect));
        Self::Text {
            first: Arc::new(first.concat().into_bytes()),
            delta: Arc::new(delta_block(dialect).into_bytes()),
            trailer: Arc::new(trailer(dialect, n)),
            n,
        }
    }

    fn tool(dialect: Dialect) -> Self {
        Self::Tool {
            chunks: tool_chunks(dialect).into_iter().map(Arc::new).collect(),
        }
    }

    fn len(&self) -> usize {
        match self {
            Self::Text { n, .. } => n + 1,
            Self::Tool { chunks } => chunks.len(),
        }
    }

    fn chunk(&self, k: usize) -> Option<Arc<Vec<u8>>> {
        match self {
            Self::Text {
                first,
                delta,
                trailer,
                n,
            } => match k {
                0 => Some(Arc::clone(first)),
                k if k < *n => Some(Arc::clone(delta)),
                k if k == *n => Some(Arc::clone(trailer)),
                _ => None,
            },
            Self::Tool { chunks } => chunks.get(k).map(Arc::clone),
        }
    }
}

// ---- the gated transport ---------------------------------------------------------------

/// The consumer's side of the body handshake, and what the body observed.
struct Probe {
    state: Mutex<State>,
    /// Events the consumer has taken receipt of. A poll for chunk `k` is read-ahead unless the
    /// event before it — the one chunk `k-1` produced — has been acknowledged.
    acks: AtomicUsize,
}

#[derive(Default)]
struct State {
    /// Chunks the body has handed out.
    handed: usize,
    /// When it handed out the newest one: the moment the added latency is measured from.
    released: Option<Instant>,
    /// Polls that arrived before the acknowledgement of the event before them: the reader asked
    /// for a chunk the consumer had not reached. Anything but zero is read-ahead.
    asked_ahead: usize,
}

impl Probe {
    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Records that the body handed out chunk `k`, and when.
    fn handed(&self, k: usize) {
        let mut state = self.lock();
        state.handed = k + 1;
        state.released = Some(Instant::now());
    }

    fn asked_ahead(&self) {
        self.lock().asked_ahead += 1;
    }

    /// Tells the body that the event it just delivered was consumed, so the chunk after that
    /// event's chunk is no longer read-ahead.
    fn ack(&self) {
        self.acks.fetch_add(1, Ordering::SeqCst);
    }

    fn acknowledged(&self) -> usize {
        self.acks.load(Ordering::SeqCst)
    }

    fn chunks(&self) -> usize {
        self.lock().handed
    }

    fn read_ahead(&self) -> usize {
        self.lock().asked_ahead
    }

    fn released(&self) -> Instant {
        self.lock().released.expect("a chunk was handed out")
    }
}

/// A scripted transport with one response: a `200` SSE body that hands out one chunk per poll
/// and records every poll that asked for a chunk before the consumer acknowledged the event
/// before it.
struct GatedTransport {
    body: Body,
    probe: Arc<Probe>,
}

impl GatedTransport {
    fn new(body: Body) -> (Self, Arc<Probe>) {
        let probe = Arc::new(Probe {
            state: Mutex::new(State::default()),
            acks: AtomicUsize::new(0),
        });
        (
            Self {
                body,
                probe: Arc::clone(&probe),
            },
            probe,
        )
    }
}

impl Transport for GatedTransport {
    fn post<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        let body = self.body.clone();
        let probe = Arc::clone(&self.probe);
        Box::pin(async move {
            Ok(HttpResponse {
                status: 200,
                headers: Vec::new(),
                body: body_stream(body, probe),
            })
        })
    }
}

/// The body stream: one chunk per poll.
fn body_stream(body: Body, probe: Arc<Probe>) -> ByteStream {
    let chunks = body.len();
    Box::pin(futures_util::stream::unfold(0usize, move |k| {
        let probe = Arc::clone(&probe);
        let body = body.clone();
        async move {
            if k >= chunks {
                return None;
            }
            if k > 0 && probe.acknowledged() < k {
                // Chunk k was asked for before the event of chunk k-1 was consumed: the
                // reader ran ahead of its consumer. The chunk is handed out anyway, so the
                // case that records this fails on its own assertion instead of hanging.
                probe.asked_ahead();
            }
            // The chunk is released to the reader here: this is the moment the case's
            // added latency is measured from.
            probe.handed(k);
            let chunk = body
                .chunk(k)
                .expect("the body has a chunk for every index below its length");
            Some((Ok(chunk.as_ref().clone()), k + 1))
        }
    }))
}

// ---- the consumer ----------------------------------------------------------------------

/// What a case does with each event, and what it keeps of it.
#[derive(Clone, Copy)]
struct Mode {
    /// Hold the consumer between events: the slow-consumer gate.
    gate: bool,
    /// Keep the per-event added latency.
    latencies: bool,
}

/// What the consumer saw.
#[derive(Default)]
struct Run {
    /// Events received, in arrival order.
    events: usize,
    /// Bytes the text deltas carried — never the text itself, so the memory case does not hold
    /// the stream in the host.
    text_bytes: usize,
    /// The tool-input fragments, in order.
    tool_inputs: Vec<String>,
    /// Per-event added latency in nanoseconds: the body's release of the event's chunk to the
    /// consumer's receipt of it.
    latencies: Vec<u64>,
    /// The event that ended the stream.
    terminal: Option<Outcome>,
    /// When the first and the last event arrived, for the rate.
    first: Option<Instant>,
    last: Option<Instant>,
}

impl Run {
    /// Events per second the consumer received them at, while the stream ran.
    fn rate(&self) -> f64 {
        match (self.first, self.last) {
            (Some(first), Some(last)) if self.events > 1 => {
                let seconds = last.duration_since(first).as_secs_f64();
                (self.events - 1) as f64 / seconds
            }
            _ => 0.0,
        }
    }

    /// The answer the terminal item carries, when the stream completed.
    fn answer(&self) -> Option<&str> {
        match self.terminal.as_ref()? {
            Outcome::Completed(response) => {
                response.item.blocks.iter().find_map(|block| match block {
                    AssistantBlock::Text { text } => Some(text.as_str()),
                    _ => None,
                })
            }
            _ => None,
        }
    }

    /// The tool call the terminal item carries.
    fn tool_call(&self) -> Option<(&str, &ToolInput)> {
        match self.terminal.as_ref()? {
            Outcome::Completed(response) => {
                response.item.blocks.iter().find_map(|block| match block {
                    AssistantBlock::ToolCall(ToolCall { name, input, .. }) => {
                        Some((name.as_str(), input))
                    }
                    _ => None,
                })
            }
            _ => None,
        }
    }
}

/// Consumes `stream`, acknowledging every event so the gated body may hand out the next chunk.
async fn consume(case: &str, probe: &Probe, stream: &mut ProviderStream, mode: Mode) -> Run {
    let mut run = Run::default();
    while let Some(event) = stream.next().await {
        let now = Instant::now();
        run.events += 1;
        let chunks = probe.chunks();
        assert_eq!(
            chunks, run.events,
            "{case}: the body handed out {chunks} chunk(s) for {} event(s): the stream is not \
             incremental",
            run.events
        );
        if mode.latencies {
            run.latencies
                .push(now.duration_since(probe.released()).as_nanos() as u64);
        }
        match &event {
            StreamEvent::TextDelta { text, .. } => run.text_bytes += text.len(),
            StreamEvent::ToolInputDelta { text, .. } => run.tool_inputs.push(text.clone()),
            StreamEvent::Finished(outcome) => run.terminal = Some(outcome.clone()),
            _ => {}
        }
        run.first.get_or_insert(now);
        run.last = Some(now);
        if mode.gate {
            slow_consumer_gate(case, probe, &run).await;
        }
        probe.ack();
    }
    run
}

/// The slow consumer: it does not ask for the next event, so the transport must not hand one
/// out. The yields give a reader of its own the chance to run ahead between the two readings
/// of the read counter.
async fn slow_consumer_gate(case: &str, probe: &Probe, run: &Run) {
    let before = probe.chunks();
    for _ in 0..GATE_YIELDS {
        tokio::task::yield_now().await;
    }
    let after = probe.chunks();
    assert_eq!(
        after, before,
        "{case}: the body was read ahead while the consumer held its gate ({before} chunk(s) \
         before the gate, {after} after it, {} event(s) received)",
        run.events
    );
}

/// `case`'s component over a scripted transport that hands out `body`, with the transport's
/// probe: the provider and the probe are the caller's, so a case can read the process's memory
/// after the provider exists and before its stream runs.
fn provider_with(case: &Case, body: Body) -> (Arc<dyn Provider>, Arc<Probe>) {
    let (transport, probe) = GatedTransport::new(body);
    (case.provider(Arc::new(transport)), probe)
}

/// Runs one stream on `provider` and returns what the consumer saw. One provider serves one
/// stream: a provider holds one transport, and its probe counts one stream's chunks.
async fn stream_of(
    case: &Case,
    provider: &Arc<dyn Provider>,
    probe: &Arc<Probe>,
    mode: Mode,
) -> Run {
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap_or_else(|error| panic!("[{}] the stream did not set up: {error}", case.name));
    consume(case.name, probe, &mut stream, mode).await
}

/// The whole case for one body: build the provider, run its one stream.
async fn run_case(case: &Case, body: Body, mode: Mode) -> (Arc<Probe>, Run) {
    let (provider, probe) = provider_with(case, body);
    let run = stream_of(case, &provider, &probe, mode).await;
    (probe, run)
}

// ---- the rows --------------------------------------------------------------------------

/// PLAN §10's `provider-events` row: the added latency of forwarding one event, and the
/// incrementality of the path that forwards it.
async fn latency_row(case: &Case) {
    let n = events();
    let mode = Mode {
        gate: false,
        latencies: true,
    };
    let (probe, run) = run_case(case, Body::text(case.dialect, n), mode).await;
    assert_eq!(
        run.events,
        n + 1,
        "[{}] {n} deltas and one terminal event",
        case.name
    );
    assert_eq!(run.text_bytes, n, "[{}] every delta arrived", case.name);
    assert_eq!(
        run.answer(),
        Some(DELTA.repeat(n).as_str()),
        "[{}] the answer the deltas spelled out",
        case.name
    );
    assert_eq!(
        probe.read_ahead(),
        0,
        "[{}] the body was asked for a chunk before the event before it was consumed",
        case.name
    );
    let samples: Vec<f64> = run
        .latencies
        .iter()
        .map(|nanos| *nanos as f64 / 1000.0)
        .collect();
    let measurements = vec![
        measurement("p50", percentile(&samples, 50), "us"),
        measurement("p95", percentile(&samples, 95), "us"),
        measurement("p99", percentile(&samples, 99), "us"),
        measurement("max", max_of(&samples), "us"),
        measurement("rate", run.rate(), "events/s"),
        measurement("read-ahead", probe.read_ahead() as f64, "chunks"),
    ];
    emit(
        &format!("latency-{}", case.key),
        measurements,
        run.events,
        format!(
            "{}: {n} text deltas and one terminal event, one SSE block per chunk, one event per \
             block, no read-ahead; the body released each chunk after the event before it was \
             consumed",
            case.name
        ),
    );
}

/// The slow-consumer case: the consumer holds its gate between events, and the transport's
/// read counter must not move.
async fn slow_consumer_row(case: &Case) {
    let n = events();
    let mode = Mode {
        gate: true,
        latencies: false,
    };
    let (probe, run) = run_case(case, Body::text(case.dialect, n), mode).await;
    assert_eq!(
        run.events,
        n + 1,
        "[{}] {n} deltas and one terminal event",
        case.name
    );
    assert_eq!(run.text_bytes, n, "[{}] every delta arrived", case.name);
    assert_eq!(
        run.answer().map(str::len),
        Some(n),
        "[{}] the answer the deltas spelled out",
        case.name
    );
    assert_eq!(
        probe.chunks(),
        n + 1,
        "[{}] the body handed out exactly the chunks the asked-for events came from",
        case.name
    );
    assert_eq!(
        probe.read_ahead(),
        0,
        "[{}] the body was asked for a chunk before the event before it was consumed",
        case.name
    );
    let measurements = vec![
        measurement("read-ahead", 0.0, "chunks"),
        measurement("chunks-handed", probe.chunks() as f64, "chunks"),
        measurement("gates", n as f64, "events"),
    ];
    emit(
        &format!("slow-consumer-{}", case.key),
        measurements,
        run.events,
        format!(
            "{}: the consumer held its gate between every one of {n} events; the scripted \
             transport's read counter did not move and no chunk was asked for early",
            case.name
        ),
    );
}

/// The tool-call stream: the argument fragments arrive as the body releases them, and the
/// terminal item carries the complete call.
async fn tool_call_row(case: &Case) {
    let mode = Mode {
        gate: false,
        latencies: true,
    };
    let (probe, run) = run_case(case, Body::tool(case.dialect), mode).await;
    assert_eq!(
        run.events, 3,
        "[{}] two fragments and one terminal",
        case.name
    );
    assert_eq!(
        run.tool_inputs,
        vec![TOOL_INPUT_FIRST.to_owned(), TOOL_INPUT_SECOND.to_owned()],
        "[{}] the argument fragments, in order",
        case.name
    );
    let expected = ToolInput::Json(format!("{TOOL_INPUT_FIRST}{TOOL_INPUT_SECOND}"));
    assert_eq!(
        run.tool_call(),
        Some((TOOL_NAME, &expected)),
        "[{}] the complete call the terminal item carries",
        case.name
    );
    assert_eq!(
        probe.read_ahead(),
        0,
        "[{}] the body was asked for a chunk before the event before it was consumed",
        case.name
    );
    let samples: Vec<f64> = run
        .latencies
        .iter()
        .map(|nanos| *nanos as f64 / 1000.0)
        .collect();
    let measurements = vec![
        measurement("tool-input-deltas", run.tool_inputs.len() as f64, "deltas"),
        measurement("read-ahead", 0.0, "chunks"),
        measurement("p95", percentile(&samples, 95), "us"),
    ];
    emit(
        &format!("tool-call-{}", case.key),
        measurements,
        run.events,
        format!(
            "{}: the two argument fragments arrived one body chunk each, before the chunk that \
             ends the stream",
            case.name
        ),
    );
}

/// The stream's own peak RSS: the process's high-water mark over exactly the stream, after the
/// peak was reset immediately before it and after the provider (and its instance) exists, so
/// the reading is the running stream's cost and not the instance's setup.
async fn memory_row(case: &Case) {
    let n = events();
    let mode = Mode {
        gate: false,
        latencies: false,
    };
    let (provider, probe) = provider_with(case, Body::text(case.dialect, n));
    let before = Memory::read();
    reset_peak();
    let run = stream_of(case, &provider, &probe, mode).await;
    let peak = Memory::read().peak_kib;
    assert_eq!(run.events, n + 1, "[{}] the whole stream ran", case.name);
    assert_eq!(run.text_bytes, n, "[{}] every delta arrived", case.name);
    assert_eq!(
        run.answer().map(str::len),
        Some(n),
        "[{}] the answer the deltas spelled out",
        case.name
    );
    let measurements = vec![
        measurement(
            "peak-rss-delta",
            (peak - before.rss_kib) as f64 / 1024.0,
            "MiB",
        ),
        measurement("peak-rss", peak as f64 / 1024.0, "MiB"),
        measurement("text-deltas", n as f64, "deltas"),
    ];
    emit(
        &format!("memory-{}", case.key),
        measurements,
        run.events,
        format!(
            "{}: {n} text deltas; the process peak RSS was reset once the provider existed and \
             immediately before the stream, so peak-rss-delta is this stream's own cost",
            case.name
        ),
    );
}

// ---- the cases -------------------------------------------------------------------------

#[tokio::test]
async fn streaming_latency_anthropic() {
    within_deadline(
        "streaming_latency_anthropic",
        latency_row(&anthropic_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_latency_responses() {
    within_deadline(
        "streaming_latency_responses",
        latency_row(&responses_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_latency_chat() {
    within_deadline("streaming_latency_chat", latency_row(&chat_case())).await;
}

#[tokio::test]
async fn streaming_slow_consumer_anthropic() {
    within_deadline(
        "streaming_slow_consumer_anthropic",
        slow_consumer_row(&anthropic_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_slow_consumer_responses() {
    within_deadline(
        "streaming_slow_consumer_responses",
        slow_consumer_row(&responses_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_slow_consumer_chat() {
    within_deadline(
        "streaming_slow_consumer_chat",
        slow_consumer_row(&chat_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_tool_call_anthropic() {
    within_deadline(
        "streaming_tool_call_anthropic",
        tool_call_row(&anthropic_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_tool_call_responses() {
    within_deadline(
        "streaming_tool_call_responses",
        tool_call_row(&responses_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_tool_call_chat() {
    within_deadline("streaming_tool_call_chat", tool_call_row(&chat_case())).await;
}

#[tokio::test]
async fn streaming_peak_rss_anthropic() {
    within_deadline(
        "streaming_peak_rss_anthropic",
        memory_row(&anthropic_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_peak_rss_responses() {
    within_deadline(
        "streaming_peak_rss_responses",
        memory_row(&responses_case()),
    )
    .await;
}

#[tokio::test]
async fn streaming_peak_rss_chat() {
    within_deadline("streaming_peak_rss_chat", memory_row(&chat_case())).await;
}

// ---- readings, statistics and output ----------------------------------------------------

/// The process's memory as the kernel accounts it.
struct Memory {
    /// `VmRSS` of `/proc/self/status`.
    rss_kib: i64,
    /// `VmHWM` of `/proc/self/status`: the peak RSS since the last [`reset_peak`].
    peak_kib: i64,
}

impl Memory {
    fn read() -> Self {
        let status = std::fs::read_to_string("/proc/self/status").expect("/proc/self/status");
        Self {
            rss_kib: field(&status, "VmRSS"),
            peak_kib: field(&status, "VmHWM"),
        }
    }
}

/// Starts a new peak RSS measurement: `5` to `clear_refs` resets `VmHWM` to the current RSS.
fn reset_peak() {
    std::fs::write("/proc/self/clear_refs", "5").expect("reset the peak RSS");
}

/// The number of `<name>:` in a `/proc` key-value text.
fn field(text: &str, name: &str) -> i64 {
    text.lines()
        .find_map(|line| {
            let (key, value) = line.split_once(':')?;
            (key == name).then(|| value.split_whitespace().next()?.parse().ok())?
        })
        .unwrap_or_else(|| panic!("no {name} in the /proc reading"))
}

/// The nearest-rank percentile `p` of `samples`: the smallest sample at or above which `p`
/// percent of the samples lie, so it is always a sample that was measured.
fn percentile(samples: &[f64], p: usize) -> f64 {
    let mut sorted = samples.to_vec();
    sorted.sort_by(f64::total_cmp);
    let rank = (p * sorted.len()).div_ceil(100).max(1);
    sorted[rank - 1]
}

fn max_of(samples: &[f64]) -> f64 {
    samples.iter().copied().fold(f64::MIN, f64::max)
}

fn measurement(statistic: &str, value: f64, unit: &str) -> Value {
    json!({
        "statistic": statistic,
        "value": (value * 1000.0).round() / 1000.0,
        "unit": unit,
    })
}

/// The case's one stdout line, which the script parses.
fn emit(row: &str, measurements: Vec<Value>, samples: usize, detail: String) {
    println!(
        "{}",
        json!({
            "streaming-row": row,
            "measurements": measurements,
            "samples": samples,
            "detail": detail,
        })
    );
}
