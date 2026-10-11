//! The `p1 acp` session driver over an in-memory pipe, against a fake session
//! handle: the host is not involved, so each case scripts exactly what the session
//! does and reads every line the driver writes. No network, no sleeps.

mod common;

use common::{Client, FakeSession, Turn, texts, workspace};
use p1_acp::driver::AcpFrontEnd;
use p1_contracts::CancellationToken;
use p1_contracts::Decision;
use p1_contracts::frontend::{BackgroundKind, FrontEndPort};
use p1_contracts::frontend::{Question, QuestionAnswer, QuestionOption, QuestionOutcome};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

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

fn operator_questions() -> Vec<Question> {
    vec![false, true]
        .into_iter()
        .map(|multi_select| Question {
            question: if multi_select {
                "Choose several"
            } else {
                "Choose one"
            }
            .into(),
            header: if multi_select { "Several" } else { "One" }.into(),
            multi_select,
            options: vec![
                QuestionOption {
                    label: "A".into(),
                    description: "First".into(),
                    preview: (!multi_select).then(|| "Preview A".into()),
                },
                QuestionOption {
                    label: "B".into(),
                    description: "Second".into(),
                    preview: None,
                },
            ],
        })
        .collect()
}

#[tokio::test]
async fn acp_question_form_and_answer_mapping() {
    drive(Turn::Reply, |mut client, session| async move {
        client.request(0, "initialize", json!({"protocolVersion":1,"clientCapabilities":{"elicitation":{"form":{}}}})).await;
        client.until_response(0).await;
        client.request(1, "session/new", json!({"cwd":workspace()})).await;
        let (_, opened) = client.until_response(1).await;
        client.next().await;
        let ask = session.front.ask_questions(Some("w7"), operator_questions(), CancellationToken::new());
        let reply = async {
            let request = client.next().await;
            assert_eq!(request["method"], "elicitation/create");
            assert_eq!(request["params"]["sessionId"], opened["result"]["sessionId"]);
            assert_eq!(request["params"]["mode"], "form");
            let schema = &request["params"]["requestedSchema"];
            assert_eq!(schema["type"], "object");
            assert_eq!(schema["title"], "[w7] Questions");
            assert_eq!(schema["properties"]["q0"]["oneOf"], json!([
                {"const":"A","title":"A","description":"First\n\nPreview A"},
                {"const":"B","title":"B","description":"Second"}
            ]));
            assert_eq!(schema["properties"]["q1"]["items"]["anyOf"], json!([
                {"const":"A","title":"A","description":"First"},
                {"const":"B","title":"B","description":"Second"}
            ]));
            assert_eq!(schema["properties"]["q0_text"]["type"], "string");
            client.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"action":"accept","content":{"q0_text":"Instead","q1":["B","A"],"q1_text":"Also"}}})).await;
        };
        let (outcome, ()) = tokio::join!(ask, reply);
        assert_eq!(outcome, QuestionOutcome::Answered(vec![
            QuestionAnswer { chosen: vec![], free_text: Some("Instead".into()) },
            QuestionAnswer { chosen: vec!["A".into(), "B".into()], free_text: Some("Also".into()) },
        ]));
        // Invalid, incomplete, unknown and dismissed answers never become answers.
        for result in [json!({"action":"decline"}), json!({"action":"cancel"}), json!({"action":"future"}), json!({"action":"accept"}), json!({"action":"accept","content":{"q0":"C","q1":["A"]}}), json!({"action":"accept","content":{"q0":"A","q1":["A","A"]}}), json!({"action":"accept","content":{"q0":"A","q1":[]}})] {
            let ask = session.front.ask_questions(None, operator_questions(), CancellationToken::new());
            let reply = async {
                let request = client.next().await;
                client.send(json!({"jsonrpc":"2.0","id":request["id"],"result":result})).await;
            };
            let (outcome, ()) = tokio::join!(ask, reply);
            assert_eq!(outcome, QuestionOutcome::Cancelled);
        }
        client
    }).await;
}

