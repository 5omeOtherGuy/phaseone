//! `p1 acp` against recorded wire fixtures (ADR-0154): the real host behind the ACP
//! adapter, on scripted providers, over an in-memory pipe.
//!
//! A fixture is one JSON object per line, `{"dir": "c2a"|"a2c", "msg": {...}}`. The
//! replay sends the `c2a` lines, collects what the agent writes, and compares the
//! whole transcript after normalising the session id, the workspace path and p1's
//! version.
//! The plan fixture freezes only plan notifications; its test drives the client
//! separately because unrelated background output can interleave.
//! `P1_ACP_RECORD=1` writes the transcript back instead (the `c2a` lines are the
//! script). No network, tempdirs only, no sleeps.

mod common;
#[cfg(feature = "workflows")]
mod workflow_common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Harness, provider_hook, write_environment};
use p1_contracts::{AssistantBlock, CancellationToken, StopReason, StreamEvent, Usage};
use p1_host::frontend::FrontEnd;
use p1_host::frontend_port::PortFrontEnd;
use p1_host::run::run_with_front_end;
use p1_testkit::{
    ScriptedProvider, Step, completed, json_call, text_block, text_response, tool_call_response,
};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SESSION: &str = "<session>";
const WORKSPACE: &str = "<workspace>";
const VERSION: &str = "<version>";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/acp/fixtures/{name}.jsonl"))
}

fn read_fixture(path: &Path) -> Vec<(String, Value)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let entry: Value = serde_json::from_str(line).unwrap();
            (
                entry["dir"].as_str().unwrap().to_string(),
                entry["msg"].clone(),
            )
        })
        .collect()
}

/// Replace `from` by `to` in every string of `value`; an empty `from` (the session id
/// before `session/new` answered) replaces nothing.
fn replace(value: &Value, from: &str, to: &str) -> Value {
    if from.is_empty() {
        return value.clone();
    }
    match value {
        Value::String(text) => Value::String(text.replace(from, to)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| replace(item, from, to)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), replace(item, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some()
}

/// Runs the fixture's client lines against `p1 acp` and returns the transcript, with
/// the agent's lines normalised.
async fn transcript(
    fixture: &[(String, Value)],
    workspace: &Path,
    harness: &mut Harness,
    environment: &str,
) -> Vec<(String, Value)> {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, mut user_write) = tokio::io::split(user);
    let mut lines = BufReader::new(user_read).lines();
    let front_end = Arc::new(PortFrontEnd::new(Arc::new(
        p1_acp::driver::AcpFrontEnd::new(
            Box::new(agent_read),
            Box::new(agent_write),
            workspace.to_path_buf(),
        ),
    )));
    let options = p1_host::cli::parse(&[
        "acp".to_string(),
        p1_host::cli::SERVE_SESSION.to_string(),
        "--env".to_string(),
        environment.to_string(),
        "--workspace".to_string(),
        workspace.to_str().unwrap().to_string(),
    ])
    .unwrap();
    let workspace_text = workspace.to_str().unwrap().to_string();

    let client = async {
        let mut recorded = Vec::new();
        let mut session = String::new();
        let mut waiting: Option<Value> = None;
        for (dir, message) in fixture.iter().filter(|(dir, _)| dir == "c2a") {
            let message = replace(
                &replace(message, WORKSPACE, &workspace_text),
                SESSION,
                &session,
            );
            let mut line = serde_json::to_vec(&message).unwrap();
            line.push(b'\n');
            user_write.write_all(&line).await.unwrap();
            recorded.push((
                dir.clone(),
                replace(
                    &replace(&message, &workspace_text, WORKSPACE),
                    &session,
                    SESSION,
                ),
            ));
            if message.get("method").is_some() && message.get("id").is_some() {
                waiting = Some(message["id"].clone());
            }
            // Read until the outstanding request is answered or the agent asks
            // something the next client line answers.
            while let Some(id) = waiting.clone() {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the agent closed early");
                let received: Value =
                    serde_json::from_str(&line).expect("every stdout line is JSON");
                assert_eq!(received["jsonrpc"], "2.0", "{received}");
                if let Some(id) = received["result"]["sessionId"].as_str() {
                    session = id.to_string();
                }
                let asks = is_request(&received);
                let answered = received.get("method").is_none() && received["id"] == id;
                let mut normal = replace(
                    &replace(&received, &workspace_text, WORKSPACE),
                    &session,
                    SESSION,
                );
                // A release changes p1's version, not the wire.
                if let Some(version) = normal.pointer_mut("/result/agentInfo/version") {
                    *version = json!(VERSION);
                }
                recorded.push(("a2c".to_string(), normal));
                if answered {
                    waiting = None;
                }
                if asks || answered {
                    break;
                }
            }
        }
        user_write.shutdown().await.unwrap();
        while lines.next_line().await.unwrap().is_some() {}
        recorded
    };
    let stderr = &harness.stderr;
    let host = run_with_front_end(
        &mut harness.deps,
        &options,
        CancellationToken::new(),
        front_end as Arc<dyn FrontEnd>,
    );
    let (code, recorded) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::pin!(host, client);
        // A host that ends before the client is done would leave the client waiting.
        tokio::select! {
            code = &mut host => {
                let code = code.unwrap();
                assert_eq!(code, 0, "stderr: {}", stderr.text());
                (code, client.await)
            }
            recorded = &mut client => (host.await.unwrap(), recorded),
        }
    })
    .await
    .unwrap_or_else(|_| panic!("p1 acp hung; stderr: {}", stderr.text()));
    assert_eq!(code, 0, "stderr: {}", stderr.text());
    recorded
}

