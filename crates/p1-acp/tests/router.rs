//! Several sessions behind one `p1 acp` (ADR-0156): the router over an in-memory
//! pipe, with each session process replaced by the single-session driver over a fake
//! session handle, started in process. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, texts, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_acp::router::{Launched, SessionLauncher, serve};
use p1_contracts::Decision;
use p1_contracts::frontend::FrontEndPort;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Starts each session in process, with the next scripted turn.
struct FakeLauncher {
    turns: Mutex<VecDeque<Turn>>,
    started: Mutex<Vec<(PathBuf, Arc<FakeSession>)>>,
    /// Each exited session process's exit code.
    exits: Arc<Mutex<Vec<i32>>>,
}

impl FakeLauncher {
    fn new(turns: &[Turn]) -> Arc<Self> {
        Arc::new(Self {
            turns: Mutex::new(turns.iter().copied().collect()),
            started: Mutex::new(Vec::new()),
            exits: Arc::new(Mutex::new(Vec::new())),
        })
    }

    fn session(&self, index: usize) -> Arc<FakeSession> {
        self.started.lock().unwrap()[index].1.clone()
    }

    fn workspaces(&self) -> Vec<PathBuf> {
        let started = self.started.lock().unwrap();
        started.iter().map(|(path, _)| path.clone()).collect()
    }
}

impl SessionLauncher for FakeLauncher {
    fn launch(&self, workspace: &Path) -> std::io::Result<Launched> {
        let turn = self
            .turns
            .lock()
            .unwrap()
            .pop_front()
            .unwrap_or(Turn::Reply);
        let (process, router) = tokio::io::duplex(1 << 16);
        let (process_read, process_write) = tokio::io::split(process);
        let (router_read, router_write) = tokio::io::split(router);
        let front = Arc::new(AcpFrontEnd::new(
            Box::new(process_read),
            Box::new(process_write),
            workspace.to_path_buf(),
        ));
        let session = FakeSession::new(front.clone(), turn);
        self.started
            .lock()
            .unwrap()
            .push((workspace.to_path_buf(), session.clone()));
        let served = tokio::spawn(async move { front.run(session.as_ref()).await });
        let exits = self.exits.clone();
        Ok(Launched {
            reader: Box::new(router_read),
            writer: Box::new(router_write),
            exited: Box::pin(async move {
                let code = served.await.unwrap_or(-1);
                exits.lock().unwrap().push(code);
            }),
        })
    }
}

/// The prompts a session ran; the closing EOF adds both cancel hooks to every one.
fn prompts(session: &FakeSession) -> Vec<String> {
    session
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("prompt"))
        .collect()
}

/// A second folder that exists: the crate's `src`.
fn other_workspace() -> PathBuf {
    workspace().join("src")
}

