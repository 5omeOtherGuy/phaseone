//! Bounded live smoke checks of the two real routes. They use the owner's existing CLI
//! logins, cost a few hundred tokens each, print no credential and no request body, and
//! live requests do NOTHING unless `P1_LIVE=1` (an env flag alone is not authorization:
//! only the lead runs these). Offline composition regressions run without this flag.
//!
//!   P1_LIVE=1 cargo test -p p1-live -- --nocapture --test-threads 1

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use p1_contracts::{
    AssistantBlock, CancellationToken, CompletedResponse, DeclarationKind, Effort, Grammar, Item,
    ModelOptions, Outcome, Provider, ProviderRequest, StopReason, StreamEvent, ToolDeclaration,
    ToolInput, ToolResultItem, ToolStatus,
};
use p1_provider_http::ReqwestTransport;

fn live() -> bool {
    std::env::var("P1_LIVE").as_deref() == Ok("1")
}

fn model(var: &str, default: &str) -> String {
    std::env::var(var).unwrap_or_else(|_| default.to_string())
}

/// The shipped profile file, so a live check runs the model policy the host runs.
fn profile(id: &str) -> Arc<p1_model_profile::ModelProfile> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../profiles/{id}.toml"));
    let text = std::fs::read_to_string(&path).expect("the shipped profile is readable");
    Arc::new(p1_model_profile::ModelProfile::from_toml(id, &text).expect("valid profile file"))
}

/// `P1_LIVE_EFFORT=low|medium|high` (default low). High makes the models reason, which
/// exercises reasoning replay on the follow-up request.
fn effort() -> Option<Effort> {
    match std::env::var("P1_LIVE_EFFORT").as_deref() {
        Ok("high") => Some(Effort::High),
        Ok("medium") => Some(Effort::Medium),
        _ => Some(Effort::Low),
    }
}

fn read_tool() -> ToolDeclaration {
    ToolDeclaration {
        name: "read".into(),
        description: "Read a file from the repository. Always use this to look at files.".into(),
        kind: DeclarationKind::Function {
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string", "description": "File path"}},
                "required": ["path"]
            }),
        },
    }
}

/// Run one request to its terminal event; print a compact, secret-free trace.
async fn respond(provider: &dyn Provider, request: ProviderRequest) -> CompletedResponse {
    provider.validate(&request).expect("request validates");
    let mut stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("stream starts");
    let (mut text, mut reasoning, mut tool_deltas) = (0usize, 0usize, 0usize);
    loop {
        let event = tokio::time::timeout(Duration::from_secs(180), stream.next())
            .await
            .expect("provider went silent for 180 s")
            .expect("stream ended without a terminal event");
        match event {
            StreamEvent::TextDelta { text: t, .. } => text += t.len(),
            StreamEvent::ReasoningDelta { text: t, .. } => reasoning += t.len(),
            StreamEvent::ToolInputDelta { .. } => tool_deltas += 1,
            // ADR-0048: display-only, but a live probe is exactly where the operator
            // wants to see which transport is in use.
            StreamEvent::Notice { text } => println!("  notice: {text}"),
            StreamEvent::Activity => {}
            StreamEvent::Finished(Outcome::Completed(done)) => {
                println!(
                    "  deltas: text {text} B, reasoning {reasoning} B, tool-input {tool_deltas}; stop {:?}; usage {:?}",
                    done.stop, done.usage
                );
                for block in &done.item.blocks {
                    match block {
                        AssistantBlock::Text { text } => {
                            println!("  text: {:?}", text.chars().take(80).collect::<String>())
                        }
                        AssistantBlock::Reasoning { text, replay } => println!(
                            "  reasoning: {} B shown, replay data: {}",
                            text.len(),
                            replay.as_ref().map_or("none".to_string(), |r| format!(
                                "v{} for {}",
                                r.version, r.origin.model
                            ))
                        ),
                        AssistantBlock::ToolCall(call) => println!(
                            "  call: {} id-len {} input {:?}",
                            call.name,
                            call.call_id.len(),
                            call.input.raw().chars().take(120).collect::<String>()
                        ),
                    }
                }
                return done;
            }
            StreamEvent::Finished(other) => panic!("live request did not complete: {other:?}"),
        }
    }
}

