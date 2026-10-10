//! The `p1 acp` session driver over an in-memory pipe, against a fake session
//! handle: the host is not involved, so each case scripts exactly what the session
//! does and reads every line the driver writes. No network, no sleeps.

use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{
    BackgroundKind, BackgroundPhase, BackgroundSignal, FrontEndPort, SessionHandle,
};
use p1_contracts::{
    AgentEvent, AuthorizationRequest, BoxFuture, CancellationToken, Decision, Effect, StopReason,
    ToolCall, ToolIdentity, ToolInput, TurnEnd,
};
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};
use tokio::sync::Notify;

fn workspace() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

const DONE: TurnEnd = TurnEnd::Completed {
    stop: StopReason::EndTurn,
};

/// What the fake session's `prompt` does.
#[derive(Clone, Copy)]
enum Turn {
    /// Streams "hello" and ends.
    Reply,
    /// Asks permission for one call and ends with what it was told.
    Ask,
    /// Starts a workflow run and a worker, says so, and ends; the work stays live.
    StartWork,
}

struct FakeSession {
    front: Arc<AcpFrontEnd>,
    turn: Turn,
    calls: Mutex<Vec<String>>,
    inbox: Mutex<VecDeque<String>>,
    arrived: Notify,
    decision: Mutex<Option<Decision>>,
}

