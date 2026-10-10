//! The permission mode (#696) as an ACP config option and as the legacy session
//! modes: `session/new` offers both, `session/set_mode` and
//! `session/set_config_option` take the one path, and a change applies at once, even
//! during a prompt, announced after its answer. Through the driver against the fake
//! session. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::FrontEndPort;
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Runs `script` against the driver over a fake session that offers modes when
/// `modes`; returns the session.
async fn drive<F, Fut>(turn: Turn, modes: bool, script: F) -> Arc<FakeSession>
where
    F: FnOnce(Client) -> Fut,
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
    let session = if modes {
        FakeSession::with_modes(front.clone(), turn)
    } else {
        FakeSession::new(front.clone(), turn)
    };
    let client = Client {
        lines: BufReader::new(user_read).lines(),
        writer: user_write,
        transcript: Vec::new(),
    };
    let served = front.run(session.as_ref());
    let script = async {
        let mut client = script(client).await;
        client.assert_no_extensions();
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

async fn opened(client: &mut Client) -> (String, Value) {
    client
        .request(
            0,
            "initialize",
            json!({"protocolVersion":1,"clientCapabilities":{}}),
        )
        .await;
    client.until_response(0).await;
    client
        .request(1, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
        .await;
    let (_, opened) = client.until_response(1).await;
    let commands = client.next().await;
    assert_eq!(
        commands["params"]["update"]["sessionUpdate"], "available_commands_update",
        "{commands}"
    );
    let names: Vec<_> = commands["params"]["update"]["availableCommands"]
        .as_array()
        .unwrap()
        .iter()
        .map(|command| command["name"].clone())
        .collect();
    assert!(
        !names.contains(&json!("mode")),
        "no command for the mode: {names:?}"
    );
    let id = opened["result"]["sessionId"].as_str().unwrap().to_string();
    (id, opened["result"].clone())
}

fn mode_option(options: &Value) -> &Value {
    options
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == "mode")
        .unwrap_or_else(|| panic!("no mode option in {options}"))
}

fn sets(session: &FakeSession) -> Vec<String> {
    session
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("set "))
        .collect()
}

#[tokio::test]
async fn acp_session_new_offers_the_mode_as_an_option_and_as_modes() {
    drive(Turn::Reply, true, |mut client| async move {
        let (_, result) = opened(&mut client).await;
        assert_eq!(
            mode_option(&result["configOptions"]),
            &json!({"id":"mode","name":"Mode","category":"mode","type":"select",
                "currentValue":"ask","options":[
                    {"value":"ask","name":"ask"},
                    {"value":"read-only","name":"read-only"},
                    {"value":"full-access","name":"full-access"}]})
        );
        assert_eq!(
            result["modes"],
            json!({"currentModeId":"ask","availableModes":[
                {"id":"ask","name":"ask"},
                {"id":"read-only","name":"read-only"},
                {"id":"full-access","name":"full-access"}]})
        );
        client
    })
    .await;
}

#[tokio::test]
async fn acp_set_mode_answers_then_announces_option_and_mode() {
    let session = drive(Turn::Reply, true, |mut client| async move {
        let (id, _) = opened(&mut client).await;
        client
            .request(2, "session/set_mode", json!({"sessionId":id,"modeId":"read-only"}))
            .await;
        let (before, done) = client.until_response(2).await;
        assert!(before.is_empty(), "the answer comes first: {before:?}");
        assert_eq!(done["result"], json!({}), "{done}");
        let option = client.next().await;
        assert_eq!(option["params"]["update"]["sessionUpdate"], "config_option_update");
        assert_eq!(
            mode_option(&option["params"]["update"]["configOptions"])["currentValue"],
            "read-only"
        );
        let mode = client.next().await;
        assert_eq!(
            mode["params"],
            json!({"sessionId":id,"update":{"sessionUpdate":"current_mode_update","currentModeId":"read-only"}})
        );

        // The option's own door: the whole list, then the same two updates.
        client
            .request(
                3,
                "session/set_config_option",
                json!({"sessionId":id,"configId":"mode","value":"full-access"}),
            )
            .await;
        let (_, done) = client.until_response(3).await;
        assert_eq!(
            mode_option(&done["result"]["configOptions"])["currentValue"],
            "full-access"
        );
        let _option = client.next().await;
        let mode = client.next().await;
        assert_eq!(mode["params"]["update"]["currentModeId"], "full-access");

        for (request, params) in [
            (4, json!({"sessionId":id,"modeId":"danger-full-access"})),
            (5, json!({"sessionId":id})),
            (6, json!({"sessionId":"nobody","modeId":"ask"})),
        ] {
            client.request(request, "session/set_mode", params).await;
            let (_, refused) = client.until_response(request).await;
            assert_eq!(refused["error"]["code"], -32602, "{refused}");
        }
        client
    })
    .await;
    assert_eq!(
        sets(&session),
        ["set Mode read-only", "set Mode full-access"]
    );
}

/// The mode needs nothing of the running turn: it changes at once, so the turn's
/// next tool call already sees it.
#[tokio::test]
async fn acp_a_mode_change_during_a_prompt_applies_at_once() {
    let session = drive(Turn::Ask, true, |mut client| async move {
        let (id, _) = opened(&mut client).await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let _announced = client.next().await;
        let asked = client.next().await;
        assert_eq!(asked["method"], "session/request_permission", "{asked}");
        client
            .request(3, "session/set_mode", json!({"sessionId":id,"modeId":"read-only"}))
            .await;
        let (_, done) = client.until_response(3).await;
        assert_eq!(done["result"], json!({}));
        let _option = client.next().await;
        let mode = client.next().await;
        assert_eq!(mode["params"]["update"]["currentModeId"], "read-only");
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        let (_, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    let calls = session.calls();
    let set = calls.iter().position(|call| call == "set Mode read-only");
    let prompt = calls.iter().position(|call| call == "prompt read it");
    assert!(set.is_some() && prompt < set, "{calls:?}");
}

#[tokio::test]
async fn acp_a_session_without_modes_offers_none_and_refuses_set_mode() {
    drive(Turn::Reply, false, |mut client| async move {
        let (id, result) = opened(&mut client).await;
        assert!(result.get("modes").is_none(), "{result}");
        client
            .request(
                2,
                "session/set_mode",
                json!({"sessionId":id,"modeId":"ask"}),
            )
            .await;
        let (_, refused) = client.until_response(2).await;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");
        client
    })
    .await;
}
