//! #695 at the driver: the transport runs before the session is served, so the client's
//! `mcpServers` reach the host while the `session/new` answer waits for the agent, and a
//! session that cannot start answers that request with the reason. A fake session stands in
//! for the host. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::FrontEndPort;
use p1_contracts::mcp::{ClientServer, Transport};
use p1_contracts::plan::{PlanEntry, PlanPriority, PlanStatus};
use serde_json::json;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncWriteExt, BufReader};

fn pipe() -> (Arc<AcpFrontEnd>, Client) {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, user_write) = tokio::io::split(user);
    let front = Arc::new(AcpFrontEnd::new(
        Box::new(agent_read),
        Box::new(agent_write),
        workspace(),
    ));
    let client = Client {
        lines: tokio::io::AsyncBufReadExt::lines(BufReader::new(user_read)),
        writer: user_write,
        transcript: Vec::new(),
    };
    (front, client)
}

fn servers() -> serde_json::Value {
    json!([
        {"name": "files", "command": "files-mcp", "args": ["--stdio"],
         "env": [{"name": "FILES_TOKEN", "value": "value-one"}]},
        {"type": "http", "name": "web", "url": "http://127.0.0.1:9/mcp",
         "headers": [{"name": "Authorization", "value": "Bearer value-two"}]},
    ])
}

async fn open(client: &mut Client) {
    client
        .request(
            0,
            "initialize",
            json!({"protocolVersion": 1, "clientCapabilities": {}}),
        )
        .await;
    // Answered before the session is served: the host assembles meanwhile.
    let (_, init) = client.until_response(0).await;
    assert_eq!(
        init["result"]["agentCapabilities"]["mcpCapabilities"],
        json!({"http": true, "sse": false})
    );
    client
        .request(
            1,
            "session/new",
            json!({"cwd": workspace(), "mcpServers": servers()}),
        )
        .await;
}

#[tokio::test]
async fn the_session_servers_reach_the_host_before_the_session_is_answered() {
    let (front, mut client) = pipe();
    let (received, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(front.session_servers(), open(&mut client))
    })
    .await
    .expect("the servers never arrived");
    assert_eq!(
        received,
        [
            ClientServer {
                name: "files".into(),
                transport: Transport::Stdio {
                    command: "files-mcp".into(),
                    args: vec!["--stdio".into()],
                    env: BTreeMap::from([("FILES_TOKEN".into(), "value-one".into())]),
                    cwd: None,
                },
            },
            ClientServer {
                name: "web".into(),
                transport: Transport::Http {
                    url: "http://127.0.0.1:9/mcp".into(),
                    headers: BTreeMap::from([("Authorization".into(), "Bearer value-two".into())]),
                },
            },
        ]
    );

    // The host announces a restored plan while it assembles: the session has an id by
    // now, but nothing goes out under it before the answer that gives the client the id.
    front.plan_updated(&[PlanEntry {
        content: "restored".into(),
        status: PlanStatus::Pending,
        priority: PlanPriority::Medium,
    }]);
    let session = FakeSession::new(front.clone(), Turn::Reply);
    let script = async {
        let (before, opened) = client.until_response(1).await;
        assert!(opened["result"]["sessionId"].is_string(), "{opened}");
        assert!(before.is_empty(), "nothing precedes the answer: {before:?}");
        client.writer.shutdown().await.unwrap();
        while client.lines.next_line().await.unwrap().is_some() {}
    };
    let (code, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(front.run(session.as_ref()), script)
    })
    .await
    .expect("the driver hung");
    assert_eq!(code, 0);
    let wire = serde_json::to_string(&client.transcript).unwrap();
    assert!(
        !wire.contains("value-one") && !wire.contains("value-two"),
        "no value goes back: {wire}"
    );
}

#[tokio::test]
async fn a_session_that_cannot_start_answers_its_request_with_the_reason() {
    let (front, mut client) = pipe();
    let script = async {
        open(&mut client).await;
        let (_, refused) = client.until_response(1).await;
        // The connection closes after the answer.
        while client.lines.next_line().await.unwrap().is_some() {}
        refused
    };
    let started = async {
        let servers = front.session_servers().await;
        assert_eq!(servers.len(), 2);
        front
            .start_failed("MCP server `files`: could not connect")
            .await;
    };
    let ((), refused) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(started, script)
    })
    .await
    .expect("the refusal hung");
    assert_eq!(refused["error"]["code"], -32603, "{refused}");
    assert_eq!(
        refused["error"]["message"],
        "p1 could not start the session: MCP server `files`: could not connect"
    );
    // A session that never started cannot be served afterwards.
    let session = FakeSession::new(front.clone(), Turn::Reply);
    assert_eq!(front.run(session.as_ref()).await, 1);
}