#[tokio::test]
async fn acp_question_cancel_and_disconnect() {
    drive(Turn::Reply, |mut client, session| async move {
        client.request(0, "initialize", json!({"protocolVersion":1,"clientCapabilities":{"elicitation":{"form":{}}}})).await;
        client.until_response(0).await;
        client.request(1, "session/new", json!({"cwd":workspace()})).await;
        client.until_response(1).await;
        client.next().await;
        let cancel = CancellationToken::new();
        let ask = session.front.ask_questions(None, operator_questions(), cancel.clone());
        let dismiss = async {
            let request = client.next().await;
            cancel.cancel();
            client.send(json!({"jsonrpc":"2.0","id":request["id"],"result":{"action":"accept","content":{"q0":"A","q1":["B"]}}})).await;
        };
        let (outcome, ()) = tokio::join!(ask, dismiss);
        assert_eq!(outcome, QuestionOutcome::Cancelled);
        let ask = session.front.ask_questions(None, operator_questions(), CancellationToken::new());
        let disconnect = async {
            client.next().await;
            client.writer.shutdown().await.unwrap();
        };
        let (outcome, ()) = tokio::join!(ask, disconnect);
        assert_eq!(outcome, QuestionOutcome::Cancelled);
        client
    }).await;
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

/// ACP-CANCEL-002: every update of a cancelled turn, its title included, precedes the
/// prompt's `cancelled` answer; nothing of the turn follows it.
#[tokio::test]
async fn acp_nothing_follows_a_cancelled_prompt_answer() {
    drive(Turn::Ask, |mut client, _| async move {
        let id = client.open().await;
        client
            .request(2, "session/prompt", json!({"sessionId":id,"prompt":[{"type":"text","text":"read it"}]}))
            .await;
        let (_, asked) = until_request(&mut client).await;
        client
            .send(json!({"jsonrpc":"2.0","method":"session/cancel","params":{"sessionId":id}}))
            .await;
        let (before, done) = client.until_response(2).await;
        assert_eq!(done["result"]["stopReason"], "cancelled", "{done}");
        assert_eq!(
            before.last().unwrap()["params"]["update"],
            json!({"sessionUpdate":"session_info_update","title":"read it"}),
            "{before:?}"
        );
        client
            .send(json!({"jsonrpc":"2.0","id":asked["id"],"result":{"outcome":{"outcome":"cancelled"}}}))
            .await;
        // A barrier: whatever the cancelled turn still sent would arrive before it.
        client
            .request(3, "session/set_config_option", json!({"sessionId":id,"configId":"unknown","value":"x"}))
            .await;
        let (after, _) = client.until_response(3).await;
        assert!(after.is_empty(), "{after:?}");
        client
    })
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

/// `session/load` and `session/resume` of the open session re-attach to it (TCK
/// ACP-LOAD-001..003, ACP-RESUME-001/002): load replays the loaded history and every
/// record committed since, resume replays nothing; both publish the command list
/// again and then answer the settings, so no update follows the answer (ACP-LOAD-002).
/// No second session opens.
#[tokio::test]
async fn acp_load_and_resume_of_the_open_session_reattach() {
    use p1_contracts::{JournalRecord, RecordBody};
    let user = |seq: u64, text: &str| JournalRecord {
        seq,
        body: RecordBody::UserInput { text: text.into() },
    };
    let session = drive(Turn::Reply, |mut client, session| async move {
        session.front.history_loaded(&[user(0, "earlier")]);
        client
            .request(0, "initialize", json!({"protocolVersion":1}))
            .await;
        client.until_response(0).await;
        client
            .request(
                1,
                "session/load",
                json!({"cwd":workspace(),"sessionId":"saved"}),
            )
            .await;
        let (mut before, loaded) = client.until_response(1).await;
        assert_eq!(before.len(), 2, "{before:?}");
        let id = loaded["result"]["sessionId"].as_str().unwrap().to_string();
        // The list is the last line before the answer.
        let commands = before.pop().unwrap();
        assert_eq!(
            commands["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        client
            .request(
                2,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"next"}]}),
            )
            .await;
        client.until_response(2).await;
        // What the host's journal accepted for that turn.
        session.front.record_committed(&user(1, "next"));

        client
            .request(
                3,
                "session/load",
                json!({"cwd":workspace(),"sessionId":id,"mcpServers":[]}),
            )
            .await;
        let (mut before, reloaded) = client.until_response(3).await;
        assert!(reloaded.get("error").is_none(), "{reloaded}");
        assert_eq!(before.pop(), Some(commands.clone()), "the list again");
        let replayed: Vec<_> = before
            .iter()
            .map(|m| {
                (
                    m["params"]["update"]["sessionUpdate"].clone(),
                    m["params"]["update"]["content"]["text"].clone(),
                )
            })
            .collect();
        assert_eq!(
            replayed,
            [
                (json!("user_message_chunk"), json!("earlier")),
                (json!("user_message_chunk"), json!("next")),
            ]
        );
        assert_eq!(
            reloaded["result"]["configOptions"],
            loaded["result"]["configOptions"]
        );

        client
            .request(
                4,
                "session/resume",
                json!({"cwd":workspace(),"sessionId":id}),
            )
            .await;
        let (before, resumed) = client.until_response(4).await;
        assert_eq!(before.len(), 1, "no replay: {before:?}");
        assert_eq!(before[0], commands, "the list again");
        assert_eq!(
            resumed["result"]["configOptions"],
            loaded["result"]["configOptions"]
        );

        // Only the open session re-attaches, and only in its own folder.
        for (request, method, params) in [
            (
                5,
                "session/load",
                json!({"cwd":workspace(),"sessionId":"other"}),
            ),
            (6, "session/new", json!({"cwd":workspace()})),
            (7, "session/fork", json!({"cwd":workspace(),"sessionId":id})),
            (
                8,
                "session/resume",
                json!({"cwd":workspace().join("src"),"sessionId":id}),
            ),
        ] {
            client.request(request, method, params).await;
            let (before, refused) = client.until_response(request).await;
            assert!(before.is_empty(), "{before:?}");
            assert!(refused.get("error").is_some(), "{method}: {refused}");
        }
        client
            .request(
                9,
                "session/prompt",
                json!({"sessionId":id,"prompt":[{"type":"text","text":"after"}]}),
            )
            .await;
        let (before, done) = client.until_response(9).await;
        assert_eq!(texts(&before), ["hello"]);
        assert_eq!(done["result"]["stopReason"], "end_turn");
        client
    })
    .await;
    let prompts: Vec<_> = session
        .calls()
        .into_iter()
        .filter(|call| call.starts_with("prompt"))
        .collect();
    assert_eq!(prompts, ["prompt next", "prompt after"]);
}

