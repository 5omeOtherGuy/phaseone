//! Issue #142 end to end: credential-shaped strings in TOOL OUTPUT never reach the
//! history, the journal, a summary or a later request, and `read` refuses known
//! credential files. Every key shape is built at runtime — no key-shaped literal is
//! ever committed. Fake providers, tempfile workspaces: no network, no sleeps.

mod common;

use std::fs;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{AssistantBlock, Item, RecordBody, ReplayData, StopReason, StreamEvent};
use p1_testkit::{
    ScriptedProvider, Step, completed, json_call, origin, text_response, tool_call_response,
};
use tempfile::tempdir;

/// A key-shaped value built at runtime: never a literal in the tree.
fn key(prefix: &str, length: usize) -> String {
    format!("{prefix}{}", "A".repeat(length))
}

/// The initial `read`-able file and environment every test uses.
fn plain_read_environment(environments: &std::path::Path) {
    write_environment(
        environments,
        "plain",
        "fake",
        "fake-model",
        &["read", "shell"],
        "test",
    );
}

fn tool_results(history: &[Item]) -> Vec<&str> {
    history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.as_str()),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------- tool output is masked

#[tokio::test]
async fn a_shell_result_with_each_pattern_family_is_masked_in_history_and_journal() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_read_environment(environments.path());
    let session_path = workspace.path().join("session.jsonl");

    // Every family in one file, so the command line itself never carries a secret.
    let plain = key("sk-", 24);
    let anthropic = key("sk-ant-", 24);
    let project = key("sk-proj-", 24);
    let bearer = key("", 40);
    let authorization = key("", 32);
    let json_value = key("", 20);
    let credentials = format!(
        "{plain}\n{anthropic}\n{project}\nBearer {bearer}\nAuthorization: {authorization}\n\
         {{\"api_key\": \"{json_value}\"}}\n"
    );
    fs::write(workspace.path().join("creds.txt"), &credentials).unwrap();

    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            "{\"command\":\"cat creds.txt\"}",
        )]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // (a) the tool's returned result the provider saw holds only the masked form.
    let requests = handle.requests();
    let results = tool_results(&requests.last().unwrap().history);
    assert_eq!(results.len(), 1, "{results:?}");
    let result = results[0];
    for secret in [
        &plain,
        &anthropic,
        &project,
        &bearer,
        &authorization,
        &json_value,
    ] {
        assert!(!result.contains(secret.as_str()), "leak in {result}");
    }
    for marker in [
        "<redacted:sk-:24 chars>",
        "<redacted:sk-ant-:24 chars>",
        "<redacted:sk-proj-:24 chars>",
        "<redacted:Bearer:40 chars>",
        "<redacted:Authorization:32 chars>",
        "<redacted:api_key:20 chars>",
    ] {
        assert!(result.contains(marker), "missing {marker} in {result}");
    }

    // (b) the whole journal holds only the masked form, and the ToolFinished record
    // carries exactly what history carries.
    let journal_text = fs::read_to_string(&session_path).unwrap();
    for secret in [
        &plain,
        &anthropic,
        &project,
        &bearer,
        &authorization,
        &json_value,
    ] {
        assert!(
            !journal_text.contains(secret.as_str()),
            "the journal leaked a value"
        );
    }
    assert!(journal_text.contains("<redacted:sk-ant-:24 chars>"));
    let loaded = p1_journal::load(&session_path).unwrap();
    let finished: Vec<&str> = loaded
        .records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::ToolFinished { result } => Some(result.content.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        finished
            .iter()
            .any(|content| content.contains("<redacted:sk-ant-:24 chars>")),
        "{finished:?}"
    );

    // (c) the per-turn count is reported through the existing display-only notice
    // path; only the count, never a value.
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("masked 6 credential-shaped value(s) in tool output"),
        "stderr: {stderr}"
    );
}

// ----------------------------------------------------------- summaries are masked

const CONTEXT_TABLE: &str = "[context]\nwindow_tokens = 100000\noutput_headroom_tokens = 1000\nsummarize_at_tokens = 580\nkeep_recent_tokens = 20\nuser_verbatim_tokens = 100\n";

fn write_context_environment(root: &std::path::Path) {
    let dir = root.join("small");
    std::fs::create_dir_all(&dir).unwrap();
    let toml = format!(
        "family = \"small\"\nprovider = \"fake\"\nmodel = \"fake-model\"\n\n{CONTEXT_TABLE}"
    );
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), "test").unwrap();
}