#[tokio::test]
async fn a_client_that_leaves_before_its_session_request_names_no_server() {
    let (front, mut client) = pipe();
    client.writer.shutdown().await.unwrap();
    let servers = tokio::time::timeout(Duration::from_secs(30), front.session_servers())
        .await
        .expect("the wait outlived the client");
    assert!(servers.is_empty());
}

fn stdio(name: &str) -> serde_json::Value {
    json!({"name": name, "command": "files-mcp"})
}

/// The `mcp …` calls the fake session took: one per swap, naming the new list.
fn swaps(session: &FakeSession) -> Vec<String> {
    session
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("mcp"))
        .collect()
}

/// #800: a re-attach with the session's list changes nothing; a different list is swapped in
/// before the answer; a swap that fails refuses the request with the reason and the session
/// keeps the list it had.
#[tokio::test]
async fn a_reattach_swaps_the_servers_only_when_its_list_differs() {
    let (front, client) = pipe();
    let session = FakeSession::new(front.clone(), Turn::Reply);
    let script = async {
        let mut client = client;
        let id = client.open().await;
        let reattach = |method: &str, servers: serde_json::Value| {
            json!({"method": method, "params":
                {"sessionId": id, "cwd": workspace(), "mcpServers": servers}})
        };
        let mut answers = Vec::new();
        for (request, message) in [
            (2, reattach("session/load", json!([]))),
            (3, reattach("session/resume", json!([stdio("files")]))),
            (4, reattach("session/load", json!([stdio("files")]))),
            (
                5,
                reattach("session/resume", json!([stdio("files"), stdio("broken")])),
            ),
            (6, reattach("session/resume", json!([stdio("files")]))),
        ] {
            let method = message["method"].as_str().unwrap();
            client
                .request(request, method, message["params"].clone())
                .await;
            let (_, answer) = client.until_response(request).await;
            answers.push(answer);
            if request == 4 {
                assert_eq!(swaps(&session), ["mcp files"], "an equal list is no swap");
            }
        }
        client.writer.shutdown().await.unwrap();
        while client.lines.next_line().await.unwrap().is_some() {}
        answers
    };
    let (code, answers) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(front.run(session.as_ref()), script)
    })
    .await
    .expect("the driver hung");
    assert_eq!(code, 0);
    for (index, answer) in answers.iter().enumerate() {
        if index == 3 {
            assert_eq!(answer["error"]["code"], -32603, "{answer}");
            assert_eq!(
                answer["error"]["message"],
                "p1 could not change the session's MCP servers: MCP server `broken`: could not \
                 connect"
            );
        } else {
            assert!(answer["result"]["sessionId"].is_string(), "{answer}");
        }
    }
    // The failed swap left `files` in place, so the last re-attach names the session's list.
    assert_eq!(swaps(&session), ["mcp files", "mcp files,broken"]);
}

/// #800: the swap takes the agent, which a running prompt holds, so a re-attach that changes
/// the servers is answered after the prompt, and swapped only then.
#[tokio::test]
async fn a_reattach_that_swaps_waits_for_the_running_prompt() {
    let (front, client) = pipe();
    let session = FakeSession::new(front.clone(), Turn::Ask);
    let script = async {
        let mut client = client;
        let id = client.open().await;
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId": id, "prompt": [{"type": "text", "text": "read it"}]}),
            )
            .await;
        let asked = loop {
            let message = client.next().await;
            if message["method"] == "session/request_permission" {
                break message;
            }
        };
        client
            .request(
                3,
                "session/load",
                json!({"sessionId": id, "cwd": workspace(), "mcpServers": [stdio("files")]}),
            )
            .await;
        // A barrier: an answer to the load would come before this one.
        client
            .request(
                4,
                "session/set_config_option",
                json!({"sessionId": id, "configId": "unknown", "value": "x"}),
            )
            .await;
        let (before, _) = client.until_response(4).await;
        assert!(
            before.iter().all(|message| message["id"] != 3),
            "{before:?}"
        );
        client
            .send(json!({"jsonrpc": "2.0", "id": asked["id"],
                "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}}))
            .await;
        let (before, loaded) = client.until_response(3).await;
        assert!(loaded["result"]["sessionId"].is_string(), "{loaded}");
        assert!(
            before
                .iter()
                .any(|message| message["id"] == 2 && message.get("result").is_some()),
            "the prompt is answered first: {before:?}"
        );
        client.writer.shutdown().await.unwrap();
        while client.lines.next_line().await.unwrap().is_some() {}
    };
    let (code, ()) = tokio::time::timeout(Duration::from_secs(30), async {
        tokio::join!(front.run(session.as_ref()), script)
    })
    .await
    .expect("the driver hung");
    assert_eq!(code, 0);
    let calls = session.calls();
    let prompt = calls.iter().position(|call| call == "prompt read it");
    let swap = calls.iter().position(|call| call == "mcp files");
    assert!(
        prompt.is_some() && swap.is_some() && prompt < swap,
        "{calls:?}"
    );
}
