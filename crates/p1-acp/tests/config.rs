//! The session's settings as ACP config options (#675): the option list's wire
//! shape, and `session/set_config_option` through the driver against the fake
//! session. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, workspace};
use p1_acp::codec::Codec;
use p1_acp::config_options::{Refusal, validate};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::frontend::{ConfigChoice, ConfigKind, ConfigValue, FrontEndPort};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

fn choice(kind: ConfigKind, current: &str, values: &[(&str, Option<&str>)]) -> ConfigChoice {
    ConfigChoice {
        kind,
        current: current.to_string(),
        values: values
            .iter()
            .map(|(value, description)| ConfigValue {
                value: value.to_string(),
                name: value.to_string(),
                description: description.map(str::to_string),
            })
            .collect(),
    }
}

#[test]
fn options_are_selects_with_acp_ids_and_categories() {
    let choices = [
        choice(
            ConfigKind::Model,
            "deepseek/v4",
            &[("deepseek/v4", Some("route deepseek")), ("gpt/sol", None)],
        ),
        choice(ConfigKind::Effort, "high", &[("low", None), ("high", None)]),
    ];
    let codec = Codec::negotiate(1);
    assert_eq!(
        codec.encode_config_options(&choices),
        json!([
            {"id":"model","name":"Model","category":"model","type":"select",
             "currentValue":"deepseek/v4","options":[
                {"value":"deepseek/v4","name":"deepseek/v4","description":"route deepseek"},
                {"value":"gpt/sol","name":"gpt/sol"}]},
            {"id":"thought_level","name":"Thought level","category":"thought_level",
             "type":"select","currentValue":"high","options":[
                {"value":"low","name":"low"},{"value":"high","name":"high"}]}
        ])
    );
    assert_eq!(
        codec.encode_config_update(&choices[1..]),
        json!({"sessionUpdate":"config_option_update","configOptions":[
            {"id":"thought_level","name":"Thought level","category":"thought_level",
             "type":"select","currentValue":"high","options":[
                {"value":"low","name":"low"},{"value":"high","name":"high"}]}]})
    );
    assert_eq!(
        validate(&choices, "thought_level", "high"),
        Ok(ConfigKind::Effort)
    );
    assert!(matches!(
        validate(&choices, "thought_level", "max"),
        Err(Refusal::UnknownValue { .. })
    ));
    assert!(matches!(
        validate(&choices, "mode", "ask"),
        Err(Refusal::UnknownOption(_))
    ));
}

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

async fn set(client: &mut Client, id: u64, session: &str, option: &str, value: &str) {
    client
        .request(
            id,
            "session/set_config_option",
            json!({"sessionId":session,"configId":option,"value":value}),
        )
        .await;
}

fn current(options: &Value, id: &str) -> String {
    options
        .as_array()
        .unwrap()
        .iter()
        .find(|option| option["id"] == id)
        .unwrap_or_else(|| panic!("no {id} in {options}"))["currentValue"]
        .as_str()
        .unwrap()
        .to_string()
}

/// The prompts and setting changes; the closing EOF adds both cancel hooks.
fn sets(session: &FakeSession) -> Vec<String> {
    session
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("set "))
        .collect()
}

fn updates(messages: &[Value]) -> Vec<&Value> {
    messages
        .iter()
        .filter(|m| m["params"]["update"]["sessionUpdate"] == "config_option_update")
        .collect()
}

#[tokio::test]
async fn acp_session_new_carries_the_options_and_a_set_switches_now() {
    let session = drive(Turn::Reply, |mut client| async move {
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
        let id = opened["result"]["sessionId"].as_str().unwrap().to_string();
        let options = &opened["result"]["configOptions"];
        assert_eq!(current(options, "model"), "e/fast");
        assert_eq!(current(options, "thought_level"), "low");

        set(&mut client, 2, &id, "model", "e/deep").await;
        let (before, done) = client.until_response(2).await;
        assert!(
            updates(&before).is_empty(),
            "the answer comes first: {before:?}"
        );
        let options = &done["result"]["configOptions"];
        assert_eq!(current(options, "model"), "e/deep", "{done}");
        // The new model's efforts are offered at once.
        assert_eq!(
            options[1]["options"].as_array().unwrap().len(),
            2,
            "{options}"
        );
        // Then the change is announced.
        let announced = client.next().await;
        assert_eq!(
            announced["params"]["update"]["sessionUpdate"],
            "config_option_update"
        );
        assert_eq!(announced["params"]["sessionId"], id.as_str());
        assert_eq!(
            current(&announced["params"]["update"]["configOptions"], "model"),
            "e/deep"
        );

        set(&mut client, 3, &id, "thought_level", "high").await;
        let (_, done) = client.until_response(3).await;
        assert_eq!(
            current(&done["result"]["configOptions"], "thought_level"),
            "high"
        );
        client
    })
    .await;
    assert_eq!(sets(&session), ["set Model e/deep", "set Effort high"]);
}

