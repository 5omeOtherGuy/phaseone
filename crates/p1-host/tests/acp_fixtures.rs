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

#[tokio::test]
async fn acp_fixture_question_replays() {
    let path = fixture_path("question");
    let fixture = read_fixture(&path);
    // Each case uses the real host and question module. No invitation for the
    // headless case: its refusal must still precede the invitation check.
    for case in [
        "accept",
        "decline",
        "cancel",
        "unsupported",
        "turn_cancel",
        "uninvited",
        "empty",
        "null",
        "url_only",
    ] {
        let workspace = tempfile::tempdir().unwrap();
        let environments = tempfile::tempdir().unwrap();
        write_environment(
            environments.path(),
            "plain",
            "fake",
            "fake-model",
            &["ask_user_question"],
            "PROMPT",
        );
        let provider = ScriptedProvider::new(vec![
            tool_call_response(vec![json_call(
                "q1",
                "ask_user_question",
                r#"{"questions":[{"question":"Which approach?","header":"Approach","options":[{"label":"A","description":"First","preview":"Preview A"},{"label":"B","description":"Second"}]}]}"#,
            )]),
            text_response("answer received"),
        ]);
        let handle = provider.clone();
        let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
        harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
        let mut script = fixture.clone();
        let unsupported = matches!(case, "unsupported" | "empty" | "null" | "url_only");
        if unsupported || case == "uninvited" {
            script.retain(|(dir, msg)| {
                dir != "c2a" || msg.get("result").is_none_or(|r| r.get("action").is_none())
            });
            let init = script
                .iter_mut()
                .find(|(dir, msg)| dir == "c2a" && msg["method"] == "initialize")
                .unwrap();
            if unsupported {
                init.1["params"]["clientCapabilities"] = match case {
                    "empty" => json!({"elicitation":{}}),
                    "null" => json!({"elicitation":{"form":null}}),
                    "url_only" => json!({"elicitation":{"url":{}}}),
                    _ => json!({}),
                };
            }
            let prompt = script
                .iter_mut()
                .find(|(dir, msg)| dir == "c2a" && msg["method"] == "session/prompt")
                .unwrap();
            prompt.1["params"]["prompt"][0]["text"] = json!("choose an approach");
        } else if case != "accept" {
            let response = script
                .iter_mut()
                .find(|(dir, msg)| dir == "c2a" && msg["result"].get("action").is_some())
                .unwrap();
            response.1 = if case == "turn_cancel" {
                json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":SESSION}})
            } else {
                json!({"jsonrpc":"2.0","id":1,"result":{"action":case}})
            };
        }
        let actual = transcript(&script, workspace.path(), &mut harness, "plain").await;
        let requests = actual
            .iter()
            .filter(|(_, m)| m["method"] == "elicitation/create")
            .count();
        assert_eq!(
            requests,
            usize::from(!unsupported && case != "uninvited"),
            "{case}"
        );
        let wire = serde_json::to_string(&actual).unwrap();
        assert!(!wire.contains("_p1") && !wire.contains("p1.dev"));
        assert!(harness.stdout.text().is_empty());
        if case == "turn_cancel" {
            assert!(
                actual
                    .iter()
                    .any(|(_, m)| m["result"]["stopReason"] == "cancelled")
            );
        } else {
            assert_eq!(handle.requests().len(), 2, "{case}");
            let expected = if unsupported {
                "no interactive user"
            } else if case == "uninvited" {
                "only available after the user"
            } else if case == "accept" {
                "Approach: B; with tests"
            } else {
                "cancelled — no answer"
            };
            assert!(wire.contains(expected), "{case}: {wire}");
        }
        if case == "accept" && !record(&path, &actual) {
            compare(&fixture, &actual);
        }
    }
}

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
    transcript_with_journal(fixture, workspace, harness, environment, None, true).await
}

async fn transcript_with_journal(
    fixture: &[(String, Value)],
    workspace: &Path,
    harness: &mut Harness,
    environment: &str,
    journal: Option<&Path>,
    ask: bool,
) -> Vec<(String, Value)> {
    transcript_from(fixture, workspace, harness, environment, journal, false, ask).await
}

/// The transcript of a session that advertises `promptCapabilities.image` when
/// `image_prompts` (#694).
async fn transcript_from(
    fixture: &[(String, Value)],
    workspace: &Path,
    harness: &mut Harness,
    environment: &str,
    journal: Option<&Path>,
    image_prompts: bool,
    ask: bool,
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
        )
        .with_image_prompts(image_prompts),
    )));
    let mut options = p1_host::cli::parse(&[
        "acp".to_string(),
        p1_host::cli::SERVE_SESSION.to_string(),
        "--env".to_string(),
        environment.to_string(),
        "--workspace".to_string(),
        workspace.to_str().unwrap().to_string(),
    ])
    .unwrap();
    // Approval fixtures opt in now that ACP, like the CLI, defaults to full access.
    options.ask = ask;
    options.session = journal.map(Path::to_path_buf);
    options.resume = journal.is_some();
    let workspace_text = workspace.to_str().unwrap().to_string();

    let client = async {
        let mut recorded = Vec::new();
        let mut session = String::new();
        let mut waiting: Vec<Value> = Vec::new();
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
                waiting.push(message["id"].clone());
            }
            // Read until the outstanding request is answered or the agent asks
            // something the next client line answers.
            while let Some(id) = waiting.last().cloned() {
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
                // Load replays before answering; learn its inner id from the first
                // update so that history has the same normalization as live output.
                if session.is_empty()
                    && let Some(id) = received["params"]["sessionId"].as_str()
                {
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
                    waiting.pop();
                }
                if asks || answered {
                    break;
                }
            }
        }
        user_write.shutdown().await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let received: Value = serde_json::from_str(&line).unwrap();
            assert_eq!(received["jsonrpc"], "2.0");
            recorded.push((
                "a2c".to_string(),
                replace(
                    &replace(&received, &workspace_text, WORKSPACE),
                    &session,
                    SESSION,
                ),
            ));
        }
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