impl FakeSession {
    fn new(front: Arc<AcpFrontEnd>, turn: Turn) -> Arc<Self> {
        Arc::new(Self {
            front,
            turn,
            calls: Mutex::new(Vec::new()),
            inbox: Mutex::new(VecDeque::new()),
            arrived: Notify::new(),
            decision: Mutex::new(None),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn say(&self, text: &str) {
        self.front.event_sink().emit(AgentEvent::TextDelta {
            text: text.to_string(),
        });
    }

    /// One piece of background work ends and its notice reaches the inbox: a
    /// workflow run queues its notice first, a worker reports its end first.
    fn end_work(&self, kind: BackgroundKind, id: &str) {
        if kind == BackgroundKind::Workflow {
            self.notice(id);
            self.signal_end(kind, id);
        } else {
            self.signal_end(kind, id);
            self.notice(id);
        }
    }

    fn signal_end(&self, kind: BackgroundKind, id: &str) {
        self.front.background(BackgroundSignal {
            phase: BackgroundPhase::Ended,
            kind,
            id: id.to_string(),
            turn: Some(1),
        });
    }

    fn notice(&self, id: &str) {
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
}

/// The client end of the pipe. Every line the driver writes must parse as a JSON-RPC
/// 2.0 message, and the whole transcript is kept for the final checks.
struct Client {
    lines: tokio::io::Lines<BufReader<ReadHalf<DuplexStream>>>,
    writer: WriteHalf<DuplexStream>,
    transcript: Vec<Value>,
}

impl Client {
    async fn send(&mut self, message: Value) {
        let mut line = serde_json::to_vec(&message).unwrap();
        line.push(b'\n');
        self.writer.write_all(&line).await.unwrap();
    }

    async fn request(&mut self, id: u64, method: &str, params: Value) {
        self.send(json!({"jsonrpc":"2.0","id":id,"method":method,"params":params}))
            .await;
    }

    async fn next(&mut self) -> Value {
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
    async fn until_response(&mut self, id: u64) -> (Vec<Value>, Value) {
        let mut before = Vec::new();
        loop {
            let message = self.next().await;
            if message["id"] == id && message.get("method").is_none() {
                return (before, message);
            }
            before.push(message);
        }
    }

    async fn open(&mut self) -> String {
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
        session["result"]["sessionId"].as_str().unwrap().to_string()
    }

    /// A client that declared no `p1.dev` sees no p1 extension.
    fn assert_no_extensions(&self) {
        let text = serde_json::to_string(&self.transcript).unwrap();
        assert!(!text.contains("_p1"), "{text}");
        assert!(!text.contains("p1.dev"), "{text}");
    }
}

fn texts(messages: &[Value]) -> Vec<String> {
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

/// Runs `client` against the driver serving `turn`; returns the session to inspect.
async fn drive<F, Fut>(turn: Turn, script: F) -> Arc<FakeSession>
where
    F: FnOnce(Client, Arc<FakeSession>) -> Fut,
    Fut: std::future::Future<Output = Client>,
{
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, user_write) = tokio::io::split(user);
    let front = Arc::new(AcpFrontEnd::new(
        Box::new(agent_read),
        Box::new(agent_write),
        workspace(),
    ));
    let session = FakeSession::new(front.clone(), turn);
    let client = Client {
        lines: BufReader::new(user_read).lines(),
        writer: user_write,
        transcript: Vec::new(),
    };
    let served = front.run(session.as_ref());
    let script = async {
        let mut client = script(client, session.clone()).await;
        client.assert_no_extensions();
        // EOF ends the session; the driver then closes its side.
        client.writer.shutdown().await.unwrap();
        while client.lines.next_line().await.unwrap().is_some() {}
    };
    let (code, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(served, script)
    })
    .await
    .expect("the driver hung");
    assert_eq!(code, 0);
    session
}

#[tokio::test]
async fn acp_handshake_and_one_session_per_process() {
    drive(Turn::Reply, |mut client, _| async move {
        client
            .request(
                0,
                "initialize",
                json!({"protocolVersion":1,"clientCapabilities":{}}),
            )
            .await;
        let (_, init) = client.until_response(0).await;
        assert_eq!(init["result"]["protocolVersion"], 1);
        assert_eq!(init["result"]["agentCapabilities"]["loadSession"], false);
        assert!(init["result"]["agentCapabilities"].get("_meta").is_none());

        client
            .request(
                1,
                "session/new",
                json!({"cwd":"/nonexistent/elsewhere","mcpServers":[]}),
            )
            .await;
        let (_, wrong) = client.until_response(1).await;
        assert_eq!(wrong["error"]["code"], -32602, "{wrong}");
        let message = wrong["error"]["message"].as_str().unwrap();
        assert!(message.contains("/nonexistent/elsewhere"), "{message}");
        assert!(message.contains(workspace().to_str().unwrap()), "{message}");

        client
            .request(2, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
            .await;
        let (_, first) = client.until_response(2).await;
        assert!(first["result"]["sessionId"].is_string(), "{first}");
        client
            .request(3, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
            .await;
        let (_, second) = client.until_response(3).await;
        assert!(
            second["error"]["message"]
                .as_str()
                .unwrap()
                .contains("one session"),
            "{second}"
        );
        client
    })
    .await;
}

#[tokio::test]
async fn acp_prompt_streams_updates_and_answers_end_turn() {
    let session = drive(Turn::Reply, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"image","data":"","mimeType":"image/png"}]}))
            .await;
        let (_, refused) = client.until_response(2).await;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");

        client
            .request(3, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"hi"}]}))
            .await;
        let (updates, done) = client.until_response(3).await;
        assert_eq!(texts(&updates), ["hello"]);
        assert!(updates.iter().all(|update| update["params"]["sessionId"] == id.as_str()));
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");

        // A resource link is baseline content: it reaches the turn as name and URI.
        client
            .request(4, "session/prompt", json!({"sessionId":id,"prompt":[
                {"type":"text","text":"see"},
                {"type":"resource_link","name":"notes","uri":"file:///tmp/notes.md"}
            ]}))
            .await;
        let (_, linked) = client.until_response(4).await;
        assert_eq!(linked["result"]["stopReason"], "end_turn", "{linked}");
        client
    })
    .await;
    let calls = session.calls();
    assert_eq!(calls[0], "prompt hi");
    assert_eq!(
        calls[1],
        "prompt see\n\nLinked resources:\n[notes](file:///tmp/notes.md)"
    );
}

/// The call is announced before its permission request; a cancel then resolves the
/// parked request as deny, answers the prompt `cancelled` and calls both hooks.
#[tokio::test]
async fn acp_cancel_denies_a_parked_approval_and_calls_both_hooks() {
    let session = drive(Turn::Ask, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let announced = client.next().await;
        assert_eq!(announced["method"], "session/update", "{announced}");
        assert_eq!(announced["params"]["update"]["sessionUpdate"], "tool_call");
        assert_eq!(announced["params"]["update"]["status"], "pending");
        let asked = client.next().await;
        assert_eq!(asked["method"], "session/request_permission", "{asked}");
        assert_eq!(asked["params"]["toolCall"]["toolCallId"], "c1");

        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        let (_, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "cancelled", "{done}");
        // The client answers the abandoned request as cancelled, as ACP asks.
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"cancelled"}}}))
            .await;
        client
    })
    .await;
    assert!(matches!(
        *session.decision.lock().unwrap(),
        Some(Decision::Deny { .. })
    ));
    let calls = session.calls();
    assert!(calls.contains(&"cancel_runs".to_string()), "{calls:?}");
    assert!(calls.contains(&"stop_workers".to_string()), "{calls:?}");
}

