//! The fake session and the scripted client the driver and router tests share.

#![allow(dead_code, reason = "each test binary uses part of the shared helpers")]

use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{
    BackgroundKind, BackgroundPhase, BackgroundSignal, CommandInfo, CommandOutput, ConfigChoice,
    ConfigKind, ConfigValue, FrontEndPort, SessionHandle,
};
use p1_contracts::{
    AgentEvent, AuthorizationRequest, BoxFuture, CancellationToken, Decision, Effect, StopReason,
    ToolCall, ToolIdentity, ToolInput, TurnEnd,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::Notify;

pub fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

pub const DONE: TurnEnd = TurnEnd::Completed {
    stop: StopReason::EndTurn,
};

/// What the fake session's `prompt` does.
#[derive(Clone, Copy)]
pub enum Turn {
    /// Streams "hello" and ends.
    Reply,
    /// Asks permission for one call and ends with what it was told.
    Ask,
    /// Starts a workflow run and a worker, says so, and ends; the work stays live.
    StartWork,
}

pub struct FakeSession {
    pub front: Arc<AcpFrontEnd>,
    turn: Turn,
    calls: Mutex<Vec<String>>,
    inbox: Mutex<VecDeque<String>>,
    arrived: Notify,
    pub decision: Mutex<Option<Decision>>,
    /// The model and effort it runs: `e/fast` offers `low`, `e/deep` offers `low`
    /// and `high`; `e/broken` is offered but fails to switch to.
    setting: Mutex<(String, String)>,
}

fn values(names: &[&str]) -> Vec<ConfigValue> {
    names
        .iter()
        .map(|name| ConfigValue {
            value: name.to_string(),
            name: name.to_string(),
            description: None,
        })
        .collect()
}

impl FakeSession {
    pub fn new(front: Arc<AcpFrontEnd>, turn: Turn) -> Arc<Self> {
        Arc::new(Self {
            front,
            turn,
            calls: Mutex::new(Vec::new()),
            inbox: Mutex::new(VecDeque::new()),
            arrived: Notify::new(),
            decision: Mutex::new(None),
            setting: Mutex::new(("e/fast".to_string(), "low".to_string())),
        })
    }

    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    pub fn say(&self, text: &str) {
        self.front.event_sink().emit(AgentEvent::TextDelta {
            text: text.to_string(),
        });
    }

    /// One piece of background work ends and its notice reaches the inbox: a
    /// workflow run queues its notice first, a worker reports its end first.
    pub fn end_work(&self, kind: BackgroundKind, id: &str) {
        if kind == BackgroundKind::Workflow {
            self.notice(id);
            self.signal_end(kind, id);
        } else {
            self.signal_end(kind, id);
            self.notice(id);
        }
    }

    pub fn signal_end(&self, kind: BackgroundKind, id: &str) {
        self.front.background(BackgroundSignal {
            phase: BackgroundPhase::Ended,
            kind,
            id: id.to_string(),
            turn: Some(1),
        });
    }

    pub fn notice(&self, id: &str) {
        self.inbox.lock().unwrap().push_back(format!("{id} ended"));
        self.arrived.notify_one();
    }
}

impl SessionHandle for FakeSession {
    fn prompt<'a>(&'a self, text: String, cancel: CancellationToken) -> BoxFuture<'a, TurnEnd> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("prompt {text}"));
            match self.turn {
                Turn::Reply => {
                    self.say("hello");
                    DONE
                }
                Turn::Ask => {
                    let call = ToolCall {
                        call_id: "c1".into(),
                        name: "read".into(),
                        input: ToolInput::Json(r#"{"file_path":"README.md"}"#.into()),
                    };
                    let identity = ToolIdentity {
                        implementation: "fake".into(),
                        variant: "default".into(),
                    };
                    let decision = self
                        .front
                        .authorization()
                        .authorize(AuthorizationRequest {
                            call: &call,
                            identity: &identity,
                            effect: Effect::ReadOnly,
                        })
                        .await;
                    *self.decision.lock().unwrap() = Some(decision);
                    if cancel.is_cancelled() {
                        TurnEnd::Cancelled
                    } else {
                        DONE
                    }
                }
                Turn::StartWork => {
                    for (kind, id) in [
                        (BackgroundKind::Workflow, "wf1"),
                        (BackgroundKind::Worker, "w1"),
                    ] {
                        self.front.background(BackgroundSignal {
                            phase: BackgroundPhase::Started,
                            kind,
                            id: id.to_string(),
                            turn: Some(1),
                        });
                    }
                    self.say("work started");
                    DONE
                }
            }
        })
    }

    fn cancel_runs<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("cancel_runs".into());
            self.end_work(BackgroundKind::Workflow, "wf1");
        })
    }

    fn stop_workers<'a>(&'a self) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            self.calls.lock().unwrap().push("stop_workers".into());
            self.end_work(BackgroundKind::Worker, "w1");
        })
    }

    fn drain_inbox<'a>(&'a self, _cancel: CancellationToken) -> BoxFuture<'a, Option<TurnEnd>> {
        Box::pin(async move {
            let notices: Vec<String> = self.inbox.lock().unwrap().drain(..).collect();
            if notices.is_empty() {
                return None;
            }
            self.calls
                .lock()
                .unwrap()
                .push(format!("inbox {}", notices.join(", ")));
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

    fn config<'a>(&'a self) -> BoxFuture<'a, Vec<ConfigChoice>> {
        Box::pin(async move {
            let (model, effort) = self.setting.lock().unwrap().clone();
            let efforts: &[&str] = if model == "e/deep" {
                &["low", "high"]
            } else {
                &["low"]
            };
            vec![
                ConfigChoice {
                    kind: ConfigKind::Model,
                    current: model,
                    values: values(&["e/fast", "e/deep", "e/broken"]),
                },
                ConfigChoice {
                    kind: ConfigKind::Effort,
                    current: effort,
                    values: values(efforts),
                },
            ]
        })
    }

    fn set_config<'a>(
        &'a self,
        kind: ConfigKind,
        value: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("set {kind:?} {value}"));
            if value == "e/broken" {
                return Err("e/broken does not assemble".to_string());
            }
            let mut setting = self.setting.lock().unwrap();
            match kind {
                ConfigKind::Model => setting.0 = value.to_string(),
                ConfigKind::Effort => setting.1 = value.to_string(),
            }
            Ok(())
        })
    }

    /// `status` reports, `review` is a turn for the model (a skill), `broken` fails.
    fn commands<'a>(&'a self) -> BoxFuture<'a, Vec<CommandInfo>> {
        Box::pin(async move {
            let command = |name: &str, hint: Option<&str>| CommandInfo {
                name: name.to_string(),
                description: format!("the {name} command"),
                hint: hint.map(str::to_string),
            };
            vec![
                command("status", None),
                command("review", Some("what to review")),
                command("broken", None),
                // The driver's own `/model` wins over a host command of that name.
                command("model", None),
            ]
        })
    }

    fn command<'a>(
        &'a self,
        name: &'a str,
        argument: &'a str,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<CommandOutput, String>> {
        Box::pin(async move {
            self.calls
                .lock()
                .unwrap()
                .push(format!("command {name} {argument}"));
            match name {
                "status" => Ok(CommandOutput::Text("model e/fast\n".to_string())),
                "review" => Ok(CommandOutput::Prompt(format!(
                    "use the review skill: {argument}"
                ))),
                _ => Err("it broke".to_string()),
            }
        })
    }
}

