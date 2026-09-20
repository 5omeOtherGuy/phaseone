//! Must-pass a–i for the turn-completion policy (spec `docs/design/completion.md`
//! §3), end to end through the real host with scripted providers and the REAL
//! `shell`/`write` tools in a tempdir workspace. Nothing here matches assistant
//! prose: every assertion is on a tool result, an exit code, a record or stderr.

mod common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, RecordBody};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

const DONE_TRUE: &str = r#"{"status":"done","summary":"wrote it","verification":["true"]}"#;

fn finish_environment(root: &Path, tools: &[&str]) {
    write_environment(root, "finish-env", "fake", "fake-model", tools, "test");
}

fn write_env(root: &Path) {
    write_environment(root, "plain", "fake", "fake-model", &["read"], "test");
}

fn shell_call(id: &str, command: &str) -> p1_contracts::ToolCall {
    debug_assert!(!command.contains('"') && !command.contains('\\'));
    json_call(id, "shell", &format!(r#"{{"command":"{command}"}}"#))
}

fn finish_results(provider: &ScriptedProvider) -> Vec<String> {
    let requests = provider.requests();
    let history = &requests.last().expect("at least one request").history;
    history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == "finish" => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

fn user_inputs(records: &[p1_contracts::JournalRecord]) -> Vec<String> {
    records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::UserInput { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[test]
fn the_continuation_message_is_exactly_the_spec_text() {
    assert_eq!(
        p1_host::run::CONTINUATION_MESSAGE,
        "You ended your turn without calling finish. You are running unattended: nobody will \
         answer a question or confirm a plan, and this task authorizes you to continue on your \
         own. Continue the work now. When it is complete and verified, call finish with status \
         \"done\"; if something outside your control stops you, call finish with status \"blocked\"."
    );
}

// ------------------------------------------------------------------ (a)

#[tokio::test]
async fn a_one_continuation_then_finish_done_exits_zero() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "write", "finish"]);
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![
        text_response("I stopped without finishing"),
        tool_call_response(vec![json_call(
            "w1",
            "write",
            r#"{"file_path":"out.txt","content":"hi"}"#,
        )]),
        tool_call_response(vec![shell_call("s1", "true")]),
        tool_call_response(vec![json_call("f1", "finish", DONE_TRUE)]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "do it",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let loaded = p1_journal::load(&session).unwrap();
    let inputs = user_inputs(&loaded.records);
    assert_eq!(
        inputs,
        vec![
            "do it".to_string(),
            p1_host::run::CONTINUATION_MESSAGE.to_string()
        ],
        "the journal must show the prompt and exactly one continuation"
    );
}

// ------------------------------------------------------------------ (b)

#[tokio::test]
async fn b_tool_calls_between_stops_allow_three_continuations_then_stall() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "finish"]);
    let stop = || text_response("still not finished");
    let progress = || tool_call_response(vec![shell_call("s", "echo ok")]);
    let provider = ScriptedProvider::new(vec![
        stop(),
        progress(),
        stop(),
        progress(),
        stop(),
        progress(),
        stop(),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_STALLED);
    assert_eq!(
        provider.requests().len(),
        7,
        "four stops, three continuations"
    );
    assert!(
        harness
            .stderr
            .text()
            .contains("stalled: the agent stopped 4 times without finishing"),
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn b_no_progress_allows_only_one_continuation_then_stall() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "finish"]);
    let provider = ScriptedProvider::new(vec![
        text_response("one"),
        text_response("two, still no work done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_STALLED);
    assert_eq!(provider.requests().len(), 2);
    assert!(
        harness
            .stderr
            .text()
            .contains("stalled: the agent stopped 2 times without finishing"),
        "stderr: {}",
        harness.stderr.text()
    );
}

// ------------------------------------------------------------------ (c)

#[tokio::test]
async fn c_blocked_exits_three_and_never_continues() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["finish"]);
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"blocked","summary":"cannot continue","needs":"an API token","tried":["checked the environment"]}"#,
        )]),
        text_response("blocked"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_BLOCKED);
    assert!(
        harness.stderr.text().contains("blocked: an API token"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        harness
            .stderr
            .text()
            .contains("tried: checked the environment"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(!harness.stdout.text().contains("You ended your turn"));
    assert!(!harness.stderr.text().contains("You ended your turn"));
}

// ------------------------------------------------------------------ (d)

#[tokio::test]
async fn d_the_three_exact_errors_then_a_valid_finish() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "write", "finish"]);
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"s","verification":["never-run"]}"#,
        )]),
        tool_call_response(vec![shell_call("s1", "false")]),
        tool_call_response(vec![json_call(
            "f2",
            "finish",
            r#"{"status":"done","summary":"s","verification":["false"]}"#,
        )]),
        tool_call_response(vec![shell_call("s2", "true")]),
        tool_call_response(vec![json_call(
            "w1",
            "write",
            r#"{"file_path":"out.txt","content":"hi"}"#,
        )]),
        tool_call_response(vec![json_call("f3", "finish", DONE_TRUE)]),
        tool_call_response(vec![shell_call("s3", "true")]),
        tool_call_response(vec![json_call("f4", "finish", DONE_TRUE)]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        finish_results(&provider),
        vec![
            "No successful run of `never-run` is recorded in this session. Run it, read the result, then finish.".to_string(),
            "No successful run of `false` is recorded in this session. Run it, read the result, then finish.".to_string(),
            "You changed files after running `true`. Run it again, then finish.".to_string(),
            "Finished.".to_string(),
        ]
    );
}