/// initialize, session/new, one prompt whose tool call is approved `allow_once`, the
/// tool's result, the answer and `end_turn`.
#[tokio::test]
async fn acp_fixture_prompt_tool_approval_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("input.txt"),
        "hello from the fixture\n",
    )
    .unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "read",
            r#"{"file_path":"input.txt"}"#,
        )]),
        text_response("input.txt says hello"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let path = fixture_path("prompt-tool-approval");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness, "plain").await;

    // A client that declared no p1.dev receives no p1 extension.
    let text = serde_json::to_string(
        &actual
            .iter()
            .map(|(_, message)| message)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!text.contains("_p1") && !text.contains("p1.dev"), "{text}");
    // The host's own lines never reach the JSON-RPC stream.
    assert!(
        harness.stdout.text().is_empty(),
        "stdout: {}",
        harness.stdout.text()
    );

    if record(&path, &actual) {
        return;
    }
    assert_eq!(
        handle.requests().len(),
        2,
        "the tool ran and the turn ended"
    );
    compare(&fixture, &actual);
}

/// `P1_ACP_RECORD=1`: write the transcript back as the fixture, and say so.
fn record(path: &Path, actual: &[(String, Value)]) -> bool {
    if std::env::var_os("P1_ACP_RECORD").is_none() {
        return false;
    }
    let lines: Vec<String> = actual
        .iter()
        .map(|(dir, message)| json!({"dir": dir, "msg": message}).to_string())
        .collect();
    std::fs::write(path, lines.join("\n") + "\n").unwrap();
    true
}

fn compare(fixture: &[(String, Value)], actual: &[(String, Value)]) {
    assert_eq!(actual.len(), fixture.len(), "{actual:#?}");
    for (index, (expected, got)) in fixture.iter().zip(actual).enumerate() {
        assert_eq!(expected, got, "fixture line {}", index + 1);
    }
}

// ------------------------------------------------------------------ model switch (#675)

const ENVIRONMENT_ONE: &str = r#"
route   = "route-one"
profile = "p-one"

[[tools]]
module = "read"
"#;

const ENVIRONMENT_TWO: &str = r#"
route   = "route-two"
profile = "p-two"

[[tools]]
module = "read"
"#;

const ROUTE_ONE: &str = r#"
id           = "route-one"
origin_route = "openai-chat/one"
adapter      = "openai-chat"
endpoint     = "https://example.invalid/v1/chat/completions"

[credential]
kind = "api-key"
env  = "ONE_API_KEY"

[adapter_settings]
dialect = "thinking-with-reasoning-alias"

[models."p-one"]
wire_model = "wire-one"
"#;

const ROUTE_TWO: &str = r#"
id           = "route-two"
origin_route = "openai-chat/two"
adapter      = "openai-chat"
endpoint     = "https://example.invalid/v1/chat/completions"

[credential]
kind = "api-key"
env  = "TWO_API_KEY"

[adapter_settings]
dialect = "thinking-with-reasoning-alias"