/// The client end of the pipe. Every line the driver writes must parse as a JSON-RPC
/// 2.0 message, and the whole transcript is kept for the final checks.
pub struct Client {
    pub lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    pub writer: WriteHalf<DuplexStream>,
    pub transcript: Vec<Value>,
}

impl Client {
    pub async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
    }

    pub async fn request(&mut self, id: u64, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
    }

    pub async fn next(&mut self) -> Value {
        let line = self
            .lines
            .next_line()
            .await
            .unwrap()
            .expect("the driver closed the stream early");
        let message: Value = serde_json::from_str(&line).expect("every line is JSON");
        assert_eq!(message["jsonrpc"], "2.0", "{message}");
        assert!(
            message.get("method").is_some() || message.get("id").is_some(),
            "a request, notification or response: {message}"
        );
        self.transcript.push(message.clone());
        message
    }

    /// Lines up to and including the response to `id`.
    pub async fn until_response(&mut self, id: u64) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let message = self.next().await;
            if message["id"] == id && message.get("method").is_none() {
                return (before, message);
            }
            before.push(message);
        }
    }

    pub async fn open(&mut self) -> String {
        self.request(
            0,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
        let (_, init) = self.until_response(0).await;
        assert_eq!(init["result"]["protocolVersion"], 1, "{init}");
        self.request(1, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
            .await;
        let (_, session) = self.until_response(1).await;
        // The command list follows the session's id (#676).
        let commands = self.next().await;
        assert_eq!(
            commands["params"]["update"]["sessionUpdate"], "available_commands_update",
            "{commands}"
        );
        session["result"]["sessionId"].as_str().unwrap().to_string()
    }

    /// A client that declared no `p1.dev` sees no p1 extension.
    pub fn assert_no_extensions(&self) {
        let text = serde_json::to_string(&self.transcript).unwrap();
        assert!(!text.contains("_p1"), "{text}");
        assert!(!text.contains("p1.dev"), "{text}");
    }
}

pub fn texts(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .filter(|message| message["params"]["update"]["sessionUpdate"] == "agent_message_chunk")
        .map(|message| {
            message["params"]["update"]["content"]["text"]
                .as_str()
                .unwrap()
                .to_string()
        })
        .collect()
}