// ------------------------------------------------------------------ (e)

#[tokio::test]
async fn e_a_later_failing_rerun_invalidates_the_earlier_success() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "write", "finish"]);
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "w1",
            "write",
            r#"{"file_path":"marker","content":"x"}"#,
        )]),
        tool_call_response(vec![shell_call("s1", "cat marker")]),
        tool_call_response(vec![shell_call("s2", "rm marker")]),
        tool_call_response(vec![shell_call("s3", "cat marker")]),
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"s","verification":["cat marker"]}"#,
        )]),
        tool_call_response(vec![json_call(
            "f2",
            "finish",
            r#"{"status":"blocked","summary":"cannot verify","needs":"a readable marker"}"#,
        )]),
        text_response("blocked"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_BLOCKED);
    assert_eq!(
        finish_results(&provider),
        vec![
            "No successful run of `cat marker` is recorded in this session. Run it, read the result, then finish.".to_string(),
            "Recorded as blocked.".to_string(),
        ]
    );
}

// ------------------------------------------------------------------ (f)

#[tokio::test]
async fn f_none_is_accepted_without_files_and_rejected_after_a_write() {
    // Accepted: a session that changed nothing.
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["finish"]);
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"answered a question","verification":["none"]}"#,
        )]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));
    let code = run_args(
        &mut harness,
        &[
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(finish_results(&provider), vec!["Finished.".to_string()]);

    // Rejected: the same call after a file change.
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "write", "finish"]);
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "w1",
            "write",
            r#"{"file_path":"out.txt","content":"hi"}"#,
        )]),
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"s","verification":["none"]}"#,
        )]),
        tool_call_response(vec![shell_call("s1", "true")]),
        tool_call_response(vec![json_call("f2", "finish", DONE_TRUE)]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        finish_results(&provider),
        vec![
            "This session changed files; verify the result with a command before finishing."
                .to_string(),
            "Finished.".to_string(),
        ]
    );
}

// ------------------------------------------------------------------ (g)

#[tokio::test]
async fn g_interactive_does_not_continue_a_text_only_turn() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["finish"]);
    let provider = ScriptedProvider::new(vec![
        text_response("first"),
        text_response("must never be requested"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &["go"]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        provider.requests().len(),
        1,
        "the model must not be asked twice"
    );
    assert!(!harness.stdout.text().contains("You ended your turn"));
}

// ------------------------------------------------------------------ (h)

#[tokio::test]
async fn h_zero_max_continuations_stalls_at_once() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["shell", "finish"]);
    let provider = ScriptedProvider::new(vec![text_response("stopped")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--max-continuations",
            "0",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_STALLED);
    assert_eq!(provider.requests().len(), 1);
    assert!(
        harness
            .stderr
            .text()
            .contains("stalled: the agent stopped 1 times without finishing"),
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn h_an_environment_without_finish_is_unchanged() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_env(environments.path());
    let provider = ScriptedProvider::new(vec![text_response("plain answer")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0);
    assert_eq!(provider.requests().len(), 1);
    assert!(!harness.stderr.text().contains("stalled"));
}

// ------------------------------------------------------------------ (i)

#[tokio::test]
async fn i_cancellation_during_a_continuation_exits_130() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path(), &["finish"]);
    let provider = ScriptedProvider::new(vec![
        text_response("stopped"),
        Step::EventsThenHang(vec![p1_contracts::StreamEvent::TextDelta {
            block: 0,
            text: "working on it".to_string(),
        }]),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let interrupt = harness.interrupt.clone();
    let watched = provider.clone();
    tokio::spawn(async move {
        while watched.requests().len() < 2 {
            tokio::task::yield_now().await;
        }
        interrupt.fire();
    });

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, p1_host::run::EXIT_CANCELLED);
}
