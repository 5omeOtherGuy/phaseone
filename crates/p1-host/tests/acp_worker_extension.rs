//! Scripted child callbacks through the host bridge and real ACP byte stream.
#![cfg(feature = "delegation")]

use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{FrontEndPort, SessionHandle};
use p1_contracts::*;
use p1_host::frontend::FrontEnd;
use p1_host::frontend_port::PortFrontEnd;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

struct Session(PortFrontEnd);

impl SessionHandle for Session {
    fn prompt<'a>(&'a self, _text: String, _cancel: CancellationToken) -> BoxFuture<'a, TurnEnd> {
        Box::pin(async move {
            let child = self
                .0
                .child_event_sink("w9", "fixture-route", "fixture-model");
            self.0.worker_context_configured("w9", Some(8192));
            self.0.child_started("w9");
            child.emit(AgentEvent::TurnStarted);
            child.emit(AgentEvent::TextDelta {
                text: "child answer".into(),
            });
            child.emit(AgentEvent::ReasoningDelta {
                text: "child reasoning".into(),
            });
            child.emit(AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "same-call".into(),
                    name: "read".into(),
                    input: ToolInput::Json(r#"{"path":"public.txt"}"#.into()),
                },
            });
            child.emit(AgentEvent::ToolFinished {
                result: ToolResultItem {
                    call_id: "same-call".into(),
                    name: "read".into(),
                    status: ToolStatus::Ok,
                    content: "public result".into(),
                },
            });
            child.emit(AgentEvent::ResponseCompleted {
                model: "fixture-model".into(),
                stop: StopReason::EndTurn,
                usage: Some(Usage {
                    input_uncached: Some(17),
                    cache_read: Some(5),
                    cache_write: Some(3),
                    output: Some(9),
                    reasoning_output: None,
                    cost_micro_usd: Some(250),
                }),
            });
            child.emit(AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
            });
            self.0.worker_ended(
                "w9",
                "inspect",
                &p1_workers::WorkerReport::new(vec!["read".into()]),
            );
            // Continuing reuses the sink, metadata and usage state, but opens again.
            child.emit(AgentEvent::TurnStarted);
            child.emit(AgentEvent::TextDelta {
                text: "continued answer".into(),
            });
            self.0
                .worker_ended("w9", "continue", &p1_workers::WorkerReport::default());
            drop(child);
            // A failed build or shutdown still gets an end, with unknown summary.
            let dropped = self.0.child_event_sink("w10", "other-route", "other-model");
            self.0.child_started("w10");
            drop(dropped);
            self.0.event_sink().emit(AgentEvent::TextDelta {
                text: "parent answer".into(),
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
        let mut workers = Vec::new();
        let mut parent = Vec::new();
        for (number, method, mut params) in [
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
                if msg.get("method").is_none() && msg["id"] == number {
                    assert!(msg.get("error").is_none(), "{msg}");
                    if number == 0 && enabled {
                        assert_eq!(
                            msg["result"]["agentCapabilities"]["_meta"]["p1.dev"]["extensions"],
                            json!(["worker_transcripts"])
                        );
                    }
                    if number == 1 {
                        id = msg["result"]["sessionId"].as_str().unwrap().into();
                    }
                    break;
                }
                if msg["method"] == "_p1/worker_update"
                    || !msg["params"]["update"]["_meta"]["p1.dev"]["worker"].is_null()
                {
                    assert_eq!(msg["params"]["sessionId"], id);
                    msg["params"]["sessionId"] = json!("<session>");
                    workers.push(msg);
                } else if msg["params"]["update"]["sessionUpdate"] == "agent_message_chunk" {
                    parent.push(
                        msg["params"]["update"]["content"]["text"]
                            .as_str()
                            .unwrap()
                            .to_string(),
                    );
                }
            }
        }
        assert_eq!(
            parent,
            [
                "worker w9 (inspect; read) ended without finish",
                "worker w9 (continue; ) ended without finish",
                "parent answer"
            ]
        );
        if enabled {
            let expected: Vec<Value> =
                include_str!("../../../docs/acp/fixtures/worker-transcript-p1dev.jsonl")
                    .lines()
                    .map(|line| serde_json::from_str::<Value>(line).unwrap()["msg"].clone())
                    .collect();
            assert_eq!(workers, expected);
        } else {
            assert!(workers.is_empty(), "{workers:?}");
        }
        write.shutdown().await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            let msg: Value = serde_json::from_str(&line).unwrap();
            assert_ne!(msg["method"], "_p1/worker_update");
            assert!(
                msg["params"]["update"]["_meta"]["p1.dev"]["worker"].is_null(),
                "late worker update: {msg}"
            );
        }
    };
    let (code, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(front.run(&session), client)
    })
    .await
    .expect("worker seam replay hung");
    assert_eq!(code, 0);
}

#[tokio::test]
async fn acp_worker_transcripts_replay_and_continue() {
    exercise(json!({"_meta":{"p1.dev":{"version":1,"capabilities":["worker_transcripts","worker_transcripts"]}}}), true).await;
}

#[tokio::test]
async fn acp_worker_transcripts_are_silent_without_negotiation() {
    for capabilities in [
        json!({}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":[]}}}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["workflow_update"]}}}),
        json!({"_meta":{"p1.dev":{"version":2,"capabilities":["worker_transcripts"]}}}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["worker_transcripts",4]}}}),
    ] {
        exercise(capabilities, false).await;
    }
}