/// Text turn, then a tool-call round trip whose follow-up replays the reasoning.
async fn round_trip(provider: &dyn Provider, tools: Vec<ToolDeclaration>, effort: Option<Effort>) {
    let options = ModelOptions {
        reasoning_effort: effort,
        cache_key: (provider.describe().origin.route == "openai-chat/opencode-go-subscription")
            .then(|| {
                format!(
                    "p1-live-{}-{}",
                    std::process::id(),
                    std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap()
                        .as_nanos()
                )
            }),
        ..ModelOptions::default()
    };
    println!("- text turn");
    let done = respond(
        provider,
        ProviderRequest {
            system_prompt: "You are a terse test assistant.".into(),
            history: vec![Item::User {
                text: "Reply with exactly: pong".into(),
            }],
            tools: Vec::new(),
            options: options.clone(),
        },
    )
    .await;
    assert!(
        done.item.text().to_lowercase().contains("pong"),
        "unexpected text"
    );
    assert!(
        done.usage.is_some(),
        "a completed live response should report usage"
    );

    println!("- tool call");
    let mut history = vec![Item::User {
        text: "What is the secret word in the file notes.txt? Use the read tool.".into(),
    }];
    let first = respond(
        provider,
        ProviderRequest {
            system_prompt: "You are a coding agent. Use your tools.".into(),
            history: history.clone(),
            tools: tools.clone(),
            options: options.clone(),
        },
    )
    .await;
    assert_eq!(first.stop, StopReason::ToolUse);
    let call = first.item.tool_calls().next().expect("a tool call").clone();
    assert_eq!(call.name, "read");
    assert!(matches!(call.input, ToolInput::Json(_)));

    println!("- follow-up with the result (replays reasoning data if any)");
    history.push(Item::Assistant(first.item.clone()));
    history.push(Item::ToolResult(ToolResultItem {
        call_id: call.call_id,
        name: call.name,
        status: ToolStatus::Ok,
        content: "     1\tthe secret word is: marzipan".into(),
    }));
    let second = respond(
        provider,
        ProviderRequest {
            system_prompt: "You are a coding agent. Use your tools.".into(),
            history,
            tools,
            options,
        },
    )
    .await;
    assert!(
        second.item.text().to_lowercase().contains("marzipan"),
        "the model did not use the tool result"
    );
}

#[tokio::test]
async fn claude_subscription_route() {
    if !live() {
        return;
    }
    let wire_model = model("P1_LIVE_CLAUDE_MODEL", "claude-sonnet-5");
    println!("== anthropic-messages/claude-subscription · {wire_model}");
    // The shipped route and profile, exactly as the host composes them.
    let provider = live_route("anthropic-subscription", "claude-sonnet-5", &wire_model);
    round_trip(&*provider, vec![read_tool()], effort()).await;
}

#[tokio::test]
async fn codex_subscription_route() {
    if !live() {
        return;
    }
    let wire_model = model("P1_LIVE_GPT_MODEL", "gpt-5.6-sol");
    println!("== openai-responses/codex-subscription · {wire_model}");
    // The shipped route and profile, exactly as the host composes them.
    let provider = live_route("openai-codex-subscription", "gpt-5.6-sol", &wire_model);
    round_trip(&*provider, vec![read_tool()], effort()).await;
}

/// A connector that delegates to the real one and counts what happened on the wire, so
/// a silent fallback to SSE cannot pass for a WebSocket success.
struct CountingConnector {
    inner: Arc<dyn p1_provider_http::ws::WsConnector>,
    connected: Arc<std::sync::atomic::AtomicUsize>,
    frames: Arc<std::sync::atomic::AtomicUsize>,
}

struct CountingConnection {
    inner: Box<dyn p1_provider_http::ws::WsConnection>,
    frames: Arc<std::sync::atomic::AtomicUsize>,
}