/// A parked permission is an explicit mid-turn boundary. Steering must answer
/// without waiting for it, and reach the next model request, not a separate turn.
#[tokio::test]
async fn acp_fixture_steering_replays() {
    use p1_contracts::{InboxKind, Item};
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    std::fs::write(workspace.path().join("input.txt"), "input\n").unwrap();
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
        text_response("revised plan applied"),
        text_response("idle steer applied"),
        text_response("barrier done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let path = fixture_path("steering");
    let actual = transcript(
        &read_fixture(&path),
        workspace.path(),
        &mut harness,
        "plain",
    )
    .await;
    let response = |id| {
        actual
            .iter()
            .find(|(dir, msg)| dir == "a2c" && msg.get("method").is_none() && msg["id"] == id)
            .unwrap()
            .1
            .clone()
    };
    assert_eq!(
        response(0)["result"]["_meta"],
        json!({"steering":{"supported":true}})
    );
    assert_eq!(response(3)["result"], json!({"outcome":"injected"}));
    assert_eq!(
        response(4)["result"],
        json!({"outcome":"promptRequired","reason":"noRunningTurn"})
    );
    assert_eq!(response(5)["error"]["code"], -32602);
    assert_eq!(response(6)["error"]["code"], -32602);
    assert_eq!(response(7)["result"], json!({"outcome":"startedNewTurn"}));
    assert_eq!(
        actual
            .iter()
            .filter(|(dir, msg)| dir == "a2c" && msg.get("method").is_none() && msg["id"] == 7)
            .count(),
        1
    );
    let requests = handle.requests();
    assert_eq!(requests.len(), 4);
    assert!(requests[1].history.iter().any(|item| matches!(item, Item::Inbox { kind: InboxKind::Steering, text } if text == "use the revised plan")));
    assert!(
        requests[2]
            .history
            .iter()
            .any(|item| matches!(item, Item::User { text } if text == "idle steer"))
    );
    assert!(!requests.iter().flat_map(|request| &request.history).any(|item| matches!(item, Item::User { text } | Item::Inbox { text, .. } if text == "client owns this" || text == "invalid option")));
    assert!(harness.stdout.text().is_empty());
    if !record(&path, &actual) {
        assert_eq!(actual, read_fixture(&path));
    }
}

/// A real background worker remains pending until cancel. Seeing the parent's
/// completed response before steering exercises the hold, not just tool execution.
#[cfg(feature = "workflows")]
#[tokio::test]
async fn acp_steering_keeps_a_held_prompt() {
    use p1_contracts::frontend::{BackgroundSignal, FrontEndPort, SessionHandle};
    use p1_contracts::{AgentEvent, AuthorizationPolicy, BoxFuture, EventSink};
    use tokio::sync::Notify;
    use workflow_common::{Fakes, Scratch};

    struct FinishedSink {
        inner: Arc<dyn EventSink>,
        finished: Arc<Notify>,
    }
    impl EventSink for FinishedSink {
        fn emit(&self, event: AgentEvent) {
            if matches!(event, AgentEvent::TurnFinished { .. }) {
                self.finished.notify_one();
            }
            self.inner.emit(event);
        }
    }
    struct ObservedPort {
        front: p1_acp::driver::AcpFrontEnd,
        finished: Arc<Notify>,
    }
    impl FrontEndPort for ObservedPort {
        fn event_sink(&self) -> Arc<dyn EventSink> {
            Arc::new(FinishedSink {
                inner: self.front.event_sink(),
                finished: self.finished.clone(),
            })
        }
        fn child_event_sink(&self, worker: &str) -> Arc<dyn EventSink> {
            self.front.child_event_sink(worker)
        }
        fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
            self.front.authorization()
        }
        fn background(&self, signal: BackgroundSignal) {
            self.front.background(signal);
        }
        fn run<'a>(&'a self, session: &'a dyn SessionHandle) -> BoxFuture<'a, i32> {
            self.front.run(session)
        }
    }
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        vec![
            tool_call_response(vec![json_call(
                "start",
                "worker_start",
                r#"{"environment":"fake","tools":["read"],"task":"wait"}"#,
            )]),
            text_response("parent held"),
            text_response("steering while held"),
        ],
        vec![Step::EventsThenAwaitCancel(Vec::new())],
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (ar, aw) = tokio::io::split(agent);
    let (ur, mut uw) = tokio::io::split(user);
    let mut lines = BufReader::new(ur).lines();
    let finished = Arc::new(Notify::new());
    let front = Arc::new(PortFrontEnd::new(Arc::new(ObservedPort {
        front: p1_acp::driver::AcpFrontEnd::new(
            Box::new(ar),
            Box::new(aw),
            scratch.workspace.path().to_path_buf(),
        ),
        finished: finished.clone(),
    })));
    let options = p1_host::cli::parse(&[
        "acp".into(),
        p1_host::cli::SERVE_SESSION.into(),
        "--env".into(),
        "parent".into(),
        "--workspace".into(),
        scratch.workspace.path().to_str().unwrap().into(),
    ])
    .unwrap();
    let client = async {
        macro_rules! send {
            ($msg:expr) => {{
                let mut bytes = serde_json::to_vec(&$msg).unwrap();
                bytes.push(b'\n');
                uw.write_all(&bytes).await.unwrap();
            }};
        }
        macro_rules! next {
            () => {
                serde_json::from_str::<Value>(&lines.next_line().await.unwrap().unwrap()).unwrap()
            };
        }
        send!(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}));
        assert_eq!(next!()["id"], 0);
        send!(
            json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":scratch.workspace.path()}})
        );
        let session = next!()["result"]["sessionId"].clone();
        send!(
            json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":session,"prompt":[{"type":"text","text":"start work"}]}})
        );
        loop {
            let msg = next!();
            if msg["method"] == "session/request_permission" {
                send!(
                    json!({"jsonrpc":"2.0","id":msg["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}})
                );
            }
            if msg.pointer("/params/update/content/text") == Some(&json!("parent held")) {
                break;
            }
        }
        // TurnFinished is emitted as the host prompt returns. In the same poll
        // the driver drains the empty inbox and enters its background hold wait.
        finished.notified().await;
        send!(
            json!({"jsonrpc":"2.0","id":3,"method":"_session/steering","params":{"sessionId":session,"prompt":[{"type":"text","text":"revise while waiting"}]}})
        );
        let mut injected = false;
        loop {
            let msg = next!();
            assert!(
                !(msg.get("method").is_none() && msg["id"] == 2),
                "steering released the hold: {msg}"
            );
            if msg.get("method").is_none() && msg["id"] == 3 {
                assert_eq!(msg["result"], json!({"outcome":"injected"}));
                injected = true;
            }
            if msg.pointer("/params/update/content/text") == Some(&json!("steering while held")) {
                break;
            }
        }
        assert!(injected);
        send!(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":session}}));
        loop {
            let msg = next!();
            if msg.get("method").is_none() && msg["id"] == 2 {
                assert_eq!(msg["result"]["stopReason"], "cancelled");
                break;
            }
        }
        uw.shutdown().await.unwrap();
        while lines.next_line().await.unwrap().is_some() {}
    };
    let host = run_with_front_end(&mut harness.deps, &options, CancellationToken::new(), front);
    let (code, ()) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(host, client)
    })
    .await
    .expect("held steering hung");
    assert_eq!(code.unwrap(), 0, "{}", harness.stderr.text());
    assert!(fakes.parent.requests()[2].history.iter().any(|item| matches!(item, p1_contracts::Item::Inbox { kind: p1_contracts::InboxKind::Steering, text } if text == "revise while waiting")));
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

/// The real todo component emits standard plans through the neutral front-end hook.
#[tokio::test]
async fn acp_fixture_todo_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["todo_write"],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "todo_write",
            r#"{"todos":[{"content":"Investigate","status":"completed","priority":"low"},{"content":"Implement","status":"in_progress","priority":"high"},{"content":"Verify","status":"pending","priority":"medium"}]}"#,
        )]),
        tool_call_response(vec![json_call("c2", "todo_write", r#"{"todos":[]}"#)]),
        text_response("Plan cleared"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let path = fixture_path("todo-plan");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness, "plain").await;
    let plans: Vec<_> = actual
        .iter()
        .filter_map(|(_, message)| {
            let update = &message["params"]["update"];
            (update["sessionUpdate"] == "plan").then(|| update.clone())
        })
        .collect();
    assert_eq!(
        plans,
        vec![
            json!({"sessionUpdate":"plan","entries":[
            {"content":"Investigate","status":"completed","priority":"low"},
            {"content":"Implement","status":"in_progress","priority":"high"},
            {"content":"Verify","status":"pending","priority":"medium"}]}),
            json!({"sessionUpdate":"plan","entries":[]})
        ]
    );
    assert_eq!(handle.requests().len(), 3);
    if !record(&path, &actual) {
        compare(&fixture, &actual);
    }
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

#[tokio::test]
async fn acp_fixture_run_cancel_p1dev_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &[],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let path = fixture_path("run-cancel-p1dev");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness, "plain").await;
    for id in [2, 3] {
        let answer = actual
            .iter()
            .find(|(_, m)| m["id"] == id && m.get("method").is_none())
            .unwrap();
        assert_eq!(
            answer.1["result"],
            if id == 2 {
                json!({"cancelled":false})
            } else {
                json!({"stopped":false})
            }
        );
    }
    assert!(harness.stdout.text().is_empty());
    if !record(&path, &actual) {
        compare(&fixture, &actual);
    }
}

#[tokio::test]
async fn acp_fixture_session_title_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &[],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![
        text_response("First answer"),
        text_response("Second answer"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let path = fixture_path("session-title");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness, "plain").await;
    let titles: Vec<_> = actual
        .iter()
        .filter(|(_, m)| m["params"]["update"]["sessionUpdate"] == "session_info_update")
        .collect();
    assert_eq!(titles.len(), 1);
    assert_eq!(titles[0].1["params"]["update"]["title"], "Fix the parser");
    let answer = actual
        .iter()
        .position(|(_, m)| m["id"] == 2 && m.get("result").is_some())
        .unwrap();
    // The title is the turn's last update before its answer (ACP-CANCEL-002).
    let last_output = actual[..answer]
        .iter()
        .rev()
        .find(|(dir, _)| dir == "a2c")
        .unwrap();
    assert_eq!(last_output.1, titles[0].1);
    assert_eq!(handle.requests().len(), 2, "no title model call");
    assert!(harness.stdout.text().is_empty());
    if !record(&path, &actual) {
        compare(&fixture, &actual);
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

/// The scratch host tree of the switch fixtures: `e-one` (route-one, `p-one`) and
/// `e-two` (route-two, `p-two`), a workspace that is its own repository root, and a
/// harness that reads no real home or config directory.
struct Switchable {
    workspace: tempfile::TempDir,
    root: tempfile::TempDir,
    config: tempfile::TempDir,
}

impl Switchable {
    fn new() -> Self {
        let switchable = Self {
            workspace: tempfile::tempdir().unwrap(),
            root: tempfile::tempdir().unwrap(),
            config: tempfile::tempdir().unwrap(),
        };
        let base = switchable.root.path();
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
        // Skill discovery walks up to the repository root: this one stops at the
        // workspace.
        std::fs::create_dir(switchable.workspace.path().join(".git")).unwrap();
        switchable
    }

    fn harness(&self, one: &ScriptedProvider, two: &ScriptedProvider) -> Harness {
        let base = self.root.path();
        let mut harness = Harness::new(vec![base.join("environments")], &[]);
        harness.deps.shell_env = Some(vec![
            ("HOME".into(), base.as_os_str().to_os_string()),
            (
                "XDG_CONFIG_HOME".into(),
                self.config.path().as_os_str().to_os_string(),
            ),
        ]);
        harness.deps.catalog_hook = Some(provider_hook(vec![
            ("route-one", one.clone()),
            ("route-two", two.clone()),
        ]));
        harness
    }
}

/// `session/new` lists the model table (`e-one/p-one`, `e-two/p-two`) and the
/// running profile's efforts; `session/set_config_option` runs the `/model` and
/// `/effort` switch, answers the whole list and announces it with
/// `config_option_update`; the next prompt goes to the new route at the new effort; a
/// value the list does not hold is invalid params and changes nothing.
#[tokio::test]
async fn acp_fixture_model_switch_replays() {
    let scratch = Switchable::new();
    let one = ScriptedProvider::new(vec![text_response("on one")]);
    let two = ScriptedProvider::new(vec![text_response("on two")]);
    let mut harness = scratch.harness(&one, &two);
    let workspace = &scratch.workspace;

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

/// Every command over the real host (#676): `session/new` is followed by the command
/// list, with the environment's skill; `/status`, `/access`, `/compact` and
/// `/modules reload` report as text and end the turn without the model; `/model` and
/// `/effort` list and switch through the config-option path, which republishes the
/// list (`e-two` has no skill tool); the skill is a turn for the model, and so is an
/// unknown `/x`.
#[tokio::test]
async fn acp_fixture_slash_commands_replays() {
    let scratch = Switchable::new();
    let environment = scratch
        .root
        .path()
        .join("environments/e-one/environment.toml");
    write(
        &environment,
        &format!("{ENVIRONMENT_ONE}\n[[tools]]\nmodule = \"skill\"\n"),
    );
    write(
        &scratch
            .workspace
            .path()
            .join(".agents/skills/review/SKILL.md"),
        "---\nname: review\ndescription: Review a change for bugs\n---\nRead the diff.\n",
    );
    let one = ScriptedProvider::new(vec![text_response("reviewed"), text_response("on one")]);
    let two = ScriptedProvider::new(vec![text_response("on two")]);
    let mut harness = scratch.harness(&one, &two);

    let path = fixture_path("slash-commands");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, scratch.workspace.path(), &mut harness, "e-one").await;
    if record(&path, &actual) {
        return;
    }
    let requests = one.requests();
    assert_eq!(requests.len(), 2, "the skill and `/x` reached the model");
    let asked = |request: &p1_contracts::ProviderRequest| format!("{:?}", request.history);
    assert!(
        asked(&requests[0]).contains("Load the skill `review` with the `skill` tool"),
        "{}",
        asked(&requests[0])
    );
    assert!(asked(&requests[1]).contains("/x hello"));
    let requests = two.requests();
    assert_eq!(
        requests.len(),
        1,
        "the prompt after `/model` ran on route-two"
    );
    assert_eq!(
        requests[0].options.reasoning_effort,
        Some(p1_contracts::Effort::Low)
    );
    compare(&fixture, &actual);
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

/// Freeze initiating cards only; background permission traffic can interleave.
/// The literal start results pin the workflow/delegation wording used to link ids.
#[cfg(feature = "workflows")]
#[tokio::test]
async fn acp_fixture_workflow_run_replays() {
    workflow_run_replay(false).await;
    workflow_run_replay(true).await;
}

#[cfg(feature = "workflows")]
async fn workflow_run_replay(worker_transcripts: bool) {
    use workflow_common::{Fakes, Scratch, done};
    let scratch = Scratch::new();
    std::fs::rename(
        scratch.root.path().join("environments/parent"),
        scratch.root.path().join("environments/plain"),
    )
    .unwrap();
    let script = r#"phase("prepare"); log("begin");
let a = agent("first task", #{ label: "Prepare" });
let b = agent("second task", #{ label: "Review" });
[a.value, b.value]"#;
    let fakes = Fakes::new(
        vec![
            tool_call_response(vec![json_call(
                "c1",
                "workflow_start",
                &json!({"script":script}).to_string(),
            )]),
            text_response("started"),
            text_response("workflow settled"),
            tool_call_response(vec![json_call(
                "c2",
                "worker_start",
                r#"{"environment":"fake","tools":["read"],"task":"direct task"}"#,
            )]),
            text_response("worker started"),
            text_response("worker settled"),
        ],
        [done("prepared"), done("reviewed"), done("direct done")].concat(),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let mut client: Vec<_> = [
        json!({"jsonrpc":"2.0","id":0,"method":"initialize",
            "params":{"protocolVersion":1,"clientCapabilities": if worker_transcripts {
                json!({"_meta":{"p1.dev":{"version":1,"capabilities":["worker_transcripts"]}}})
            } else { json!({}) }}}),
        json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd":WORKSPACE,"mcpServers":[]}}),
    ]
    .into_iter()
    .map(|msg| ("c2a".to_string(), msg))
    .collect();
    // Separate held prompts keep worker ids deterministic: two workflow steps,
    // then the direct worker. Each prompt waits for its background inbox turn.
    for (prompt_id, approvals) in [(2, 0..3), (3, 3..5)] {
        client.push((
            "c2a".into(),
            json!({"jsonrpc":"2.0","id":prompt_id,"method":"session/prompt",
            "params":{"sessionId":SESSION,"prompt":[{"type":"text","text":"run work"}]}}),
        ));
        for id in approvals {
            client.push((
                "c2a".into(),
                json!({"jsonrpc":"2.0","id":id,
                "result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}),
            ));
        }
    }
    let actual = transcript(&client, scratch.workspace.path(), &mut harness, "plain").await;
    let mut lifecycle = Vec::new();
    let mut worker_calls = Vec::new();
    for (index, (dir, msg)) in actual.iter().enumerate() {
        if dir != "a2c" {
            continue;
        }
        let stamp = &msg["params"]["update"]["_meta"]["p1.dev"]["worker"];
        let worker = if msg["method"] == "_p1/worker_update" {
            let worker = msg["params"]["worker"].as_str().unwrap();
            lifecycle.push((
                worker.to_string(),
                msg["params"]["state"].as_str().unwrap().to_string(),
            ));
            Some(worker)
        } else if !stamp.is_null() {
            // Catalog hooks replace the configured provider with ScriptedProvider.
            assert_eq!(stamp["route"], "fake-route");
            assert_eq!(stamp["model"], "fake-model");
            if msg["params"]["update"]["sessionUpdate"] == "tool_call" {
                worker_calls.push(stamp["id"].as_str().unwrap().to_string());
            }
            stamp["id"].as_str()
        } else {
            None
        };
        if let Some(worker) = worker {
            assert!(worker_transcripts, "worker activity leaked: {msg}");
            let reply = actual
                .iter()
                .position(|(dir, msg)| {
                    dir == "a2c"
                        && msg.get("method").is_none()
                        && msg["id"] == if worker == "w3" { 3 } else { 2 }
                })
                .unwrap();
            assert!(index < reply, "worker update must precede prompt reply");
        }
    }
    if worker_transcripts {
        assert_eq!(
            lifecycle,
            [
                ("w1", "started"),
                ("w1", "ended"),
                ("w2", "started"),
                ("w2", "ended"),
                ("w3", "started"),
                ("w3", "ended")
            ]
            .map(|(id, state)| (id.to_string(), state.to_string()))
        );
        assert_eq!(worker_calls, ["w1", "w2", "w3"]);
    } else {
        assert!(lifecycle.is_empty() && worker_calls.is_empty());
    }
    let mut cards = Vec::new();
    for (index, (dir, msg)) in actual.iter().enumerate() {
        let update = &msg["params"]["update"];
        let id = update["toolCallId"].as_str().unwrap_or_default();
        if dir == "a2c"
            && update["sessionUpdate"] == "tool_call_update"
            && ["c1", "c2"].contains(&id)
        {
            let reply = actual
                .iter()
                .position(|(dir, msg)| {
                    dir == "a2c"
                        && msg.get("method").is_none()
                        && msg["id"] == if id == "c1" { 2 } else { 3 }
                })
                .unwrap();
            assert!(index < reply, "card update must precede prompt reply");
            cards.push((dir.clone(), msg.clone()));
        }
    }
    let workflow: Vec<_> = cards
        .iter()
        .map(|(_, msg)| &msg["params"]["update"])
        .filter(|update| update["toolCallId"] == "c1" && update.get("content").is_some())
        .collect();
    let start = "Started workflow wf1. You will be notified when it ends; do not poll.";
    assert_eq!(workflow[0]["content"][0]["content"]["text"], start);
    for pair in workflow.windows(2) {
        assert_eq!(pair[0]["status"], "in_progress");
        assert!(
            pair[1]["content"][0]["content"]["text"]
                .as_str()
                .unwrap()
                .starts_with(pair[0]["content"][0]["content"]["text"].as_str().unwrap())
        );
    }
    let last = workflow.last().unwrap();
    assert_eq!(last["status"], "completed");
    let text = last["content"][0]["content"]["text"].as_str().unwrap();
    assert!(
        text.contains("phase: prepare") && text.contains("workflow wf1: begin"),
        "{text}"
    );
    assert!(text.find("Prepare").unwrap() < text.find("Review").unwrap());
    let worker: Vec<_> = cards
        .iter()
        .map(|(_, msg)| &msg["params"]["update"])
        .filter(|update| update["toolCallId"] == "c2" && update.get("content").is_some())
        .collect();
    assert!(
        worker[0]["content"][0]["content"]["text"]
            .as_str()
            .unwrap()
            .starts_with("Started worker w3 on ")
    );
    let note = worker.last().unwrap();
    assert!(
        note.get("status").is_none(),
        "notes must not reopen the completed start tool"
    );
    // worker_end_note reports grants and finish state, not the finish summary.
    let text = note["content"][0]["content"]["text"].as_str().unwrap();
    assert!(
        text.contains("\nworker w3 (") && text.contains(") done"),
        "{text}"
    );
    assert_eq!(fakes.main.requests().len(), 3);
    assert!(harness.stdout.text().is_empty());
    let path = fixture_path("workflow-run");
    if !record(&path, &cards) {
        assert_eq!(cards, read_fixture(&path));
    }
}

/// A journal recorded by the real host, then loaded without executing its tools.
#[tokio::test]
async fn acp_fixture_session_load_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    let state = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "PROMPT",
    );
    std::fs::write(workspace.path().join("input.txt"), "hello from journal\n").unwrap();
    let journal = state.path().join("journal.jsonl");
    let recorded_journal = fixture_path("session-journal");
    if std::env::var_os("P1_ACP_RECORD").is_some() {
        let provider = ScriptedProvider::new(vec![
            tool_call_response(vec![json_call(
                "old-call",
                "read",
                r#"{"file_path":"input.txt"}"#,
            )]),
            text_response("recorded answer"),
        ]);
        let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
        harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
        let options = p1_host::cli::parse(&[
            "--env".into(),
            "plain".into(),
            "--workspace".into(),
            workspace.path().to_str().unwrap().into(),
            "--session".into(),
            journal.to_str().unwrap().into(),
            "read input.txt".into(),
        ])
        .unwrap();
        assert_eq!(p1_host::run::run(&mut harness.deps, options).await, 0);
        let text = std::fs::read_to_string(&journal).unwrap();
        // Synthetic fixture content only; normalize genuinely unstable metadata.
        let lines = text
            .lines()
            .map(|line| {
                let mut value: Value = serde_json::from_str(line).unwrap();
                if value.get("at_ms").is_some() {
                    value["at_ms"] = json!(1);
                }
                if value["record"] == "request_timing" {
                    for key in ["sent_ms", "first_event_ms", "first_output_ms", "ended_ms"] {
                        if !value[key].is_null() {
                            value[key] = json!(1);
                        }
                    }
                }
                if value.get("assembly").is_some() {
                    value["assembly"]["host"]["commit"] = json!("fixture");
                }
                replace(&value, workspace.path().to_str().unwrap(), WORKSPACE).to_string()
            })
            .collect::<Vec<_>>()
            .join("\n")
            + "\n";
        std::fs::write(&recorded_journal, lines).unwrap();
    }
    std::fs::write(
        &journal,
        std::fs::read_to_string(&recorded_journal)
            .unwrap()
            .replace(WORKSPACE, workspace.path().to_str().unwrap()),
    )
    .unwrap();
    // If replay accidentally dispatched read, it would fail and change the fixture.
    std::fs::remove_file(workspace.path().join("input.txt")).unwrap();
    let provider = ScriptedProvider::new(vec![text_response("continued answer")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let path = fixture_path("session-load");
    let fixture = read_fixture(&path);
    let actual = transcript_with_journal(
        &fixture,
        workspace.path(),
        &mut harness,
        "plain",
        Some(&journal),
        true,
    )
    .await;
    let replay = actual
        .iter()
        .filter(|(_, msg)| msg["method"] == "session/update")
        .map(|(_, msg)| &msg["params"]["update"])
        .collect::<Vec<_>>();
    assert_eq!(replay[0]["sessionUpdate"], "user_message_chunk");
    assert_eq!(replay[0]["content"]["text"], "read input.txt");
    assert_eq!(replay[1]["toolCallId"], "old-call");
    assert_eq!(replay[2]["status"], "completed");
    assert_eq!(replay[3]["content"]["text"], "recorded answer");
    assert_eq!(handle.requests().len(), 1);
    assert!(handle.requests()[0].history.iter().any(
        |item| matches!(item, p1_contracts::Item::Assistant(a) if a.text() == "recorded answer")
    ));
    if !record(&path, &actual) {
        compare(&fixture, &actual);
    }
    // Resume the recorded nonempty history but do not stream it back. A blank
    // fake session would not distinguish load from resume here.
    let mut resumed_script = fixture.clone();
    for (_, msg) in &mut resumed_script {
        if msg["method"] == "session/load" {
            msg["method"] = json!("session/resume");
        }
    }
    let provider = ScriptedProvider::new(vec![text_response("resumed answer")]);
    let handle = provider.clone();
    let mut resumed_host = Harness::new(vec![environments.path().to_path_buf()], &[]);
    resumed_host.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let resumed = transcript_with_journal(
        &resumed_script,
        workspace.path(),
        &mut resumed_host,
        "plain",
        Some(&journal),
        true,
    )
    .await;
    assert!(
        resumed
            .iter()
            .filter(|(_, msg)| msg["method"] == "session/update")
            .all(|(_, msg)| {
                !matches!(
                    msg["params"]["update"]["sessionUpdate"].as_str(),
                    Some("user_message_chunk" | "tool_call" | "tool_call_update")
                )
            })
    );
    assert!(handle.requests()[0].history.iter().any(
        |item| matches!(item, p1_contracts::Item::Assistant(a) if a.text() == "recorded answer")
    ));
    // Load the open session again on the same connection (TCK ACP-LOAD-001..003): it
    // re-attaches, and the replay is the journal's, the turn this process committed
    // included, then the command list, before the answer; nothing follows the answer
    // (ACP-LOAD-002). The journal starts from the recording again.
    std::fs::write(
        &journal,
        std::fs::read_to_string(&recorded_journal)
            .unwrap()
            .replace(WORKSPACE, workspace.path().to_str().unwrap()),
    )
    .unwrap();
    let mut reloaded_script = fixture.clone();
    reloaded_script.push((
        "c2a".to_string(),
        json!({"jsonrpc":"2.0","id":3,"method":"session/load",
            "params":{"cwd":WORKSPACE,"mcpServers":[],"sessionId":SESSION}}),
    ));
    let provider = ScriptedProvider::new(vec![text_response("continued answer")]);
    let mut reloaded_host = Harness::new(vec![environments.path().to_path_buf()], &[]);
    reloaded_host.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let reloaded = transcript_with_journal(
        &reloaded_script,
        workspace.path(),
        &mut reloaded_host,
        "plain",
        Some(&journal),
        true,
    )
    .await;
    let second = reloaded
        .iter()
        .position(|(_, msg)| msg["id"] == 3 && msg["method"] == "session/load")
        .unwrap();
    let answer = reloaded
        .iter()
        .position(|(_, msg)| msg["id"] == 3 && msg.get("method").is_none())
        .unwrap();
    assert!(reloaded[answer].1.get("error").is_none(), "{reloaded:#?}");
    let replay = reloaded[second + 1..answer]
        .iter()
        .map(|(_, msg)| {
            let update = &msg["params"]["update"];
            format!(
                "{} {}",
                update["sessionUpdate"].as_str().unwrap(),
                update["content"]["text"]
                    .as_str()
                    .or(update["toolCallId"].as_str())
                    .unwrap_or_default()
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        replay,
        [
            "user_message_chunk read input.txt",
            "tool_call old-call",
            "tool_call_update old-call",
            "agent_message_chunk recorded answer",
            "user_message_chunk continue",
            "agent_message_chunk continued answer",
            "available_commands_update ",
        ]
    );
    // The transcript runs to the host's exit: the answer is its last line.
    assert_eq!(answer, reloaded.len() - 1, "{:#?}", &reloaded[answer..]);
}

/// The permission mode over the real host (#696): `session/new` offers it as the
/// `mode` option and as the legacy modes. In `read-only` a read runs and a write is
/// refused without asking; in `full-access` the write runs unasked; back in `ask` the
/// write asks, and the approval decides it. Each change is answered first, then
/// announced with `config_option_update` and `current_mode_update`.
#[tokio::test]
async fn acp_fixture_modes_replays() {
    let scratch = Switchable::new();
    write(
        &scratch
            .root
            .path()
            .join("environments/e-one/environment.toml"),
        &format!("{ENVIRONMENT_ONE}\n[[tools]]\nmodule = \"write\"\n"),
    );
    std::fs::write(scratch.workspace.path().join("input.txt"), "hello\n").unwrap();
    let write_call = |id: &str, text: &str| {
        json_call(
            id,
            "write",
            &json!({"file_path":"out.txt","content":text}).to_string(),
        )
    };
    let one = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("r1", "read", r#"{"file_path":"input.txt"}"#),
            write_call("w1", "read-only"),
        ]),
        text_response("refused"),
        tool_call_response(vec![write_call("w2", "full-access")]),
        text_response("wrote"),
        tool_call_response(vec![write_call("w3", "ask")]),
        text_response("asked"),
    ]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&one, &two);

    let path = fixture_path("modes");
    let fixture = read_fixture(&path);
    let actual = transcript_with_journal(
        &fixture,
        scratch.workspace.path(),
        &mut harness,
        "e-one",
        None,
        false,
    )
    .await;
    if record(&path, &actual) {
        return;
    }
    assert_eq!(
        one.requests().len(),
        6,
        "three turns, each a call and an answer"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.workspace.path().join("out.txt")).unwrap(),
        "ask",
        "read-only refused its write; full-access and the approved ask wrote"
    );
    compare(&fixture, &actual);
}

#[tokio::test]
async fn acp_defaults_to_full_access_and_writes_without_permission_requests() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["write"],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "w1",
            "write",
            r#"{"file_path":"out.txt","content":"unasked"}"#,
        )]),
        text_response("wrote without asking"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let script = vec![
        (
            "c2a".into(),
            json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1}}),
        ),
        (
            "c2a".into(),
            json!({"jsonrpc":"2.0","id":1,"method":"session/new","params":{"cwd":WORKSPACE,"mcpServers":[]}}),
        ),
        (
            "c2a".into(),
            json!({"jsonrpc":"2.0","id":2,"method":"session/prompt","params":{"sessionId":SESSION,"prompt":[{"type":"text","text":"write out.txt"}]}}),
        ),
    ];
    let actual = transcript_with_journal(
        &script,
        workspace.path(),
        &mut harness,
        "plain",
        None,
        false,
    )
    .await;
    let session = actual
        .iter()
        .find(|(dir, msg)| dir == "a2c" && msg["id"] == 1)
        .unwrap();
    assert_eq!(session.1["result"]["modes"]["currentModeId"], "full-access");
    let mode = session.1["result"]["configOptions"]
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "mode")
        .unwrap();
    assert_eq!(mode["currentValue"], "full-access");
    assert!(
        !actual
            .iter()
            .any(|(_, msg)| msg["method"] == "session/request_permission")
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "unasked"
    );
    assert!(
        actual
            .iter()
            .any(|(_, msg)| msg["id"] == 2 && msg["result"]["stopReason"] == "end_turn")
    );
}

