//! `_p1/agent_state` (#683) through the host's port bridge and the real ACP driver: a
//! fake two-step workflow run started by a prompt. A client that declares the
//! capability gets the prompt's answer at the turn's end, the run's updates after it
//! and the inbox turn about the run with its two state transitions; any other client
//! is held to the end (D4). The declaring transcript is the literal fixture
//! `docs/acp/fixtures/agent-state-p1dev.jsonl`; `P1_ACP_RECORD=1` re-records it.
//! No network, tempdirs only, no sleeps.
//!
//! The last case runs the real host on scripted providers: a real two-step workflow
//! run, whose inbox turn waits on the agent's own lock between prompts.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{FrontEndPort, SessionHandle};
use p1_contracts::{AgentEvent, BoxFuture, CancellationToken, StopReason, TurnEnd};
use p1_host::frontend::{
    FrontEnd, WorkflowRunEnded, WorkflowRunStarted, WorkflowStepEnded, WorkflowStepStarted,
};
use p1_host::frontend_port::PortFrontEnd;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::Notify;

const DONE: TurnEnd = TurnEnd::Completed {
    stop: StopReason::EndTurn,
};

/// The session: its prompt starts run `wf1`; the test then runs its two steps and
/// ends it, and the run's notice makes the parent's inbox turn.
struct Session {
    host: PortFrontEnd,
    port: Arc<AcpFrontEnd>,
    inbox: Mutex<VecDeque<String>>,
    arrived: Notify,
}

impl Session {
    fn say(&self, text: &str) {
        self.port.event_sink().emit(AgentEvent::TextDelta {
            text: text.to_string(),
        });
    }

    fn step(&self, ordinal: u32, label: &str, worker: &str) {
        self.host.child_started(worker);
        self.host.workflow_step_started(&WorkflowStepStarted {
            run: "wf1".into(),
            ordinal,
            call: format!("call-{ordinal}"),
            label: Some(label.into()),
            phase: None,
            role: "worker".into(),
            model: "fake/main".into(),
            worker_id: Some(worker.into()),
            attempt: 1,
            prompt: format!("{label} task"),
        });
        self.host.workflow_step_ended(&WorkflowStepEnded {
            run: "wf1".into(),
            ordinal,
            call: format!("call-{ordinal}"),
            label: Some(label.into()),
            model: "fake/main".into(),
            status: "done".into(),
            attempts: 1,
            replayed: false,
            error: None,
            worker_id: Some(worker.into()),
        });
    }

    /// Both steps, then the run's end: a run queues its notice before its end signal.
    fn run_to_end(&self) {
        self.step(1, "Prepare", "w1");
        self.step(2, "Review", "w2");
        self.inbox.lock().unwrap().push_back("wf1 ended".into());
        self.arrived.notify_one();
        self.host.workflow_run_ended(&WorkflowRunEnded {
            id: "wf1".into(),
            outcome: "completed".into(),
            error: None,
        });
    }
}

impl SessionHandle for Session {
    fn prompt<'a>(&'a self, _text: String, _cancel: CancellationToken) -> BoxFuture<'a, TurnEnd> {
        Box::pin(async move {
            self.host.workflow_run_started(&WorkflowRunStarted {
                id: "wf1".into(),
                resumed_from: None,
            });
            self.say("started wf1");
            DONE
        })
    }
    fn cancel_runs<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn stop_workers<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn drain_inbox<'a>(&'a self, _cancel: CancellationToken) -> BoxFuture<'a, Option<TurnEnd>> {
        Box::pin(async move {
            let notices: Vec<String> = self.inbox.lock().unwrap().drain(..).collect();
            if notices.is_empty() {
                return None;
            }
            self.say(&format!("summary of {}", notices.join(", ")));
            Some(DONE)
        })
    }
    fn inbox_ready<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            while self.inbox.lock().unwrap().is_empty() {
                self.arrived.notified().await;
            }
        })
    }
}