impl p1_provider_http::ws::WsConnector for CountingConnector {
    fn connect<'a>(
        &'a self,
        request: p1_provider_http::ws::WsHandshake,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<Box<dyn p1_provider_http::ws::WsConnection>, p1_provider_http::ws::WsConnectError>,
    > {
        Box::pin(async move {
            let inner = match self.inner.connect(request).await {
                Ok(inner) => inner,
                Err(p1_provider_http::ws::WsConnectError::Status { status, body }) => {
                    // The status is the finding; the body is not printed (it is the server's text).
                    println!(
                        "   websocket upgrade REFUSED with HTTP {status} ({} body bytes)",
                        body.len()
                    );
                    return Err(p1_provider_http::ws::WsConnectError::Status { status, body });
                }
                Err(other) => {
                    println!("   websocket connect FAILED");
                    return Err(other);
                }
            };
            self.connected
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(Box::new(CountingConnection {
                inner,
                frames: self.frames.clone(),
            })
                as Box<dyn p1_provider_http::ws::WsConnection>)
        })
    }
}

impl p1_provider_http::ws::WsConnection for CountingConnection {
    fn send_text<'a>(
        &'a mut self,
        text: String,
    ) -> futures_util::future::BoxFuture<'a, Result<(), p1_provider_http::ws::WsError>> {
        // Sizes and one flag only — never the frame's content.
        println!(
            "   frame sent: {} bytes, continuation: {}",
            text.len(),
            text.contains("\"previous_response_id\"")
        );
        self.inner.send_text(text)
    }

    fn next_text<'a>(
        &'a mut self,
    ) -> futures_util::future::BoxFuture<'a, Result<Option<String>, p1_provider_http::ws::WsError>>
    {
        self.inner.next_text()
    }

    fn next_bounded<'a>(
        &'a mut self,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<p1_provider_http::ws::WsNext, p1_provider_http::ws::WsError>,
    > {
        // The adapter reads through `next_bounded`, so this decorator MUST forward it:
        // delegating to the provided default would drop the read bound issue #164 adds
        // and report an expiry as a close. The frame count moves here with it.
        Box::pin(async move {
            let next = self.inner.next_bounded().await;
            if matches!(next, Ok(p1_provider_http::ws::WsNext::Text(_))) {
                self.frames
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            }
            next
        })
    }
}

/// websocket.md §8: does the SUBSCRIPTION backend accept the WebSocket upgrade, and does a
/// whole tool round trip arrive over it? All requests on one provider instance must reuse
/// the connection (exactly ONE successful connect).
#[tokio::test]
async fn codex_subscription_route_over_websocket() {
    if !live() {
        return;
    }
    let wire_model = model("P1_LIVE_GPT_MODEL", "gpt-5.6-sol");
    println!("== openai-responses/codex-subscription over WebSocket · {wire_model}");
    let connected = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let provider = live_route_with_connector(
        "openai-codex-subscription",
        "gpt-5.6-sol",
        &wire_model,
        Arc::new(CountingConnector {
            inner: Arc::new(p1_provider_http::ws::TungsteniteConnector::new()),
            connected: connected.clone(),
            frames: frames.clone(),
        }),
    );
    round_trip(&*provider, vec![read_tool()], effort()).await;
    assert_websocket_round_trip(&connected, &frames);
}

fn assert_websocket_round_trip(
    connected: &std::sync::atomic::AtomicUsize,
    frames: &std::sync::atomic::AtomicUsize,
) {
    let connects = connected.load(std::sync::atomic::Ordering::SeqCst);
    let received = frames.load(std::sync::atomic::Ordering::SeqCst);
    println!("   websocket connects: {connects} · text frames received: {received}");
    assert_eq!(
        connects, 1,
        "all requests of the round trip must share ONE connection"
    );
    assert!(
        received > 0,
        "the responses must have arrived as WebSocket frames"
    );
}

