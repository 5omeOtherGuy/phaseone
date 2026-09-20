//! Bounded live smoke checks of the two real routes. They use the owner's existing CLI
//! logins, cost a few hundred tokens each, print no credential and no request body, and
//! do NOTHING unless `P1_LIVE=1` (an env flag alone is not authorization: only the lead
//! runs these). Not part of the gate.
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
fn live_route(route_id: &str, profile_id: &str, wire_model: &str) -> Arc<dyn Provider> {
    let dirs = vec![PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")];
    let route = p1_host::routes::load_route_by_id(&dirs, route_id).expect("the shipped route file");
    let profile = profile(profile_id);
    let mut binding = route
        .binding(&profile.id)
        .expect("the route serves this profile")
        .clone();
    binding.wire_model = wire_model.to_string();
    let credentials = p1_host::auth::credential_source(&route, Arc::new(ReqwestTransport::new()));
    p1_host::catalog::route_provider(
        &route,
        &binding,
        profile,
        Arc::new(ReqwestTransport::new()),
        credentials,
    )
    .expect("valid route/profile binding")
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
