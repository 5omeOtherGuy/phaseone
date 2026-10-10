//! `p1 acp` against recorded wire fixtures (ADR-0154): the real host behind the ACP
//! adapter, on scripted providers, over an in-memory pipe.
//!
//! A fixture is one JSON object per line, `{"dir": "c2a"|"a2c", "msg": {...}}`. The
//! replay sends the `c2a` lines, collects what the agent writes, and compares the
//! whole transcript after normalising the session id, the workspace path and p1's
//! version.
//! `P1_ACP_RECORD=1` writes the transcript back instead (the `c2a` lines are the
//! script). No network, tempdirs only, no sleeps.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use common::{Harness, provider_hook, write_environment};
use p1_contracts::CancellationToken;
use p1_host::frontend::FrontEnd;
use p1_host::frontend_port::PortFrontEnd;
use p1_host::run::run_with_front_end;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const SESSION: &str = "<session>";
const WORKSPACE: &str = "<workspace>";
const VERSION: &str = "<version>";

fn fixture_path(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../docs/acp/fixtures/{name}.jsonl"))
}

fn read_fixture(path: &Path) -> Vec<(String, Value)> {
    std::fs::read_to_string(path)
        .unwrap()
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| {
            let entry: Value = serde_json::from_str(line).unwrap();
            (
                entry["dir"].as_str().unwrap().to_string(),
                entry["msg"].clone(),
            )
        })
        .collect()
}

/// Replace `from` by `to` in every string of `value`; an empty `from` (the session id
/// before `session/new` answered) replaces nothing.
fn replace(value: &Value, from: &str, to: &str) -> Value {
    if from.is_empty() {
        return value.clone();
    }
    match value {
        Value::String(text) => Value::String(text.replace(from, to)),
        Value::Array(items) => {
            Value::Array(items.iter().map(|item| replace(item, from, to)).collect())
        }
        Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(key, item)| (key.clone(), replace(item, from, to)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn is_request(message: &Value) -> bool {
    message.get("method").is_some() && message.get("id").is_some()
}

/// Runs the fixture's client lines against `p1 acp` and returns the transcript, with
/// the agent's lines normalised.
async fn transcript(
    fixture: &[(String, Value)],
    workspace: &Path,
    harness: &mut Harness,
) -> Vec<(String, Value)> {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, mut user_write) = tokio::io::split(user);
    let mut lines = BufReader::new(user_read).lines();
    let front_end = Arc::new(PortFrontEnd::new(Arc::new(
        p1_acp::driver::AcpFrontEnd::new(
            Box::new(agent_read),
            Box::new(agent_write),
            workspace.to_path_buf(),
        ),
    )));
    let options = p1_host::cli::parse(&[
        "acp".to_string(),
        "--env".to_string(),
        "plain".to_string(),
        "--workspace".to_string(),
        workspace.to_str().unwrap().to_string(),
    ])
    .unwrap();
    let workspace_text = workspace.to_str().unwrap().to_string();

    let client = async {
        let mut recorded = Vec::new();
        let mut session = String::new();
        let mut waiting: Option<Value> = None;
        for (dir, message) in fixture.iter().filter(|(dir, _)| dir == "c2a") {
            let message = replace(
                &replace(message, WORKSPACE, &workspace_text),
                SESSION,
                &session,
            );
            let mut line = serde_json::to_vec(&message).unwrap();
            line.push(b'\n');
            user_write.write_all(&line).await.unwrap();
            recorded.push((
                dir.clone(),
                replace(
                    &replace(&message, &workspace_text, WORKSPACE),
                    &session,
                    SESSION,
                ),
            ));
            if message.get("method").is_some() && message.get("id").is_some() {
                waiting = Some(message["id"].clone());
            }
            // Read until the outstanding request is answered or the agent asks
            // something the next client line answers.
            while let Some(id) = waiting.clone() {
                let line = lines
                    .next_line()
                    .await
                    .unwrap()
                    .expect("the agent closed early");
                let received: Value =
                    serde_json::from_str(&line).expect("every stdout line is JSON");
                assert_eq!(received["jsonrpc"], "2.0", "{received}");
                if let Some(id) = received["result"]["sessionId"].as_str() {
                    session = id.to_string();
                }
                let asks = is_request(&received);
                let answered = received.get("method").is_none() && received["id"] == id;
                let mut normal = replace(
                    &replace(&received, &workspace_text, WORKSPACE),
                    &session,
                    SESSION,
                );
                // A release changes p1's version, not the wire.
                if let Some(version) = normal.pointer_mut("/result/agentInfo/version") {
                    *version = json!(VERSION);
                }
                recorded.push(("a2c".to_string(), normal));
                if answered {
                    waiting = None;
                }
                if asks || answered {
                    break;
                }
            }
        }
        user_write.shutdown().await.unwrap();
        while lines.next_line().await.unwrap().is_some() {}
        recorded
    };
    let (code, recorded) = tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(
            run_with_front_end(
                &mut harness.deps,
                &options,
                CancellationToken::new(),
                front_end as Arc<dyn FrontEnd>,
            ),
            client
        )
    })
    .await
    .expect("p1 acp hung");
    assert_eq!(code.unwrap(), 0, "stderr: {}", harness.stderr.text());
    recorded
}

/// initialize, session/new, one prompt whose tool call is approved `allow_once`, the
/// tool's result, the answer and `end_turn`.
#[tokio::test]
async fn acp_fixture_prompt_tool_approval_replays() {
    let workspace = tempfile::tempdir().unwrap();
    let environments = tempfile::tempdir().unwrap();
    std::fs::write(
        workspace.path().join("input.txt"),
        "hello from the fixture\n",
    )
    .unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "read",
            r#"{"file_path":"input.txt"}"#,
        )]),
        text_response("input.txt says hello"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let path = fixture_path("prompt-tool-approval");
    let fixture = read_fixture(&path);
    let actual = transcript(&fixture, workspace.path(), &mut harness).await;

    // A client that declared no p1.dev receives no p1 extension.
    let text = serde_json::to_string(
        &actual
            .iter()
            .map(|(_, message)| message)
            .collect::<Vec<_>>(),
    )
    .unwrap();
    assert!(!text.contains("_p1") && !text.contains("p1.dev"), "{text}");
    // The host's own lines never reach the JSON-RPC stream.
    assert!(
        harness.stdout.text().is_empty(),
        "stdout: {}",
        harness.stdout.text()
    );

    if std::env::var_os("P1_ACP_RECORD").is_some() {
        let lines: Vec<String> = actual
            .iter()
            .map(|(dir, message)| json!({"dir": dir, "msg": message}).to_string())
            .collect();
        std::fs::write(&path, lines.join("\n") + "\n").unwrap();
        return;
    }
    assert_eq!(
        handle.requests().len(),
        2,
        "the tool ran and the turn ended"
    );
    assert_eq!(actual.len(), fixture.len(), "{actual:#?}");
    for (index, (expected, got)) in fixture.iter().zip(&actual).enumerate() {
        assert_eq!(expected, got, "fixture line {}", index + 1);
    }
}