/// One client run; the transcript with the session id and workspace normalised.
async fn exercise(capabilities: Value) -> Vec<(String, Value)> {
    let workspace = tempfile::tempdir().unwrap();
    let (agent, client) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(agent);
    let port = Arc::new(AcpFrontEnd::new(
        Box::new(read),
        Box::new(write),
        workspace.path().to_path_buf(),
    ));
    let session = Session {
        host: PortFrontEnd::new(port.clone()),
        port: port.clone(),
        inbox: Mutex::new(VecDeque::new()),
        arrived: Notify::new(),
    };
    let declaring = capabilities.to_string().contains("agent_state");
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    let client = async {
        let mut transcript = Vec::new();
        let mut id = String::new();
        let mut next = async |transcript: &mut Vec<(String, Value)>| {
            let message: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            assert_eq!(message["jsonrpc"], "2.0");
            transcript.push(("a2c".to_string(), message.clone()));
            message
        };
        for (number, method, params) in [
            (
                0,
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":capabilities}),
            ),
            (
                1,
                "session/new",
                json!({"cwd":"<workspace>","mcpServers":[]}),
            ),
            (
                2,
                "session/prompt",
                json!({"sessionId":"<session>","prompt":[{"type":"text","text":"run the work"}]}),
            ),
        ] {
            let request = json!({"jsonrpc":"2.0","id":number,"method":method,"params":params});
            transcript.push(("c2a".to_string(), request.clone()));
            let line = request
                .to_string()
                .replace("<workspace>", workspace.path().to_str().unwrap())
                .replace("<session>", &id);
            write
                .write_all(format!("{line}\n").as_bytes())
                .await
                .unwrap();
            if number == 2 {
                break;
            }
            loop {
                let message = next(&mut transcript).await;
                if message.get("method").is_none() && message["id"] == number {
                    if number == 1 {
                        id = message["result"]["sessionId"].as_str().unwrap().into();
                    }
                    break;
                }
            }
        }
        // The prompt's turn is over once its text arrived.
        while texts(&next(&mut transcript).await) != ["started wf1"] {}
        if declaring {
            // Answered at the turn's end, while the run is still live.
            loop {
                let message = next(&mut transcript).await;
                if message.get("method").is_none() && message["id"] == 2 {
                    break;
                }
            }
        }
        session.run_to_end();
        let mut inbox_turn = false;
        loop {
            let message = next(&mut transcript).await;
            inbox_turn |= texts(&message) == ["summary of wf1 ended"];
            let answered = message.get("method").is_none() && message["id"] == 2;
            let idle =
                message["method"] == "_p1/agent_state" && message["params"]["state"] == "idle";
            if if declaring {
                inbox_turn && idle
            } else {
                answered
            } {
                assert!(
                    inbox_turn,
                    "the inbox turn precedes the answer of a held prompt"
                );
                break;
            }
        }
        write.shutdown().await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            transcript.push(("a2c".to_string(), serde_json::from_str(&line).unwrap()));
        }
        normalise(transcript, &id, workspace.path().to_str().unwrap())
    };
    let (code, transcript) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(port.run(&session), client)
    })
    .await
    .expect("p1 acp hung");
    assert_eq!(code, 0);
    transcript
}

fn texts(message: &Value) -> Vec<&str> {
    let update = &message["params"]["update"];
    if update["sessionUpdate"] == "agent_message_chunk" {
        vec![update["content"]["text"].as_str().unwrap()]
    } else {
        Vec::new()
    }
}

fn normalise(
    transcript: Vec<(String, Value)>,
    session: &str,
    workspace: &str,
) -> Vec<(String, Value)> {
    transcript
        .into_iter()
        .map(|(dir, mut message)| {
            if let Some(version) = message.pointer_mut("/result/agentInfo/version") {
                *version = json!("<version>");
            }
            let text = message
                .to_string()
                .replace(session, "<session>")
                .replace(workspace, "<workspace>");
            (dir, serde_json::from_str(&text).unwrap())
        })
        .collect()
}

