//! `_p1/agent_state` (#683): a client that declares it opts out of the hold rule (D4).
//! Its prompt answers at the turn's end, the parent's inbox turns run between
//! prompts, and every turn reports its state. Against the fake session over an
//! in-memory pipe; no network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, texts, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{BackgroundKind, FrontEndPort};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const DECLARING: &str = r#"{"_meta":{"p1.dev":{"version":1,"capabilities":["agent_state"]}}}"#;

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

/// Initializes with `capabilities` and opens the session; the initialize answer and
/// the session id.
async fn open(client: &mut Client, capabilities: Value) -> (Value, String) {
    client
        .request(
            0,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":capabilities}),
        )
        .await;
    let (_, init) = client.until_response(0).await;
    client
        .request(1, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
        .await;
    let (_, session) = client.until_response(1).await;
    let commands = client.next().await;
    assert_eq!(
        commands["params"]["update"]["sessionUpdate"],
        "available_commands_update"
    );
    (
        init,
        session["result"]["sessionId"].as_str().unwrap().to_string(),
    )
}

async fn prompt(client: &mut Client, id: u64, session: &str, text: &str) {
    client
        .request(
            id,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
        )
        .await;
}

/// A line as the test compares it: the state, a message chunk, another update kind,
/// a request or an answer.
fn shape(message: &Value) -> String {
    if message["method"] == "_p1/agent_state" {
        let params = &message["params"];
        return match params["stopReason"].as_str() {
            Some(stop) => format!("state {} {stop}", params["state"].as_str().unwrap()),
            None => format!("state {}", params["state"].as_str().unwrap()),
        };
    }
    if let Some(text) = texts(std::slice::from_ref(message)).pop() {
        return format!("say {text}");
    }
    if let Some(kind) = message["params"]["update"]["sessionUpdate"].as_str() {
        return format!("update {kind}");
    }
    if let Some(method) = message["method"].as_str() {
        return format!("ask {method}");
    }
    format!("answer {}", message["result"]["stopReason"])
}

async fn lines(client: &mut Client, count: usize) -> Vec<String> {
    let mut shapes = Vec::new();
    for _ in 0..count {
        shapes.push(shape(&client.next().await));
    }
    shapes
}

#[tokio::test]
async fn acp_agent_state_is_advertised_only_when_declared() {
    drive(Turn::Reply, |mut client, _| async move {
        let (init, _) = open(&mut client, serde_json::from_str(DECLARING).unwrap()).await;
        assert_eq!(
            init["result"]["agentCapabilities"]["_meta"]["p1.dev"],
            json!({"version":1,"extensions":["agent_state"]})
        );
        client
    })
    .await;
}

/// The two-step background work: the prompt answers at its turn's end, then each
/// piece of work's inbox turn streams between prompts with its own two transitions.
#[tokio::test]
async fn acp_agent_state_answers_at_the_turn_end_and_streams_the_inbox_turns() {
    let session = drive(Turn::StartWork, |mut client, session| async move {
        let (_, id) = open(&mut client, serde_json::from_str(DECLARING).unwrap()).await;
        prompt(&mut client, 2, &id, "go").await;
        // The session's title follows the first answer.
        assert_eq!(
            lines(&mut client, 5).await,
            [
                "state running",
                "say work started",
                "state idle end_turn",
                "answer \"end_turn\"",
                "update session_info_update"
            ]
        );
        // The work is still live and the prompt has answered.
        session.end_work(BackgroundKind::Workflow, "wf1");
        assert_eq!(
            lines(&mut client, 3).await,
            [
                "state running",
                "say summary of wf1 ended",
                "state idle end_turn"
            ]
        );
        session.end_work(BackgroundKind::Worker, "w1");
        assert_eq!(
            lines(&mut client, 3).await,
            [
                "state running",
                "say summary of w1 ended",
                "state idle end_turn"
            ]
        );
        // The next prompt is a turn of its own.
        prompt(&mut client, 3, &id, "again").await;
        let (updates, done) = client.until_response(3).await;
        assert_eq!(done["result"]["stopReason"], "end_turn");
        let updates: Vec<String> = updates.iter().map(shape).collect();
        assert_eq!(updates.first().unwrap(), "state running");
        assert_eq!(updates.last().unwrap(), "state idle end_turn");
        client
    })
    .await;
    assert_eq!(
        session.calls()[..4],
        [
            "prompt go",
            "inbox wf1 ended",
            "inbox w1 ended",
            "prompt again"
        ]
    );
}

/// Without the capability the prompt holds to the end, also for a `p1.dev` client
/// that declared other extensions, and no state is sent.
#[tokio::test]
async fn acp_agent_state_absent_keeps_the_hold_and_sends_nothing() {
    for capabilities in [
        json!({}),
        json!({"_meta":{"p1.dev":{"version":1,"capabilities":["workflow_update"]}}}),
        json!({"_meta":{"p1.dev":{"version":2,"capabilities":["agent_state"]}}}),
    ] {
        drive(Turn::StartWork, |mut client, session| async move {
            let (init, id) = open(&mut client, capabilities).await;
            assert!(!init.to_string().contains("agent_state"), "{init}");
            prompt(&mut client, 2, &id, "go").await;
            let started = client.next().await;
            assert_eq!(texts(&[started]), ["work started"]);
            session.end_work(BackgroundKind::Workflow, "wf1");
            session.end_work(BackgroundKind::Worker, "w1");
            let (updates, done) = client.until_response(2).await;
            let said = texts(&updates).concat();
            assert!(
                said.contains("wf1 ended") && said.contains("w1 ended"),
                "the inbox turns ran inside the held prompt: {said}"
            );
            assert_eq!(done["result"]["stopReason"], "end_turn");
            let wire = serde_json::to_string(&client.transcript).unwrap();
            assert!(!wire.contains("_p1/agent_state"), "{wire}");
            client
        })
        .await;
    }
}

/// A permission request makes the state `requires_action` until the client answers.
#[tokio::test]
async fn acp_agent_state_requires_action_while_a_permission_waits() {
    drive(Turn::Ask, |mut client, _| async move {
        let (_, id) = open(&mut client, serde_json::from_str(DECLARING).unwrap()).await;
        prompt(&mut client, 2, &id, "go").await;
        assert_eq!(
            lines(&mut client, 3).await,
            [
                "state running",
                "update tool_call",
                "state requires_action"
            ]
        );
        let asked = client.next().await;
        assert_eq!(asked["method"], "session/request_permission");
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        assert_eq!(
            lines(&mut client, 3).await,
            [
                "state running",
                "state idle end_turn",
                "answer \"end_turn\""
            ]
        );
        client
    })
    .await;
}

/// A cancel while a permission waits: the cancelled turn's idle, with its reason,
/// still precedes the answer.
#[tokio::test]
async fn acp_agent_state_a_cancel_during_a_permission_ends_idle_before_the_answer() {
    drive(Turn::Ask, |mut client, _| async move {
        let (_, id) = open(&mut client, serde_json::from_str(DECLARING).unwrap()).await;
        prompt(&mut client, 2, &id, "go").await;
        let asked = lines(&mut client, 4).await;
        assert_eq!(
            asked[2..],
            ["state requires_action", "ask session/request_permission"]
        );
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        assert_eq!(
            lines(&mut client, 3).await,
            [
                "state running",
                "state idle cancelled",
                "answer \"cancelled\""
            ]
        );
        client
    })
    .await;
}

/// The notices a cancel leaves (the stopped work's ends) wait for the next prompt,
/// as under the hold; the client's next input starts inbox turns again.
#[tokio::test]
async fn acp_agent_state_a_cancel_leaves_its_notices_for_the_next_prompt() {
    let session = drive(Turn::StartWork, |mut client, _| async move {
        let (_, id) = open(&mut client, serde_json::from_str(DECLARING).unwrap()).await;
        prompt(&mut client, 2, &id, "go").await;
        client.until_response(2).await;
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        client
            .request(
                3,
                "session/set_config_option",
                json!({"sessionId":id,"configId":"model","value":"e/deep"}),
            )
            .await;
        let (before, _) = client.until_response(3).await;
        let before: Vec<String> = before.iter().map(shape).collect();
        assert!(
            !before.iter().any(|line| line.starts_with("state")),
            "no turn ran after the cancel: {before:?}"
        );
        prompt(&mut client, 4, &id, "again").await;
        client.until_response(4).await;
        let mut after = Vec::new();
        while after
            .last()
            .is_none_or(|last: &String| last != "state idle end_turn")
        {
            after.push(shape(&client.next().await));
        }
        assert_eq!(after.first().unwrap(), "state running", "{after:?}");
        client
    })
    .await;
    let calls = session.calls();
    assert_eq!(
        calls[..4],
        [
            "prompt go",
            "cancel_runs",
            "stop_workers",
            "set Model e/deep"
        ]
    );
    assert_eq!(calls[4], "prompt again");
    assert!(calls[5].starts_with("inbox wf1 ended"), "{calls:?}");
}