#[tokio::test]
async fn acp_an_unknown_option_or_value_is_invalid_params_and_a_failed_switch_changes_nothing() {
    let session = drive(Turn::Reply, |mut client| async move {
        let id = client.open().await;
        for (request, option, value) in [
            (2, "mode", "ask"),
            (3, "model", "e/missing"),
            (4, "thought_level", "high"),
        ] {
            set(&mut client, request, &id, option, value).await;
            let (_, refused) = client.until_response(request).await;
            assert_eq!(refused["error"]["code"], -32602, "{refused}");
        }
        set(&mut client, 5, &id, "model", "e/broken").await;
        let (before, failed) = client.until_response(5).await;
        assert!(
            failed["error"]["message"]
                .as_str()
                .unwrap()
                .contains("e/broken"),
            "{failed}"
        );
        assert!(updates(&before).is_empty());
        client
    })
    .await;
    assert_eq!(sets(&session), ["set Model e/broken"]);
}

/// A change asked for during a prompt answers at once and applies before the next
/// prompt, then is announced.
#[tokio::test]
async fn acp_a_set_during_a_prompt_applies_on_the_next_turn() {
    let session = drive(Turn::Ask, |mut client| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let _announced = client.next().await;
        let asked = client.next().await;
        assert_eq!(asked["method"], "session/request_permission", "{asked}");

        set(&mut client, 3, &id, "model", "e/deep").await;
        let (_, answered) = client.until_response(3).await;
        assert_eq!(current(&answered["result"]["configOptions"], "model"), "e/deep");

        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        let (before, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        // The change applies once the prompt is over; its announcement may overtake the
        // prompt's answer, which travels through the transport's handler task.
        let announced = match updates(&before).first() {
            Some(update) => (*update).clone(),
            None => client.next().await,
        };
        assert_eq!(announced["params"]["update"]["sessionUpdate"], "config_option_update", "{announced}");
        assert_eq!(current(&announced["params"]["update"]["configOptions"], "model"), "e/deep");
        client
    })
    .await;
    let calls = session.calls();
    assert_eq!(
        calls[..2],
        ["prompt read it", "set Model e/deep"],
        "{calls:?}"
    );
}

/// Answers during a prompt agree with each other: they show every change already
/// waiting, a waiting model switch replaces a waiting effort and drops the effort
/// option (the new model decides its efforts), and an effort is refused until it ran.
#[tokio::test]
async fn acp_changes_waiting_for_a_prompt_answer_what_the_next_turn_runs() {
    let session = drive(Turn::Ask, |mut client| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let _announced = client.next().await;
        let asked = client.next().await;
        assert_eq!(asked["method"], "session/request_permission", "{asked}");

        set(&mut client, 3, &id, "thought_level", "low").await;
        let (_, answered) = client.until_response(3).await;
        assert_eq!(current(&answered["result"]["configOptions"], "thought_level"), "low");

        set(&mut client, 4, &id, "model", "e/deep").await;
        let (_, answered) = client.until_response(4).await;
        let options = &answered["result"]["configOptions"];
        assert_eq!(current(options, "model"), "e/deep");
        assert_eq!(options.as_array().unwrap().len(), 1, "{options}");

        set(&mut client, 5, &id, "thought_level", "high").await;
        let (_, refused) = client.until_response(5).await;
        assert_eq!(refused["error"]["code"], -32602, "{refused}");

        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"selected","optionId":"allow_once"}}}))
            .await;
        let (before, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "end_turn", "{done}");
        let announced = match updates(&before).first() {
            Some(update) => (*update).clone(),
            None => client.next().await,
        };
        let options = &announced["params"]["update"]["configOptions"];
        assert_eq!(current(options, "model"), "e/deep");
        assert_eq!(current(options, "thought_level"), "low");
        client
    })
    .await;
    assert_eq!(sets(&session), ["set Model e/deep"]);
}