async fn initialize(client: &mut Client) -> Value {
    client
        .request(
            0,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    client.until_response(0).await.1
}

async fn new_session(client: &mut Client, id: u64, cwd: &Path) -> String {
    client
        .request(id, "session/new", json!({"cwd":cwd,"mcpServers":[]}))
        .await;
    let (before, opened) = client.until_response(id).await;
    assert!(before.is_empty(), "nothing precedes the answer: {before:?}");
    let session = opened["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("{opened}"))
        .to_string();
    // Its command list (#676) follows the answer, under the router's id.
    let commands = client.next().await;
    assert_eq!(
        commands["params"]["update"]["sessionUpdate"], "available_commands_update",
        "{commands}"
    );
    assert_eq!(
        commands["params"]["sessionId"],
        session.as_str(),
        "{commands}"
    );
    session
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

/// Lines up to and including the next request the agent sends.
async fn until_request(client: &mut Client) -> (Vec<Value>, Value) {
    let mut before = Vec::new();
    loop {
        let message = client.next().await;
        if message.get("method").is_some() && message.get("id").is_some() {
            return (before, message);
        }
        before.push(message);
    }
}

/// Runs `script` against the router; returns the launcher to inspect.
async fn drive<F, Fut>(launcher: Arc<FakeLauncher>, default: Option<PathBuf>, script: F)
where
    F: FnOnce(Client) -> Fut,
    Fut: std::future::Future<Output = Client>,
{
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, user_write) = tokio::io::split(user);
    let client = Client {
        lines: BufReader::new(user_read).lines(),
        writer: user_write,
        transcript: Vec::new(),
    };
    let served = serve(
        Box::new(agent_read),
        Box::new(agent_write),
        launcher.clone(),
        default,
    );
    let script = async {
        let mut client = script(client).await;
        client.assert_no_extensions();
        // EOF ends every session; the router then closes its side.
        client.writer.shutdown().await.unwrap();
        while client.lines.next_line().await.unwrap().is_some() {}
    };
    let (code, ()) = tokio::time::timeout(std::time::Duration::from_secs(30), async {
        tokio::join!(served, script)
    })
    .await
    .expect("the router hung");
    assert_eq!(code, 0);
    // The router returned only after every session process had exited, cleanly.
    let started = launcher.started.lock().unwrap().len();
    assert_eq!(*launcher.exits.lock().unwrap(), vec![0; started]);
}

#[tokio::test]
async fn acp_two_sessions_get_their_own_ids_folders_and_updates() {
    let launcher = FakeLauncher::new(&[Turn::Reply, Turn::Reply]);
    drive(launcher.clone(), None, |mut client| async move {
        let init = initialize(&mut client).await;
        assert_eq!(
            init["result"]["agentCapabilities"]["sessionCapabilities"],
            json!({"close":{}}),
            "{init}"
        );
        let first = new_session(&mut client, 1, &workspace()).await;
        let second = new_session(&mut client, 2, &other_workspace()).await;
        assert_ne!(first, second);

        prompt(&mut client, 3, &second, "to the second").await;
        let (updates, done) = client.until_response(3).await;
        assert_eq!(texts(&updates), ["hello"]);
        assert!(
            updates
                .iter()
                .all(|update| update["params"]["sessionId"] == second.as_str()),
            "{updates:?}"
        );
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let title = client.next().await;
        assert_eq!(
            title["params"],
            json!({"sessionId":second,"update":{
            "sessionUpdate":"session_info_update","title":"to the second"}})
        );
        prompt(&mut client, 4, &first, "to the first").await;
        let (updates, _) = client.until_response(4).await;
        assert!(
            updates
                .iter()
                .all(|update| update["params"]["sessionId"] == first.as_str())
        );
        let title = client.next().await;
        assert_eq!(
            title["params"],
            json!({"sessionId":first,"update":{
            "sessionUpdate":"session_info_update","title":"to the first"}})
        );
        client
    })
    .await;
    assert_eq!(launcher.workspaces(), [workspace(), other_workspace()]);
    assert_eq!(prompts(&launcher.session(0)), ["prompt to the first"]);
    assert_eq!(prompts(&launcher.session(1)), ["prompt to the second"]);
}

/// The client names the folder: an existing absolute directory, or none when the
/// operator gave `--workspace`.
#[tokio::test]
async fn acp_session_new_takes_an_existing_absolute_folder() {
    let launcher = FakeLauncher::new(&[]);
    drive(launcher.clone(), None, |mut client| async move {
        initialize(&mut client).await;
        for (id, params) in [
            (1, json!({"cwd":"relative/dir","mcpServers":[]})),
            (2, json!({"cwd":"/nonexistent/p1-acp-test","mcpServers":[]})),
            (3, json!({"mcpServers":[]})),
        ] {
            client.request(id, "session/new", params).await;
            let (_, refused) = client.until_response(id).await;
            assert_eq!(refused["error"]["code"], -32602, "{refused}");
        }
        prompt(&mut client, 4, "never-created", "hi").await;
        let (_, unknown) = client.until_response(4).await;
        assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
        client
    })
    .await;
    assert!(launcher.workspaces().is_empty());

    let launcher = FakeLauncher::new(&[]);
    drive(
        launcher.clone(),
        Some(other_workspace()),
        |mut client| async move {
            initialize(&mut client).await;
            client
                .request(1, "session/new", json!({"mcpServers":[]}))
                .await;
            let (_, opened) = client.until_response(1).await;
            assert!(opened["result"]["sessionId"].is_string(), "{opened}");
            client
        },
    )
    .await;
    assert_eq!(launcher.workspaces(), [other_workspace()]);
}

/// An approval goes to its own session; cancelling one session leaves the other
/// running.
#[tokio::test]
async fn acp_approvals_and_cancel_stay_in_their_session() {
    let launcher = FakeLauncher::new(&[Turn::Ask, Turn::Reply]);
    drive(launcher.clone(), None, |mut client| async move {
        initialize(&mut client).await;
        let asking = new_session(&mut client, 1, &workspace()).await;
        let other = new_session(&mut client, 2, &other_workspace()).await;

        prompt(&mut client, 3, &asking, "read it").await;
        let (announced, asked) = until_request(&mut client).await;
        assert_eq!(asked["method"], "session/request_permission", "{asked}");
        assert_eq!(asked["params"]["sessionId"], asking.as_str());
        assert!(
            announced
                .iter()
                .all(|update| update["params"]["sessionId"] == asking.as_str())
        );

        // The other session answers while the first one waits for its approval.
        prompt(&mut client, 4, &other, "hi").await;
        let (_, done) = client.until_response(4).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");

        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":asking}}))
            .await;
        let (_, cancelled) = client.until_response(3).await;
        assert_eq!(cancelled["result"]["stopReason"], "cancelled", "{cancelled}");
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"cancelled"}}}))
            .await;

        // The cancelled session's neighbour still runs.
        prompt(&mut client, 5, &other, "again").await;
        let (_, again) = client.until_response(5).await;
        assert_eq!(again["result"]["stopReason"], "end_turn", "{again}");
        client
    })
    .await;
    assert!(matches!(
        *launcher.session(0).decision.lock().unwrap(),
        Some(Decision::Deny { .. })
    ));
    assert_eq!(prompts(&launcher.session(1)), ["prompt hi", "prompt again"]);
}