[models."p-two"]
wire_model = "wire-two"
"#;

const PROFILE_ONE: &str = r#"
id             = "p-one"
revision       = 1
model_id       = "p-one-model"
family         = "temp"
thinking       = "enabled"
efforts        = ["low", "high"]
default_effort = "high"
"#;

const PROFILE_TWO: &str = r#"
id             = "p-two"
revision       = 1
model_id       = "p-two-model"
family         = "temp"
thinking       = "enabled"
efforts        = ["low", "medium"]
default_effort = "medium"
"#;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// `session/new` lists the model table (`e-one/p-one`, `e-two/p-two`) and the
/// running profile's efforts; `session/set_config_option` runs the `/model` and
/// `/effort` switch, answers the whole list and announces it with
/// `config_option_update`; the next prompt goes to the new route at the new effort; a
/// value the list does not hold is invalid params and changes nothing.
#[tokio::test]
async fn acp_fixture_model_switch_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let root = tempfile::tempdir().unwrap();
    let config = tempfile::tempdir().unwrap();
    let base = root.path();
    write(
        &base.join("environments/e-one/environment.toml"),
        ENVIRONMENT_ONE,
    );
    write(&base.join("environments/e-one/prompt.md"), "one\n");
    write(
        &base.join("environments/e-two/environment.toml"),
        ENVIRONMENT_TWO,
    );
    write(&base.join("environments/e-two/prompt.md"), "two\n");
    write(&base.join("routes/route-one.toml"), ROUTE_ONE);
    write(&base.join("routes/route-two.toml"), ROUTE_TWO);
    write(&base.join("profiles/p-one.toml"), PROFILE_ONE);
    write(&base.join("profiles/p-two.toml"), PROFILE_TWO);

    let one = ScriptedProvider::new(vec![text_response("on one")]);
    let two = ScriptedProvider::new(vec![text_response("on two")]);
    let mut harness = Harness::new(vec![base.join("environments")], &[]);
    // No test reads the real home or config directory.
    harness.deps.shell_env = Some(vec![
        ("HOME".into(), base.as_os_str().to_os_string()),
        (
            "XDG_CONFIG_HOME".into(),
            config.path().as_os_str().to_os_string(),
        ),
    ]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("route-one", one.clone()),
        ("route-two", two.clone()),
    ]));

    let path = fixture_path("model-switch");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness, "e-one").await;
    if record(&path, &actual) {
        return;
    }
    assert_eq!(one.requests().len(), 1, "the first turn ran on route-one");
    let requests = two.requests();
    assert_eq!(requests.len(), 1, "the second turn ran on route-two");
    assert_eq!(
        requests[0].options.reasoning_effort,
        Some(p1_contracts::Effort::Low),
        "the effort set after the switch reached the request"
    );
    compare(&fixture, &actual);
}

/// Every committed response, including a tool-use response, reports parent usage
/// before its prompt answers. Unknown usage never manufactures an update or cost.
#[tokio::test]
async fn acp_fixture_usage_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("input.txt"),
        "hello from the fixture\n",
    )
    .unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "PROMPT",
    );
    let path = environments.path().join("plain/environment.toml");
    let mut environment = std::fs::read_to_string(&path).unwrap();
    environment.push_str("\n[context]\nwindow_tokens = 100000\noutput_headroom_tokens = 1000\nsummarize_at_tokens = 90000\nkeep_recent_tokens = 10000\nuser_verbatim_tokens = 1000\n");
    std::fs::write(path, environment).unwrap();
    let provider = ScriptedProvider::new(vec![
        Step::Events(vec![StreamEvent::Finished(completed(
            vec![AssistantBlock::ToolCall(json_call(
                "c1",
                "read",
                r#"{"file_path":"input.txt"}"#,
            ))],
            StopReason::ToolUse,
            Some(Usage {
                input_uncached: Some(41),
                cache_read: Some(70),
                cache_write: Some(13),
                output: Some(900),
                reasoning_output: Some(300),
                cost_micro_usd: Some(1250),
            }),
        ))]),
        Step::Events(vec![
            StreamEvent::TextDelta {
                block: 0,
                text: "input.txt says hello".into(),
            },
            StreamEvent::Finished(completed(
                vec![text_block("input.txt says hello")],
                StopReason::EndTurn,
                Some(Usage {
                    input_uncached: Some(9),
                    cache_read: Some(2),
                    cache_write: Some(4),
                    output: Some(800),
                    reasoning_output: Some(300),
                    cost_micro_usd: Some(2000),
                }),
            )),
        ]),
        text_response("usage unavailable"),
        Step::Events(vec![
            StreamEvent::TextDelta {
                block: 0,
                text: "usage resumed".into(),
            },
            StreamEvent::Finished(completed(
                vec![text_block("usage resumed")],
                StopReason::EndTurn,
                Some(Usage {
                    input_uncached: Some(17),
                    cost_micro_usd: Some(4000),
                    ..Usage::default()
                }),
            )),
        ]),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let path = fixture_path("usage");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness, "plain").await;
    assert_eq!(handle.requests().len(), 4);
    assert!(harness.stdout.text().is_empty());
    if std::env::var_os("P1_ACP_RECORD").is_some() {
        let lines: Vec<_> = actual
            .iter()
            .map(|(dir, msg)| json!({"dir":dir,"msg":msg}).to_string())
            .collect();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    } else {
        assert_eq!(actual, fixture);
    }
}