fn position(transcript: &[(String, Value)], found: impl Fn(&Value) -> bool) -> usize {
    transcript
        .iter()
        .position(|(dir, message)| dir == "a2c" && found(message))
        .unwrap()
}

#[tokio::test]
async fn acp_agent_state_streams_a_two_step_run_after_the_answer() {
    let transcript =
        exercise(json!({"_meta":{"p1.dev":{"version":1,"capabilities":["agent_state"]}}})).await;
    let answer = position(&transcript, |m| m.get("method").is_none() && m["id"] == 2);
    let summary = position(&transcript, |m| texts(m) == ["summary of wf1 ended"]);
    let plans: Vec<usize> = transcript
        .iter()
        .enumerate()
        .filter(|(_, (_, m))| m["params"]["update"]["sessionUpdate"] == "plan")
        .map(|(index, _)| index)
        .collect();
    assert!(
        plans.len() >= 4 && plans.iter().all(|index| *index > answer),
        "both steps stream after the answer: {plans:?}, answer at {answer}"
    );
    let states: Vec<(usize, String)> = transcript
        .iter()
        .enumerate()
        .filter(|(_, (_, m))| m["method"] == "_p1/agent_state")
        .map(|(index, (_, m))| {
            let params = &m["params"];
            (
                index,
                format!(
                    "{} {}",
                    params["state"].as_str().unwrap(),
                    params["stopReason"].as_str().unwrap_or("-")
                ),
            )
        })
        .collect();
    let after: Vec<&str> = states
        .iter()
        .filter(|(index, _)| *index > answer)
        .map(|(_, state)| state.as_str())
        .collect();
    assert_eq!(after, ["running -", "idle end_turn"], "{states:?}");
    assert!(
        states
            .iter()
            .filter(|(index, _)| *index > answer)
            .all(|(index, state)| { (state == "running -") == (*index < summary) })
    );
    let path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../docs/acp/fixtures/agent-state-p1dev.jsonl");
    if std::env::var_os("P1_ACP_RECORD").is_some() {
        let lines: Vec<String> = transcript
            .iter()
            .map(|(dir, msg)| json!({"dir": dir, "msg": msg}).to_string())
            .collect();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        return;
    }
    let expected: Vec<(String, Value)> = std::fs::read_to_string(&path)
        .unwrap()
        .lines()
        .map(|line| {
            let entry: Value = serde_json::from_str(line).unwrap();
            (
                entry["dir"].as_str().unwrap().to_string(),
                entry["msg"].clone(),
            )
        })
        .collect();
    assert_eq!(transcript.len(), expected.len(), "{transcript:#?}");
    for (index, (expected, got)) in expected.iter().zip(&transcript).enumerate() {
        assert_eq!(expected, got, "fixture line {}", index + 1);
    }
}

#[tokio::test]
async fn acp_agent_state_absent_holds_the_prompt_to_the_end_of_the_run() {
    for capabilities in [
        json!({}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["workflow_update"]}}}),
    ] {
        let transcript = exercise(capabilities).await;
        let answer = position(&transcript, |m| m.get("method").is_none() && m["id"] == 2);
        let summary = position(&transcript, |m| texts(m) == ["summary of wf1 ended"]);
        assert!(
            summary < answer,
            "the prompt is held through the inbox turn"
        );
        let wire = serde_json::to_string(&transcript).unwrap();
        assert!(!wire.contains("_p1/agent_state") && !wire.contains("agent_state"));
    }
}