/// An answered approval reaches the session that asked.
#[tokio::test]
async fn acp_an_approval_answer_reaches_its_session() {
    let launcher = FakeLauncher::new(&[Turn::Reply, Turn::Ask]);
    drive(launcher.clone(), None, |mut client| async move {
        initialize(&mut client).await;
        let _idle = new_session(&mut client, 1, &workspace()).await;
        let asking = new_session(&mut client, 2, &other_workspace()).await;
        prompt(&mut client, 3, &asking, "read it").await;
        let (_, asked) = until_request(&mut client).await;
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        let (_, done) = client.until_response(3).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(
        *launcher.session(1).decision.lock().unwrap(),
        Some(Decision::Permit)
    );
    assert!(prompts(&launcher.session(0)).is_empty());
}

/// `session/close` cancels the held prompt and stops the session's work, answers once
/// the session is gone, and leaves the other session open.
#[tokio::test]
async fn acp_close_cancels_the_prompt_and_stops_its_work() {
    let launcher = FakeLauncher::new(&[Turn::StartWork, Turn::Reply]);
    drive(launcher.clone(), None, |mut client| async move {
        initialize(&mut client).await;
        let closing = new_session(&mut client, 1, &workspace()).await;
        let other = new_session(&mut client, 2, &other_workspace()).await;
        prompt(&mut client, 3, &closing, "go").await;
        let started = client.next().await;
        assert_eq!(texts(&[started]), ["work started"]);

        client
            .request(4, "session/close", json!({"sessionId":closing}))
            .await;
        let mut answered = Vec::new();
        while answered.len() < 2 {
            let message = client.next().await;
            if message.get("method").is_none() {
                answered.push(message);
            }
        }
        let prompt_answer = answered.iter().find(|m| m["id"] == 3).unwrap();
        assert_eq!(prompt_answer["result"]["stopReason"], "cancelled");
        let close_answer = answered.iter().find(|m| m["id"] == 4).unwrap();
        assert_eq!(close_answer["result"], json!({}), "{close_answer}");

        prompt(&mut client, 5, &closing, "after close").await;
        let (_, gone) = client.until_response(5).await;
        assert_eq!(gone["error"]["code"], -32602, "{gone}");
        prompt(&mut client, 6, &other, "still here").await;
        let (_, done) = client.until_response(6).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    let closed = launcher.session(0).calls();
    assert!(closed.contains(&"cancel_runs".to_string()), "{closed:?}");
    assert!(closed.contains(&"stop_workers".to_string()), "{closed:?}");
    assert_eq!(prompts(&launcher.session(1)), ["prompt still here"]);
}

/// `session/set_config_option` and its `config_option_update` (#675) pass through the
/// router with the client's session id, and switch only the session they name.
#[tokio::test]
async fn acp_a_config_change_reaches_its_session_only() {
    let launcher = FakeLauncher::new(&[Turn::Reply, Turn::Reply]);
    drive(launcher.clone(), None, |mut client| async move {
        initialize(&mut client).await;
        let first = new_session(&mut client, 1, &workspace()).await;
        let second = new_session(&mut client, 2, &other_workspace()).await;
        client
            .request(
                3,
                "session/set_config_option",
                json!({"sessionId":second,"configId":"model","value":"e/deep"}),
            )
            .await;
        let (updates, answered) = client.until_response(3).await;
        assert!(answered["result"]["configOptions"].is_array(), "{answered}");
        let update = updates
            .iter()
            .find(|update| update["params"]["update"]["sessionUpdate"] == "config_option_update")
            .unwrap_or_else(|| panic!("{updates:?}"));
        assert_eq!(update["params"]["sessionId"], second.as_str(), "{update}");
        assert_ne!(first, second);
        client
    })
    .await;
    let sets = |index| -> Vec<String> {
        launcher
            .session(index)
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("set "))
            .collect()
    };
    assert!(sets(0).is_empty());
    assert_eq!(sets(1), ["set Model e/deep"]);
}