/// Only plan notifications are frozen here: unrelated parent output and worker
/// approvals may interleave, but workflow step order and full snapshots may not.
#[cfg(feature = "workflows")]
#[tokio::test]
async fn acp_fixture_plan_replays() {
    use workflow_common::{Fakes, Scratch, done};

    let scratch = Scratch::new();
    std::fs::rename(
        scratch.root.path().join("environments/parent"),
        scratch.root.path().join("environments/plain"),
    )
    .unwrap();
    let script = r#"
let a = agent("first task", #{ label: "Prepare" });
let b = agent("second task", #{ label: "Review" });
[a.value, b.value]
"#;
    let fakes = Fakes::new(
        vec![
            tool_call_response(vec![json_call("c1", "workflow_start",
                &json!({"script":script}).to_string())]),
            text_response("started"),
            text_response("workflow settled"),
        ],
        [done("prepared"), vec![tool_call_response(vec![json_call("f1", "finish",
            r#"{"status":"blocked","summary":"review needs input","needs":"a review checklist"}"#,
        )])]].concat(),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let mut client: Vec<_> = [
        json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":1,"clientCapabilities":{}}}),
        json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd":WORKSPACE,"mcpServers":[]}}),
        json!({"jsonrpc":"2.0","id":2,"method":"session/prompt",
            "params":{"sessionId":SESSION,"prompt":[{"type":"text","text":"run the workflow"}]}}),
    ]
    .into_iter()
    .map(|msg| ("c2a".to_string(), msg))
    .collect();
    // Parent workflow_start, then the two workers' finish calls.
    for id in 0..3 {
        client.push((
            "c2a".into(),
            json!({"jsonrpc":"2.0","id":id,
            "result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}),
        ));
    }
    let actual = transcript(&client, scratch.workspace.path(), &mut harness, "plain").await;
    let prompt_reply = actual
        .iter()
        .position(|(dir, msg)| dir == "a2c" && msg.get("method").is_none() && msg["id"] == 2)
        .unwrap();
    let mut plans = Vec::new();
    for (index, (dir, msg)) in actual.iter().enumerate() {
        if dir == "a2c" && msg["params"]["update"]["sessionUpdate"] == "plan" {
            assert!(index < prompt_reply, "plan must precede prompt reply");
            plans.push((dir.clone(), msg.clone()));
        }
    }
    assert_eq!(fakes.main.requests().len(), 2);
    assert!(harness.stdout.text().is_empty());
    assert_eq!(
        plans.last().unwrap().1["params"]["update"],
        json!({
            "sessionUpdate":"plan","entries":[
                {"content":"wf1/1: Prepare","priority":"medium","status":"completed"},
                {"content":"wf1/2: Review (blocked)","priority":"medium","status":"pending"}
            ]
        })
    );
    let path = fixture_path("plan");
    if std::env::var_os("P1_ACP_RECORD").is_some() {
        let lines: Vec<_> = plans
            .iter()
            .map(|(dir, msg)| json!({"dir":dir,"msg":msg}).to_string())
            .collect();
        std::fs::write(path, lines.join("\n") + "\n").unwrap();
    } else {
        assert_eq!(plans, read_fixture(&path));
    }
}