/// An answered approval permits the call and the prompt finishes normally.
#[tokio::test]
async fn acp_an_allowed_approval_permits_the_call() {
    let session = drive(Turn::Ask, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let _announced = client.next().await;
        let asked = client.next().await;
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        let (_, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(*session.decision.lock().unwrap(), Some(Decision::Permit));
}

/// D4: the prompt stays pending while the work it started is live; the inbox turn
/// about that work runs inside the prompt, which then answers.
#[tokio::test]
async fn acp_a_prompt_holds_until_its_work_ends_then_runs_the_inbox_turn() {
    let session = drive(Turn::StartWork, |mut client, session| async move {
        let id = client.open().await;
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"go"}]}),
            )
            .await;
        let started = client.next().await;
        assert_eq!(texts(&[started]), ["work started"]);
        // The turn is over; the prompt holds. The work now ends, as the host reports it.
        session.end_work(BackgroundKind::Worker, "w1");
        session.end_work(BackgroundKind::Workflow, "wf1");
        let (updates, done) = client.until_response(2).await;
        let said = texts(&updates).concat();
        assert!(said.contains("summary of"), "{said}");
        assert!(
            said.contains("w1 ended") && said.contains("wf1 ended"),
            "{said}"
        );
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    // The closing EOF stops the session's work: the two hooks come last.
    let calls = session.calls();
    assert_eq!(calls[0], "prompt go");
    assert_eq!(calls[calls.len() - 2..], ["cancel_runs", "stop_workers"]);
    assert!(
        calls[1..calls.len() - 2]
            .iter()
            .all(|call| call.starts_with("inbox")),
        "{calls:?}"
    );
}

/// A cancel releases the hold: the prompt answers `cancelled` without waiting.
#[tokio::test]
async fn acp_cancel_releases_a_held_prompt() {
    drive(Turn::StartWork, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"go"}]}),
            )
            .await;
        let _started = client.next().await;
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        let (_, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "cancelled", "{done}");
        client
    })
    .await;
}

/// The next prompt releases a held one: the held prompt answers, then the next runs.
#[tokio::test]
async fn acp_the_next_prompt_releases_a_held_one() {
    let session = drive(Turn::StartWork, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"go"}]}),
            )
            .await;
        let _started = client.next().await;
        client
            .request(
                3,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"again"}]}),
            )
            .await;
        let (_, held) = client.until_response(2).await;
        assert_eq!(held["result"]["stopReason"], "end_turn", "{held}");
        // The next prompt starts work of its own and holds on it; a cancel ends it.
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        let (_, next) = client.until_response(3).await;
        assert_eq!(next["result"]["stopReason"], "cancelled", "{next}");
        client
    })
    .await;
    assert_eq!(session.calls()[..2], ["prompt go", "prompt again"]);
}

/// A worker reports its end before its notice reaches the inbox; the prompt waits for
/// the notice and runs its inbox turn before it answers.
#[tokio::test]
async fn acp_a_held_prompt_waits_for_a_late_worker_notice() {
    let session = drive(Turn::StartWork, |mut client, session| async move {
        let id = client.open().await;
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"go"}]}),
            )
            .await;
        let _started = client.next().await;
        session.end_work(BackgroundKind::Workflow, "wf1");
        // The inbox turn about wf1 runs; w1 is still live.
        let summary = client.next().await;
        assert_eq!(texts(&[summary]), ["summary of wf1 ended"]);
        session.signal_end(BackgroundKind::Worker, "w1");
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        session.notice("w1");
        let (updates, done) = client.until_response(2).await;
        assert_eq!(texts(&updates), ["summary of w1 ended"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(
        session.calls()[..3],
        ["prompt go", "inbox wf1 ended", "inbox w1 ended"]
    );
}

/// The client goes away while a prompt holds: the prompt is cancelled with its work,
/// and the process ends instead of waiting for the work.
#[tokio::test]
async fn acp_eof_cancels_a_held_prompt_and_its_work() {
    let session = drive(Turn::StartWork, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"go"}]}),
            )
            .await;
        let _started = client.next().await;
        client.writer.shutdown().await.unwrap();
        let (_, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "cancelled", "{done}");
        client
    })
    .await;
    let calls = session.calls();
    assert!(calls.contains(&"cancel_runs".to_string()), "{calls:?}");
    assert!(calls.contains(&"stop_workers".to_string()), "{calls:?}");
}