/// The real host: the prompt starts a two-step workflow and answers at its turn's end;
/// the steps' permission requests and the parent's inbox turn about the run follow,
/// between prompts, with their state transitions.
#[tokio::test]
async fn acp_agent_state_real_host_runs_the_inbox_turn_between_prompts() {
    use p1_testkit::{json_call, text_response, tool_call_response};
    use workflow_common::{Fakes, Scratch, done};
    let scratch = Scratch::new();
    std::fs::rename(
        scratch.root.path().join("environments/parent"),
        scratch.root.path().join("environments/plain"),
    )
    .unwrap();
    let script = r#"let a = agent("first task", #{ label: "Prepare" });
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
        ],
        [done("prepared"), done("reviewed")].concat(),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let workspace = scratch.workspace.path();
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, mut user_write) = tokio::io::split(user);
    let mut lines = BufReader::new(user_read).lines();
    let front_end = Arc::new(PortFrontEnd::new(Arc::new(AcpFrontEnd::new(
        Box::new(agent_read),
        Box::new(agent_write),
        workspace.to_path_buf(),
    ))));
    let options = p1_host::cli::parse(&[
        "acp".to_string(),
        p1_host::cli::SERVE_SESSION.to_string(),
        "--env".to_string(),
        "plain".to_string(),
        "--workspace".to_string(),
        workspace.to_str().unwrap().to_string(),
    ])
    .unwrap();
    let client = async {
        let mut send = async |message: Value| {
            user_write
                .write_all(format!("{message}\n").as_bytes())
                .await
                .unwrap();
        };
        send(json!({"jsonrpc":"2.0","id":0,"method":"initialize","params":{"protocolVersion":1,
            "clientCapabilities":{"_meta":{"p1.dev":{"version":1,"capabilities":["agent_state"]}}}}}))
        .await;
        send(json!({"jsonrpc":"2.0","id":1,"method":"session/new",
            "params":{"cwd":workspace,"mcpServers":[]}}))
        .await;
        let mut seen = Vec::new();
        let mut settled = false;
        loop {
            let message: Value =
                serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
            if message["id"] == 1 && message.get("method").is_none() {
                let id = message["result"]["sessionId"].clone();
                send(json!({"jsonrpc":"2.0","id":2,"method":"session/prompt",
                    "params":{"sessionId":id,"prompt":[{"type":"text","text":"run work"}]}}))
                .await;
            } else if message["method"] == "session/request_permission" {
                send(json!({"jsonrpc":"2.0","id":message["id"],
                    "result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
                .await;
            }
            let shape = if message["method"] == "_p1/agent_state" {
                format!("state {}", message["params"]["state"].as_str().unwrap())
            } else if message.get("method").is_none() && message["id"] == 2 {
                "answer".to_string()
            } else if texts(&message) == ["workflow settled"] {
                settled = true;
                "settled".to_string()
            } else {
                continue;
            };
            let idle = shape == "state idle";
            seen.push(shape);
            if settled && idle {
                break;
            }
        }
        user_write.shutdown().await.unwrap();
        while lines.next_line().await.unwrap().is_some() {}
        seen
    };
    let stderr = &harness.stderr;
    let host = p1_host::run::run_with_front_end(
        &mut harness.deps,
        &options,
        CancellationToken::new(),
        front_end as Arc<dyn FrontEnd>,
    );
    let (code, seen) = tokio::time::timeout(std::time::Duration::from_secs(60), async {
        tokio::join!(host, client)
    })
    .await
    .unwrap_or_else(|_| panic!("p1 acp hung; stderr: {}", stderr.text()));
    assert_eq!(code.unwrap(), 0, "stderr: {}", stderr.text());
    let answer = seen.iter().position(|shape| shape == "answer").unwrap();
    let settled = seen.iter().position(|shape| shape == "settled").unwrap();
    assert!(answer < settled, "{seen:?}");
    // The answer follows its turn's idle; the inbox turn is running, then idle.
    assert_eq!(seen[answer - 1], "state idle", "{seen:?}");
    assert_eq!(seen[settled - 1], "state running", "{seen:?}");
    assert_eq!(seen[settled + 1..], ["state idle"], "{seen:?}");
    assert_eq!(fakes.main.requests().len(), 2);
}