#[tokio::test]
async fn acp_load_replays_pre_compaction_turns_and_interrupted_text_before_reply() {
    use p1_contracts::{InterruptionReason, Item, JournalRecord, RecordBody};
    drive(Turn::Reply, |mut client, session| async move {
        let bodies = [
            RecordBody::UserInput {
                text: "original turn".into(),
            },
            RecordBody::ContextReplaced {
                items: vec![Item::User {
                    text: "model-only summary".into(),
                }],
                usage: None,
            },
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::Cancelled,
                partial_text: "visible partial answer".into(),
                error: None,
            },
        ];
        session.front.history_loaded(
            &bodies
                .into_iter()
                .enumerate()
                .map(|(seq, body)| JournalRecord {
                    seq: seq as u64,
                    body,
                })
                .collect::<Vec<_>>(),
        );
        client
            .request(0, "initialize", json!({"protocolVersion":1}))
            .await;
        client.until_response(0).await;
        client
            .request(
                1,
                "session/load",
                json!({"cwd":workspace(),"sessionId":"saved"}),
            )
            .await;
        let (before, loaded) = client.until_response(1).await;
        assert!(loaded.get("error").is_none());
        assert_eq!(before.len(), 3, "the replay, then the command list");
        assert_eq!(
            before[2]["params"]["update"]["sessionUpdate"],
            "available_commands_update"
        );
        assert_eq!(
            before[0]["params"]["update"]["sessionUpdate"],
            "user_message_chunk"
        );
        assert_eq!(
            before[0]["params"]["update"]["content"]["text"],
            "original turn"
        );
        assert_eq!(
            before[1]["params"]["update"]["content"]["text"],
            "visible partial answer"
        );
        assert!(
            !serde_json::to_string(&before)
                .unwrap()
                .contains("model-only summary")
        );
        assert!(session.calls().is_empty(), "replay is not a prompt");
        client
    })
    .await;
}

#[tokio::test]
async fn acp_titles_precede_answers_and_only_republish_on_change() {
    drive(Turn::Reply, |mut client, session| async move {
        let id = client.open().await;
        for (request, text, expected) in [
            (2, "  Fix\n the   parser  ", Some("Fix the parser")),
            (3, "unrelated second prompt", None),
            (4, "third", Some("Renamed by host")),
            (5, "fourth", None),
        ] {
            if request == 4 {
                *session.title.lock().unwrap() = Some("Renamed by host".to_string());
            }
            client
                .request(
                    request,
                    "session/prompt",
                    json!({"sessionId":id,
                "prompt":[{"type":"text","text":text}]}),
                )
                .await;
            let (mut before, done) = client.until_response(request).await;
            assert_eq!(done["result"]["stopReason"], "end_turn");
            // The title, when it changed, is the turn's last update before its answer.
            if let Some(expected) = expected {
                let update = before.pop().unwrap();
                assert_eq!(
                    update["params"],
                    json!({"sessionId":id,"update":{
                    "sessionUpdate":"session_info_update","title":expected}})
                );
            }
            assert_eq!(texts(&before), ["hello"]);
            assert!(
                before
                    .iter()
                    .all(|m| m["params"]["update"]["sessionUpdate"] != "session_info_update")
            );
        }
        // A barrier catches an unwanted duplicate after the final prompt answer.
        client
            .request(
                6,
                "session/set_config_option",
                json!({"sessionId":id,
            "configId":"unknown","value":"x"}),
            )
            .await;
        let (before, _) = client.until_response(6).await;
        assert!(before.is_empty(), "{before:?}");
        client
    })
    .await;
}

/// Character limits must not split UTF-8 or shorten a word that fits exactly.
#[tokio::test]
async fn acp_title_fallback_is_bounded_unicode_opening_words() {
    for (text, expected) in [
        ("é".repeat(80), "é".repeat(80)),
        (format!("{} next", "é".repeat(80)), "é".repeat(80)),
        (format!("{} suffixlong", "é".repeat(75)), "é".repeat(75)),
        ("🦀".repeat(81), "🦀".repeat(80)),
        (" \n\t".to_string(), "Untitled session".to_string()),
    ] {
        drive(Turn::Reply, |mut client, _| async move {
            let id = client.open().await;
            client
                .request(
                    2,
                    "session/prompt",
                    json!({"sessionId":id,
                "prompt":[{"type":"text","text":text}]}),
                )
                .await;
            let (before, _) = client.until_response(2).await;
            let update = before.last().unwrap();
            assert_eq!(update["params"]["update"]["title"], expected);
            client
        })
        .await;
    }
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