#[tokio::test]
async fn a_summary_carrying_a_key_reaches_history_masked() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path());

    // The summarizer is the third request; its answer carries a runtime-built key.
    let secret = key("sk-", 26);
    let summary_answer = format!("## Decisions\nkeep {secret}\n");
    let provider = ScriptedProvider::new(vec![
        text_response(&"a".repeat(2_000)),
        text_response(&"b".repeat(50)),
        text_response(&summary_answer),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "three", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "small",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // The summary became a history item; every later request carries only the mask.
    let requests = handle.requests();
    let after = requests
        .iter()
        .skip_while(|request| !request.system_prompt.contains("summar"))
        .nth(1)
        .expect("a request after the summarization");
    let summary_items: Vec<&str> = after
        .history
        .iter()
        .filter_map(|item| match item {
            Item::User { text } if text.contains("p1 context summary") => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        !summary_items.is_empty(),
        "the summary item is in the history"
    );
    for text in summary_items {
        assert!(!text.contains(&secret), "the summary leaked the value");
        assert!(text.contains("<redacted:sk-:26 chars>"), "{text}");
    }
}

// --------------------------------------------------- signatures are never rewritten

#[tokio::test]
async fn a_thinking_signature_item_is_left_verbatim() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    plain_read_environment(environments.path());
    fs::write(workspace.path().join("notes.txt"), "ordinary\n").unwrap();
    let session_path = workspace.path().join("session.jsonl");

    // A long base64 blob whose item type is a signature: opaque continuation data.
    let signature = format!("{}{}", "QWxsQWJjRGVm".repeat(8), "Z2hp");
    let replay = ReplayData {
        origin: origin(),
        version: 1,
        payload: p1_contracts::serde_json::json!({ "type": "thinking", "signature": signature }),
    };
    let outcome = completed(
        vec![
            AssistantBlock::Reasoning {
                text: "thinking".into(),
                replay: Some(replay.clone()),
            },
            AssistantBlock::ToolCall(json_call("c1", "read", "{\"file_path\":\"notes.txt\"}")),
        ],
        StopReason::ToolUse,
        None,
    );
    let provider = ScriptedProvider::new(vec![
        Step::Events(vec![StreamEvent::Finished(outcome)]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session_path.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    let assistant = requests
        .last()
        .unwrap()
        .history
        .iter()
        .find_map(|item| match item {
            Item::Assistant(assistant) => Some(assistant),
            _ => None,
        })
        .expect("the signed assistant item is in the later request");
    let carried = assistant
        .blocks
        .iter()
        .find_map(|block| match block {
            AssistantBlock::Reasoning { replay, .. } => replay.clone(),
            _ => None,
        })
        .expect("the reasoning block survives");
    assert_eq!(carried, replay, "the signature item must be verbatim");

    let journal = fs::read_to_string(&session_path).unwrap();
    assert!(
        journal.contains(&signature),
        "the journal must carry the signature verbatim"
    );
    assert!(
        !journal.contains("<redacted:sk-"),
        "a signature blob must not be rewritten as a key"
    );
}

// ------------------------------------------------------- read refuses credentials

#[tokio::test]
async fn read_refuses_a_credential_file_and_still_reads_an_ordinary_one() {
    // One temp dir is both the workspace and the injected home: the credential file
    // is INSIDE the confinement, so only the refusal can stop it.
    let home = tempdir().unwrap();
    fs::create_dir_all(home.path().join(".codex")).unwrap();
    fs::write(home.path().join(".codex/auth.json"), "{}\n").unwrap();
    let environments = tempdir().unwrap();
    plain_read_environment(environments.path());
    fs::create_dir_all(home.path().join(".config/keys")).unwrap();
    fs::write(home.path().join(".config/keys/tool.key"), "{}\n").unwrap();
    fs::write(home.path().join("notes.txt"), "ordinary\n").unwrap();

    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "read",
            "{\"file_path\":\".codex/auth.json\"}",
        )]),
        tool_call_response(vec![json_call(
            "c2",
            "read",
            "{\"file_path\":\".config/keys/tool.key\"}",
        )]),
        tool_call_response(vec![json_call(
            "c3",
            "read",
            "{\"file_path\":\"notes.txt\"}",
        )]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.home = Some(home.path().to_path_buf());
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            home.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    let results = tool_results(&requests.last().unwrap().history);
    assert_eq!(results.len(), 3, "{results:?}");
    for refused in [results[0], results[1]] {
        assert!(
            refused.contains("read refuses credential files"),
            "{refused}"
        );
        assert!(
            refused.contains("credentials never enter the model's context"),
            "{refused}"
        );
    }
    assert!(results[2].contains("ordinary"), "{}", results[2]);
}