/// routes.md [todo-live]: does the Codex subscription route accept a freeform/grammar
/// tool, and does the model answer with a `custom_tool_call` carrying raw patch text?
#[tokio::test]
async fn codex_route_accepts_a_freeform_patch_tool() {
    if !live() {
        return;
    }
    let wire_model = model("P1_LIVE_GPT_MODEL", "gpt-5.6-sol");
    println!("== freeform apply_patch on openai-responses/codex-subscription · {wire_model}");
    let provider = live_route("openai-codex-subscription", "gpt-5.6-sol", &wire_model);
    let grammar = "start: begin_patch hunk+ end_patch\nbegin_patch: \"*** Begin Patch\" LF\nend_patch: \"*** End Patch\" LF?\nhunk: add_hunk | delete_hunk | update_hunk\nadd_hunk: \"*** Add File: \" filename LF add_line+\ndelete_hunk: \"*** Delete File: \" filename LF\nupdate_hunk: \"*** Update File: \" filename LF change_move? change?\nfilename: /(.+)/\nadd_line: \"+\" /(.*)/ LF -> line\nchange_move: \"*** Move to: \" filename LF\nchange: (change_context | change_line)+ eof_line?\nchange_context: (\"@@\" | \"@@ \" /(.+)/) LF\nchange_line: (\"+\" | \"-\" | \" \") /(.*)/ LF\neof_line: \"*** End of File\" LF\n%import common.LF\n";
    let patch_tool = ToolDeclaration {
        name: "apply_patch".into(),
        description: "Create, change or delete files with a patch in the V4A format.".into(),
        kind: DeclarationKind::Freeform {
            grammar: Some(Grammar {
                syntax: "lark".into(),
                definition: grammar.into(),
            }),
        },
    };
    let done = respond(
        &*provider,
        ProviderRequest {
            system_prompt: "You are a coding agent. apply_patch is the only way to create files."
                .into(),
            history: vec![Item::User {
                text: "Create the file hello.txt containing the single line: hello".into(),
            }],
            tools: vec![patch_tool],
            options: ModelOptions {
                reasoning_effort: Some(Effort::Low),
                ..ModelOptions::default()
            },
        },
    )
    .await;
    let call = done.item.tool_calls().next().expect("a tool call");
    assert_eq!(call.name, "apply_patch");
    match &call.input {
        ToolInput::Text(raw) => assert!(
            raw.contains("*** Begin Patch") && raw.contains("hello.txt"),
            "{raw:?}"
        ),
        other => panic!("expected freeform text input, got {other:?}"),
    }
}

/// A shipped route through the host's own loading path: `routes::load_route_by_id`
/// reads the file and `catalog::route_provider` builds the provider from the file's
/// data, so a live check runs exactly what the host runs. `wire_model` is the live
/// knob's model, which overrides the file's binding for the run.
///
/// `route_provider` activates the provider COMPONENT the route's adapter names (D083b),
/// from the module set the host itself would read: a live check needs the built modules
/// (`scripts/build-modules.sh --all`; a debug build finds them through S3.8.0's discovery once
/// it is on main). Since S7.10-R5 no route builds a native adapter, a
/// `transport = "websocket"` one included: the component lowers that transport and connects
/// through the injected connector (ADR-0078).
///
/// The connector is injected next to the transport (ADR-0047 §1); a live check gets
/// the REAL one, because the shipped Codex route asks for WebSocket.
fn live_route(route_id: &str, profile_id: &str, wire_model: &str) -> Arc<dyn Provider> {
    live_route_with_connector(
        route_id,
        profile_id,
        wire_model,
        Arc::new(p1_provider_http::ws::TungsteniteConnector::new()),
    )
}

fn live_route_with_connector(
    route_id: &str,
    profile_id: &str,
    wire_model: &str,
    ws: Arc<dyn p1_provider_http::ws::WsConnector>,
) -> Arc<dyn Provider> {
    let dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")];
    let route = p1_host::routes::load_route_by_id(&dirs, route_id).expect("the shipped route file");
    let transport = Arc::new(ReqwestTransport::new());
    let credentials = p1_host::auth::credential_source(&route, transport.clone());
    let components = p1_host::catalog::ProviderComponents::installed()
        .expect("the module set a live check needs");
    route_with_io(
        &components,
        &route,
        profile(profile_id),
        wire_model,
        transport,
        ws,
        credentials,
    )
    .expect("valid route/profile binding")
}

// Share the live composition with offline checks, without reading a real login or opening a socket.
fn route_with_io(
    components: &p1_host::catalog::ProviderComponents,
    route: &p1_host::routes::RouteFile,
    profile: Arc<p1_model_profile::ModelProfile>,
    wire_model: &str,
    transport: Arc<dyn p1_provider_http::Transport>,
    ws: Arc<dyn p1_provider_http::ws::WsConnector>,
    credentials: Arc<dyn p1_provider_http::CredentialSource>,
) -> Result<Arc<dyn Provider>, String> {
    let dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")];
    let mut binding = route.binding(&profile.id)?.clone();
    binding.wire_model = wire_model.to_string();
    p1_host::catalog::route_provider(
        components,
        &dirs,
        route,
        &binding,
        profile,
        transport,
        ws,
        credentials,
    )
}

