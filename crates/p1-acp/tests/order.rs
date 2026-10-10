//! The order of the wire (#733): an answer goes out before what follows it, on any
//! runtime. Each case runs many times on a multi-thread runtime, where the transport's
//! handler tasks, the session loop and the router's relay run in parallel; nothing
//! but the order the code writes in may decide it. No sleeps.

mod common;

use common::{Client, FakeSession, Turn, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_acp::router::{Launched, SessionLauncher, serve};
use p1_contracts::frontend::FrontEndPort;
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader, DuplexStream, ReadHalf, WriteHalf};

const ROUNDS: usize = 200;

fn client(read: ReadHalf<DuplexStream>, write: WriteHalf<DuplexStream>) -> Client {
    Client {
        lines: BufReader::new(read).lines(),
        writer: write,
        transcript: Vec::new(),
    }
}

/// Starts each session in process: the driver over the fake session.
struct InProcess;

impl SessionLauncher for InProcess {
    fn launch(&self, workspace: &Path) -> std::io::Result<Launched> {
        let (process, router) = tokio::io::duplex(1 << 16);
        let (process_read, process_write) = tokio::io::split(process);
        let (router_read, router_write) = tokio::io::split(router);
        let front = Arc::new(AcpFrontEnd::new(
            Box::new(process_read),
            Box::new(process_write),
            workspace.to_path_buf(),
        ));
        let session = FakeSession::new(front.clone(), Turn::Reply);
        let served = tokio::spawn(async move { front.run(session.as_ref()).await });
        Ok(Launched {
            reader: Box::new(router_read),
            writer: Box::new(router_write),
            exited: Box::pin(async move {
                let _ = served.await;
            }),
        })
    }
}

/// Every line up to the answer to `id`, then the answer.
async fn answered(client: &mut Client, id: u64) -> (Vec<Value>, Value) {
    client.until_response(id).await
}

fn kind(message: &Value) -> &str {
    message["params"]["update"]["sessionUpdate"]
        .as_str()
        .unwrap_or("?")
}

/// What a conversation checks.
#[derive(Clone, Copy)]
enum Case {
    /// `session/new`, whose command list must follow its answer.
    Open,
    /// Then a prompt, and `/status` sent before the prompt's answer arrived: the
    /// prompts run in the order sent, and `/status`'s text follows the first answer.
    Prompts,
}

async fn converse(mut client: Client, case: Case) -> Client {
    client
        .request(
            0,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    answered(&mut client, 0).await;
    client
        .request(1, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
        .await;
    let (before, opened) = answered(&mut client, 1).await;
    assert!(
        before.is_empty(),
        "before the session/new answer: {before:?}"
    );
    let id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    let commands = client.next().await;
    assert_eq!(kind(&commands), "available_commands_update", "{commands}");
    if matches!(case, Case::Open) {
        return client;
    }
    for (request, text) in [(2, "hello"), (3, "/status")] {
        client
            .request(
                request,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":text}]}),
            )
            .await;
    }
    let (before, _) = answered(&mut client, 2).await;
    let texts: Vec<_> = before
        .iter()
        .map(|message| message["params"]["update"]["content"]["text"].clone())
        .collect();
    assert_eq!(texts, [json!("hello")], "the first prompt's own lines only");
    let (before, _) = answered(&mut client, 3).await;
    assert_eq!(before.len(), 2, "{before:?}");
    assert_eq!(kind(&before[0]), "session_info_update");
    assert_eq!(before[0]["params"]["update"]["title"], "hello");
    assert_eq!(kind(&before[1]), "agent_message_chunk");
    client
}

async fn through_the_driver(case: Case) {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, user_write) = tokio::io::split(user);
    let front = Arc::new(AcpFrontEnd::new(
        Box::new(agent_read),
        Box::new(agent_write),
        workspace(),
    ));
    let session = FakeSession::new(front.clone(), Turn::Reply);
    let served = tokio::spawn(async move { front.run(session.as_ref()).await });
    let mut client = converse(client(user_read, user_write), case).await;
    client.writer.shutdown().await.unwrap();
    while client.lines.next_line().await.unwrap().is_some() {}
    assert_eq!(served.await.unwrap(), 0);
}

async fn through_the_router(case: Case) {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, user_write) = tokio::io::split(user);
    let served = tokio::spawn(serve(
        Box::new(agent_read),
        Box::new(agent_write),
        Arc::new(InProcess),
        None,
    ));
    let mut client = converse(client(user_read, user_write), case).await;
    client.writer.shutdown().await.unwrap();
    while client.lines.next_line().await.unwrap().is_some() {}
    assert_eq!(served.await.unwrap(), 0);
}

/// A session process that writes its `session/new` answer and the update after it in
/// one write, so the router reads both at once: the order on the client's side is
/// then the router's alone.
struct Burst;

impl SessionLauncher for Burst {
    fn launch(&self, _workspace: &Path) -> std::io::Result<Launched> {
        let (process, router) = tokio::io::duplex(1 << 16);
        let (process_read, mut process_write) = tokio::io::split(process);
        let (router_read, router_write) = tokio::io::split(router);
        let served = tokio::spawn(async move {
            let mut lines = BufReader::new(process_read).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let request: Value = serde_json::from_str(&line).unwrap();
                let id = &request["id"];
                let text = match request["method"].as_str() {
                    Some("initialize") => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"protocolVersion":1}}).to_string()
                            + "\n"
                    }
                    Some("session/new") => {
                        json!({"jsonrpc":"2.0","id":id,"result":{"sessionId":"inner"}}).to_string()
                            + "\n"
                            + &json!({"jsonrpc":"2.0","method":"session/update","params":{
                                "sessionId":"inner","update":{
                                    "sessionUpdate":"available_commands_update",
                                    "availableCommands":[]}}})
                            .to_string()
                            + "\n"
                    }
                    _ => continue,
                };
                process_write.write_all(text.as_bytes()).await.unwrap();
            }
            let _ = process_write.shutdown().await;
        });
        Ok(Launched {
            reader: Box::new(router_read),
            writer: Box::new(router_write),
            exited: Box::pin(async move {
                let _ = served.await;
            }),
        })
    }
}

async fn burst() {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, user_write) = tokio::io::split(user);
    let served = tokio::spawn(serve(
        Box::new(agent_read),
        Box::new(agent_write),
        Arc::new(Burst),
        None,
    ));
    let mut client = converse(client(user_read, user_write), Case::Open).await;
    client.writer.shutdown().await.unwrap();
    while client.lines.next_line().await.unwrap().is_some() {}
    assert_eq!(served.await.unwrap(), 0);
}

async fn rounds(run: impl Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>) {
    for _ in 0..ROUNDS {
        tokio::time::timeout(std::time::Duration::from_secs(30), run())
            .await
            .expect("the conversation hung");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_the_driver_publishes_commands_after_the_session_new_answer() {
    rounds(|| Box::pin(through_the_driver(Case::Open))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_the_router_publishes_commands_after_the_session_new_answer() {
    rounds(|| Box::pin(through_the_router(Case::Open))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_the_driver_keeps_prompts_and_answers_in_order() {
    rounds(|| Box::pin(through_the_driver(Case::Prompts))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_the_router_keeps_prompts_and_answers_in_order() {
    rounds(|| Box::pin(through_the_router(Case::Prompts))).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn acp_the_router_answers_before_a_line_read_with_the_answer() {
    rounds(|| Box::pin(burst())).await;
}