/// A 2x3 PNG with a text chunk (zero CRCs: the store cuts chunks, it never checks them),
/// and the same PNG as the store keeps it, without the text chunk.
fn generated_png() -> (Vec<u8>, Vec<u8>) {
    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut out = (data.len() as u32).to_be_bytes().to_vec();
        out.extend_from_slice(kind);
        out.extend_from_slice(data);
        out.extend_from_slice(&[0; 4]);
        out
    }
    let head = [
        b"\x89PNG\r\n\x1a\n".to_vec(),
        chunk(b"IHDR", &[0, 0, 0, 2, 0, 0, 0, 3, 8, 6, 0, 0, 0]),
    ]
    .concat();
    let tail = [chunk(b"IDAT", &[1, 2, 3]), chunk(b"IEND", &[])].concat();
    let sent = [
        head.clone(),
        chunk(b"tEXt", b"Author\0someone"),
        tail.clone(),
    ]
    .concat();
    (sent, [head, tail].concat())
}

/// ACP image prompts over the real host (#694). On a route whose model takes images,
/// `initialize` advertises `promptCapabilities.image`, the image goes into the store
/// (its text chunk cut), history and request hold its reference only, and a
/// `session/load` replays the stored image as image content. On a text-only route,
/// and for bytes that are no image, the prompt is refused with invalid params before
/// any turn: nothing reaches the provider and nothing is stored.
#[tokio::test]
async fn acp_fixture_image_prompt_replays() {
    let path = fixture_path("image-prompt");
    let fixture = read_fixture(&path);
    let (sent, stored) = generated_png();
    let prompt = fixture
        .iter()
        .find(|(dir, msg)| dir == "c2a" && msg["method"] == "session/prompt")
        .unwrap();
    assert_eq!(
        prompt.1["params"]["prompt"][1]["data"],
        p1_contracts::ImageData::encode(&sent).as_base64(),
        "the fixture's image is the generated one"
    );
    for case in ["accepted", "text_route", "not_an_image"] {
        let workspace = tempfile::tempdir().unwrap();
        let environments = tempfile::tempdir().unwrap();
        let state = tempfile::tempdir().unwrap();
        write_environment(
            environments.path(),
            "plain",
            "fake",
            "fake-model",
            &[],
            "PROMPT",
        );
        let provider = ScriptedProvider::new(vec![text_response("a red pixel")]);
        let provider = if case == "text_route" {
            provider
        } else {
            provider.taking_images()
        };
        let handle = provider.clone();
        let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
        harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
        let root = state.path().join("attachments");
        harness.deps.attachments = Some(Arc::new(p1_attachment_local::LocalAttachmentStore::new(
            &root,
        )));
        let mut script = fixture.clone();
        if case == "not_an_image" {
            let prompt = script
                .iter_mut()
                .find(|(dir, msg)| dir == "c2a" && msg["method"] == "session/prompt")
                .unwrap();
            prompt.1["params"]["prompt"][1]["data"] =
                json!(p1_contracts::ImageData::encode(b"hello").as_base64());
        }
        let actual = transcript_from(
            &script,
            workspace.path(),
            &mut harness,
            "plain",
            None,
            case != "text_route",
            false,
        )
        .await;
        let answer = |id: u64| {
            actual
                .iter()
                .find(|(dir, msg)| dir == "a2c" && msg["id"] == id && msg.get("method").is_none())
                .map(|(_, msg)| msg.clone())
                .unwrap()
        };
        assert_eq!(
            answer(0)["result"]["agentCapabilities"]["promptCapabilities"]["image"],
            case != "text_route",
            "{case}"
        );
        let objects = || {
            std::fs::read_dir(root.join("objects"))
                .map(|entries| entries.map(|entry| entry.unwrap().path()).collect())
                .unwrap_or_else(|_| Vec::new())
        };
        if case != "accepted" {
            let refused = answer(2);
            assert_eq!(refused["error"]["code"], -32602, "{case}: {refused}");
            let message = refused["error"]["message"].as_str().unwrap();
            let expected = if case == "text_route" {
                "does not take image input"
            } else {
                "not a supported image"
            };
            assert!(message.contains(expected), "{case}: {message}");
            assert!(handle.requests().is_empty(), "{case}");
            assert!(objects().is_empty(), "{case}");
            continue;
        }
        assert_eq!(answer(2)["result"]["stopReason"], "end_turn");
        // The store holds the image without its text chunk, named by its content.
        let objects: Vec<PathBuf> = objects();
        assert_eq!(objects.len(), 1);
        assert_eq!(std::fs::read(&objects[0]).unwrap(), stored);
        // The request carries the reference; the host resolves bytes per request.
        let requests = handle.requests();
        let [p1_contracts::Item::UserContent { blocks }] = requests[0].history.as_slice() else {
            panic!("{:?}", requests[0].history);
        };
        let [
            p1_contracts::UserBlock::Text { text },
            p1_contracts::UserBlock::Image { image, .. },
        ] = blocks.as_slice()
        else {
            panic!("{blocks:?}");
        };
        assert_eq!(text, "what is in this image?");
        assert_eq!(
            (image.media_type, image.width, image.height, image.bytes),
            (p1_contracts::ImageType::Png, 2, 3, stored.len() as u64)
        );
        assert_eq!(
            objects[0].file_name().unwrap().to_str().unwrap(),
            image.id.as_str()
        );
        // The re-attaching load replays the turn, the image as the stored bytes.
        let load = actual
            .iter()
            .position(|(dir, msg)| dir == "c2a" && msg["method"] == "session/load")
            .unwrap();
        let replay: Vec<&Value> = actual[load..]
            .iter()
            .filter(|(_, msg)| msg["method"] == "session/update")
            .map(|(_, msg)| &msg["params"]["update"]["content"])
            .collect();
        assert_eq!(
            replay[0],
            &json!({"type":"text","text":"what is in this image?"})
        );
        assert_eq!(
            replay[1],
            &json!({"type":"image","mimeType":"image/png",
                "data": p1_contracts::ImageData::encode(&stored).as_base64()})
        );
        assert_eq!(replay[2], &json!({"type":"text","text":"a red pixel"}));
        if !record(&path, &actual) {
            compare(&fixture, &actual);
        }
    }
}