struct OfflineCredential;

impl p1_provider_http::CredentialSource for OfflineCredential {
    fn access<'a>(
        &'a self,
    ) -> p1_contracts::BoxFuture<
        'a,
        Result<p1_provider_http::Credential, p1_contracts::ProviderError>,
    > {
        Box::pin(async {
            Ok(p1_provider_http::Credential {
                bearer: "offline-placeholder".into(),
                account_id: Some("offline-account".into()),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a p1_provider_http::Credential,
    ) -> p1_contracts::BoxFuture<
        'a,
        Result<p1_provider_http::Credential, p1_contracts::ProviderError>,
    > {
        panic!("offline success must not refresh credentials")
    }
}

#[test]
fn websocket_smoke_requires_a_provider_component() {
    let dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")];
    let route = p1_host::routes::load_route_by_id(&dirs, "openai-codex-subscription").unwrap();
    let result = route_with_io(
        &p1_host::catalog::ProviderComponents::none(),
        &route,
        profile("gpt-5.6-sol"),
        "gpt-5.6-sol",
        Arc::new(p1_provider_http::testing::ScriptedTransport::new(vec![])),
        Arc::new(p1_provider_http::testing::ScriptedWsConnector::new(vec![])),
        Arc::new(OfflineCredential),
    );
    let error = result.err().expect("no native adapter fallback");
    assert!(error.contains("p1/provider-openai"), "{error}");
}

#[tokio::test]
async fn websocket_smoke_component_path_counts_frames_and_reuses_connection() {
    use p1_provider_http::testing::{
        ScriptedConnection, ScriptedFrame, ScriptedResponse, ScriptedTransport, ScriptedWsConnector,
    };

    let dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")];
    let route = p1_host::routes::load_route_by_id(&dirs, "openai-codex-subscription").unwrap();
    let components = p1_host::catalog::ProviderComponents::installed().unwrap();
    let completed = r#"{"type":"response.completed","response":{}}"#;
    let peer = Arc::new(ScriptedWsConnector::new(vec![ScriptedConnection::accept(
        (0..3).map(|_| ScriptedFrame::text(completed)).collect(),
    )]));
    let connected = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    // Successful SSE replies let the counters, not an unavailable HTTP fixture, catch fallback.
    let transport = Arc::new(ScriptedTransport::new(vec![
        ScriptedResponse::ok_sse(
            &format!("data: {completed}\n\n")
        );
        3
    ]));
    let provider = route_with_io(
        &components,
        &route,
        profile("gpt-5.6-sol"),
        "gpt-5.6-sol",
        transport.clone(),
        Arc::new(CountingConnector {
            inner: peer.clone(),
            connected: connected.clone(),
            frames: frames.clone(),
        }),
        Arc::new(OfflineCredential),
    )
    .expect("the live composition activates a component");
    for _ in 0..3 {
        respond(
            &*provider,
            ProviderRequest {
                system_prompt: "offline test".into(),
                history: vec![Item::User { text: "hi".into() }],
                tools: vec![],
                options: ModelOptions::default(),
            },
        )
        .await;
    }
    assert_websocket_round_trip(&connected, &frames);
    assert_eq!(frames.load(std::sync::atomic::Ordering::SeqCst), 3);
    assert_eq!(peer.sent_texts()[0].len(), 3);
    assert!(transport.requests().is_empty(), "no silent SSE fallback");
}

#[tokio::test]
async fn deepseek_subscription_route() {
    if !live() {
        return;
    }
    let provider = live_route(
        "opencode-go-subscription",
        "deepseek-v4.1-flash",
        &model("P1_LIVE_DEEPSEEK_MODEL", "deepseek-v4.1-flash"),
    );
    round_trip(&*provider, vec![read_tool()], Some(Effort::High)).await;
}

#[tokio::test]
async fn glm_subscription_route() {
    if !live() {
        return;
    }
    let provider = live_route(
        "glm-subscription",
        "glm-5.3",
        &model("P1_LIVE_GLM_MODEL", "glm-5.3"),
    );
    round_trip(&*provider, vec![read_tool()], Some(Effort::High)).await;
}
