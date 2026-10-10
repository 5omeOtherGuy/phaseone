//! All eight existing host seam callbacks traverse the neutral port and the real
//! ACP driver. Literal fixture pins fields independently of the conversion code.
#![cfg(feature = "workflows")]

use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{FrontEndPort, SessionHandle};
use p1_contracts::{BoxFuture, CancellationToken, StopReason, TurnEnd};
use p1_host::frontend::{
    FrontEnd, WorkflowRunEnded, WorkflowRunStarted, WorkflowStepEnded, WorkflowStepStarted,
};
use p1_host::frontend_port::PortFrontEnd;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Session(PortFrontEnd);

impl SessionHandle for Session {
    fn prompt<'a>(&'a self, _text: String, _cancel: CancellationToken) -> BoxFuture<'a, TurnEnd> {
        Box::pin(async move {
            self.0.workflow_run_started(&WorkflowRunStarted {
                id: "wf7".into(),
                resumed_from: Some("wf2".into()),
            });
            self.0.workflow_phase("wf7", "Review");
            self.0.workflow_log("wf7", "checking\nsecond line");
            self.0.workflow_jobs_queued("wf7", 3);
            self.0.workflow_step_started(&WorkflowStepStarted {
                run: "wf7".into(),
                ordinal: 2,
                call: "call-a".into(),
                label: None,
                phase: Some("Review".into()),
                role: "reviewer".into(),
                model: "env/deep:high".into(),
                worker_id: Some("w9".into()),
                attempt: 4,
                prompt: "public script task".into(),
            });
            self.0.workflow_step_ended(&WorkflowStepEnded {
                run: "wf7".into(),
                ordinal: 2,
                call: "call-a".into(),
                label: Some("Inspect".into()),
                model: "env/deep:high".into(),
                status: "blocked".into(),
                attempts: 4,
                replayed: false,
                error: Some("needs checklist".into()),
                worker_id: Some("w9".into()),
            });
            self.0.workflow_thunk_failed("wf7", "invalid item");
            self.0.workflow_run_ended(&WorkflowRunEnded {
                id: "wf7".into(),
                outcome: "completed_with_issues".into(),
                error: None,
            });
            TurnEnd::Completed {
                stop: StopReason::EndTurn,
            }
        })
    }
    fn cancel_runs<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn stop_workers<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async {})
    }
    fn drain_inbox<'a>(&'a self, _cancel: CancellationToken) -> BoxFuture<'a, Option<TurnEnd>> {
        Box::pin(async { None })
    }
    fn inbox_ready<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(std::future::pending())
    }
}

async fn exercise(capabilities: Value, enabled: bool) {
    let workspace = tempfile::tempdir().unwrap();
    let (agent, client) = tokio::io::duplex(1 << 16);
    let (read, write) = tokio::io::split(agent);
    let front = Arc::new(AcpFrontEnd::new(
        Box::new(read),
        Box::new(write),
        workspace.path().to_path_buf(),
    ));
    let session = Session(PortFrontEnd::new(front.clone()));
    let (read, mut write) = tokio::io::split(client);
    let mut lines = BufReader::new(read).lines();
    let client = async {
        let mut id = String::new();
        let mut extensions = Vec::new();
        let mut plans = Vec::new();
        for (number, method, params) in [
            (
                0,
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":capabilities}),
            ),
            (
                1,
                "session/new",
                json!({"cwd":workspace.path(),"mcpServers":[]}),
            ),
            (
                2,
                "session/prompt",
                json!({"sessionId":"", "prompt":[{"type":"text","text":"observe"}]}),
            ),
        ] {
            let mut params = params;
            if number == 2 {
                params["sessionId"] = json!(id);
            }
            let request = json!({"jsonrpc":"2.0","id":number,"method":method,"params":params});
            write
                .write_all(format!("{request}\n").as_bytes())
                .await
                .unwrap();
            loop {
                let mut msg: Value =
                    serde_json::from_str(&lines.next_line().await.unwrap().unwrap()).unwrap();
                assert_eq!(msg["jsonrpc"], "2.0");
                if msg.get("method").is_none() && msg["id"] == number {
                    assert!(msg.get("error").is_none(), "{msg}");
                    if number == 0 && enabled {
                        assert_eq!(
                            msg["result"]["agentCapabilities"]["_meta"]["p1.dev"]["extensions"],
                            json!(["workflow_update"])
                        );
                    }
                    if number == 1 {
                        id = msg["result"]["sessionId"].as_str().unwrap().into();
                    }
                    break;
                }
                if msg["method"] == "_p1/workflow_update" {
                    assert_eq!(msg["params"]["sessionId"], id);
                    msg["params"]["sessionId"] = json!("<session>");
                    extensions.push(msg);
                } else if msg["params"]["update"]["sessionUpdate"] == "plan" {
                    plans.push(msg["params"]["update"].clone());
                }
            }
        }
        assert_eq!(plans.len(), 2, "standard plan remains unchanged");
        assert_eq!(plans.last().unwrap()["entries"][0]["status"], "pending");
        if enabled {
            let expected: Vec<Value> =
                include_str!("../../../docs/acp/fixtures/workflow-run-p1dev.jsonl")
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap()["msg"].clone())
                    .collect();
            assert_eq!(extensions, expected);
        } else {
            assert!(extensions.is_empty(), "{extensions:?}");
        }
        write.shutdown().await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let msg: Value = serde_json::from_str(&line).unwrap();
            assert_ne!(
                msg["method"], "_p1/workflow_update",
                "late notification: {msg}"
            );
        }
    };
    let (code, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(front.run(&session), client)
    })
    .await
    .expect("ACP seam replay hung");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn acp_workflow_extension_replays_eight_seam_events() {
    exercise(
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["workflow_update"]}}}),
        true,
    )
    .await;
}

#[tokio::test]
async fn acp_workflow_extension_is_silent_without_negotiation() {
    for capabilities in [
        json!({}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":[]}}}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["future"]}}}),
        json!({"_meta":{"p1.dev":{"version":2,"capabilities":["workflow_update"]}}}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["workflow_update",4]}}}),
    ] {
        exercise(capabilities, false).await;
    }
}
