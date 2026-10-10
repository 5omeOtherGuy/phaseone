//! Slash commands (#676): the `available_commands_update` after `session/new`, and a
//! `/name` prompt routed to its command instead of the model, through the driver
//! against the fake session. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, texts, workspace};
use p1_acp::commands::{Invocation, list, parse};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{CommandInfo, ConfigKind, FrontEndPort};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Runs `script` against the driver over the fake session; returns the session.
async fn drive<F, Fut>(turn: Turn, script: F) -> Arc<FakeSession>
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
    let session = FakeSession::new(front.clone(), turn);
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

/// The prompt `text`; returns what came before its answer, and the answer.
async fn say(client: &mut Client, id: u64, session: &str, text: &str) -> (Vec<Value>, Value) {
    client
        .request(
            id,
            "session/prompt",
            json!({"sessionId":session,"prompt":[{"type":"text","text":text}]}),
        )
        .await;
    client.until_response(id).await
}

fn kinds(messages: &[Value]) -> Vec<String> {
    messages
        .iter()
        .map(|message| {
            message["params"]["update"]["sessionUpdate"]
                .as_str()
                .unwrap_or("?")
                .to_string()
        })
        .collect()
}

/// What the session ran, without the hooks the closing EOF adds.
fn ran(session: &FakeSession) -> Vec<String> {
    session
        .calls()
        .into_iter()
        .filter(|call| call != "cancel_runs" && call != "stop_workers")
        .collect()
}

#[test]
fn a_command_is_an_advertised_name_alone_or_before_whitespace() {
    let command = |name: &str| CommandInfo {
        name: name.to_string(),
        description: String::new(),
        hint: None,
    };
    let commands = [command("model"), command("status"), command("modules")];
    assert_eq!(
        parse("/model e/deep", &commands),
        Some(Invocation::Setting {
            kind: ConfigKind::Model,
            argument: "e/deep".into()
        })
    );
    assert_eq!(
        parse("  /modules   reload \n", &commands),
        Some(Invocation::Host {
            name: "modules".into(),
            argument: "reload".into()
        })
    );
    assert_eq!(
        parse("/status", &commands),
        Some(Invocation::Host {
            name: "status".into(),
            argument: String::new()
        })
    );
    // Not a command: another name, a longer word, a path, plain text.
    for text in ["/models", "/statusx", "/usr/bin/env", "status", "/x y"] {
        assert_eq!(parse(text, &commands), None, "{text}");
    }
    // Each name once; `model` and `effort` are the driver's, offered or not.
    let listed = list(
        &[],
        vec![command("status"), command("model"), command("status")],
    );
    assert_eq!(listed, [command("status")]);
}

#[tokio::test]
async fn acp_session_new_is_followed_by_the_command_list() {
    drive(Turn::Reply, |mut client| async move {
        client
            .request(0, "initialize", json!({"protocolVersion":1,"clientCapabilities":{}}))
            .await;
        client.until_response(0).await;
        client
            .request(1, "session/new", json!({"cwd":workspace(),"mcpServers":[]}))
            .await;
        let (before, opened) = client.until_response(1).await;
        assert!(before.is_empty(), "nothing precedes the session's id: {before:?}");
        let id = opened["result"]["sessionId"].as_str().unwrap().to_string();
        let update = client.next().await;
        assert_eq!(update["method"], "session/update");
        assert_eq!(update["params"]["sessionId"], id.as_str());
        assert_eq!(
            update["params"]["update"],
            json!({"sessionUpdate":"available_commands_update","availableCommands":[
                {"name":"model","description":"Switch the model; without an argument, list the models","input":{"hint":"ENV/PROFILE"}},
                {"name":"effort","description":"Set the reasoning effort; without an argument, list the efforts","input":{"hint":"level"}},
                {"name":"status","description":"the status command"},
                {"name":"review","description":"the review command","input":{"hint":"what to review"}},
                {"name":"broken","description":"the broken command"}
            ]})
        );
        client
    })
    .await;
}

#[tokio::test]
async fn acp_a_host_command_reports_as_text_and_ends_the_turn() {
    let session = drive(Turn::Reply, |mut client| async move {
        let id = client.open().await;
        let (before, done) = say(&mut client, 2, &id, "/status").await;
        assert_eq!(texts(&before), ["model e/fast\n"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");

        let (before, done) = say(&mut client, 3, &id, "/broken").await;
        assert_eq!(texts(&before), ["/broken: it broke\n"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(ran(&session), ["command status ", "command broken "]);
}

#[tokio::test]
async fn acp_a_skill_command_is_a_turn_for_the_model() {
    let session = drive(Turn::Reply, |mut client| async move {
        let id = client.open().await;
        let (before, done) = say(&mut client, 2, &id, "/review the parser").await;
        assert_eq!(texts(&before), ["hello"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(
        ran(&session),
        [
            "command review the parser",
            "prompt use the review skill: the parser"
        ]
    );
}

#[tokio::test]
async fn acp_model_and_effort_lines_take_the_config_option_path() {
    let session = drive(Turn::Reply, |mut client| async move {
        let id = client.open().await;
        let (before, done) = say(&mut client, 2, &id, "/model").await;
        assert_eq!(texts(&before), ["* e/fast\n  e/deep\n  e/broken\n"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let title = client.next().await;
        assert_eq!(
            title["params"]["update"],
            json!({
                "sessionUpdate":"session_info_update", "title":"/model"
            })
        );

        let (before, _) = say(&mut client, 3, &id, "/model e/deep").await;
        assert_eq!(
            kinds(&before),
            ["config_option_update", "agent_message_chunk"]
        );
        assert_eq!(texts(&before), ["model: e/deep\n"]);

        let (before, _) = say(&mut client, 4, &id, "/effort high").await;
        assert_eq!(texts(&before), ["effort: high\n"]);

        let (before, _) = say(&mut client, 5, &id, "/effort max").await;
        assert_eq!(
            texts(&before),
            ["effort not changed: `max` is not a value of config option `thought_level`\n"]
        );
        let (before, _) = say(&mut client, 6, &id, "/model e/broken").await;
        assert_eq!(
            texts(&before),
            ["model not changed: e/broken does not assemble\n"]
        );
        client
    })
    .await;
    assert_eq!(
        ran(&session),
        ["set Model e/deep", "set Effort high", "set Model e/broken"]
    );
}

#[tokio::test]
async fn acp_an_unknown_slash_line_goes_to_the_model() {
    let session = drive(Turn::Reply, |mut client| async move {
        let id = client.open().await;
        let (before, done) = say(&mut client, 2, &id, "/x what is this").await;
        assert_eq!(texts(&before), ["hello"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(ran(&session), ["prompt /x what is this"]);
}

/// A command sent while a prompt runs waits for it, like any prompt, then runs
/// without the model.
#[tokio::test]
async fn acp_a_command_sent_during_a_prompt_runs_after_it() {
    let session = drive(Turn::Ask, |mut client| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let _announced = client.next().await;
        let asked = client.next().await;
        assert_eq!(asked["method"], "session/request_permission", "{asked}");
        client
            .request(3, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"/status"}]}))
            .await;
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        let (_, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let (before, done) = client.until_response(3).await;
        assert_eq!(texts(&before), ["model e/fast\n"]);
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        client
    })
    .await;
    assert_eq!(ran(&session), ["prompt read it", "command status "]);
}
