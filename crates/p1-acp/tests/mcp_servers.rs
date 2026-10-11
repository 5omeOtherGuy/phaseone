//! #695 at the driver: the transport runs before the session is served, so the client's
//! `mcpServers` reach the host while the `session/new` answer waits for the agent, and a
//! session that cannot start answers that request with the reason. A fake session stands in
//! for the host. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::FrontEndPort;
use p1_contracts::mcp::{ClientServer, Transport};
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

    let session = FakeSession::new(front.clone(), Turn::Reply);
    let script = async {
        let (_, opened) = client.until_response(1).await;
        assert!(opened["result"]["sessionId"].is_string(), "{opened}");
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
