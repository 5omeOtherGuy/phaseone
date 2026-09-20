//! Must-pass tests for §3c of `docs/design/completion.md`: in a HEADLESS run, a
//! session that keeps replacing its context without ever changing the workspace is
//! forgetting, not working. The host counts CONSECUTIVE `ContextReplaced` events
//! since the last progress (a workspace mutation or a `finish` call) and, at
//! `--max-idle-summaries N` (default 6), cancels the turn and exits 4.
//!
//! End to end through the real host, with a scripted provider and the REAL
//! summarizer wired by `[context]` (the same path `context_wiring.rs` exercises).
//! The always-replacing policy is made deterministic by a tiny `summarize_at_tokens`
//! and a prompt large enough to cross it at every request. No test sleeps.

mod common;

use common::{Harness, provider_hook, run_args};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A context table whose threshold a short prompt already crosses, so `prepare`
/// returns a replacement before every model request.
const IDLE_CONTEXT: &str = "[context]\nwindow_tokens = 1000000\noutput_headroom_tokens = 1\nsummarize_at_tokens = 100\nkeep_recent_tokens = 1\nuser_verbatim_tokens = 1\n";

/// A prompt long enough (400 chars ≈ 115 estimated tokens) to cross
/// `summarize_at_tokens` on the very first request.
fn big_prompt() -> String {
    "x".repeat(400)
}

fn write_context_environment(root: &std::path::Path, tools: &[&str]) {
    let dir = root.join("ctx");
    std::fs::create_dir_all(&dir).unwrap();
    let mut toml =
        format!("family = \"ctx\"\nprovider = \"fake\"\nmodel = \"fake-model\"\n\n{IDLE_CONTEXT}");
    for tool in tools {
        toml.push_str(&format!("[[tools]]\nmodule = \"{tool}\"\n"));
    }
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), "test").unwrap();
}

/// The summarizer's reply. The same provider serves the model and the summarizer,
/// so each replacement consumes one scripted step and each model request another.
fn summary() -> Step {
    text_response("SUMMARY")
}

/// A read-only tool call: finished activity, but NOT progress for §3c.
fn read_call(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "read",
        r#"{"file_path":"missing.txt"}"#,
    )])
}

/// A mutation: the `write` tool finishing successfully resets the counter.
fn write_call(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "write",
        r#"{"file_path":"out.txt","content":"hi"}"#,
    )])
}

/// A rejected `finish(done)`: it stores nothing but it is a finished `finish` call,
/// so §3c counts it as progress.
fn rejected_finish(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "finish",
        r#"{"status":"done","summary":"premature"}"#,
    )])
}

/// `finish(blocked)` ends the run at exit 3 without needing a verification command.
fn finish_blocked(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "finish",
        r#"{"status":"blocked","summary":"cannot","needs":"a fake credential"}"#,
    )])
}

/// Build the harness, register the scripted provider, and run one headless
/// `--env ctx` turn. Extra leading arguments come from `extra`.
async fn run_ctx(
    environments: &std::path::Path,
    workspace: &std::path::Path,
    script: Vec<Step>,
    extra: &[&str],
) -> (i32, Harness, ScriptedProvider) {
    let provider = ScriptedProvider::new(script);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let prompt = big_prompt();
    let mut args: Vec<String> = extra.iter().map(|arg| arg.to_string()).collect();
    args.push("--env".to_string());
    args.push("ctx".to_string());
    args.push("--workspace".to_string());
    args.push(workspace.to_str().unwrap().to_string());
    args.push(prompt);
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    let code = run_args(&mut harness, &borrowed).await;
    (code, harness, handle)
}

fn stalled_message(n: usize) -> String {
    format!(
        "stalled: {n} context summaries without a change to the workspace — the task does not fit \
         the configured context (see [context] in the environment), or it is too large for one job"
    )
}

/// §3c must-pass: N replacements with no mutation → exit 4 and the message, and no
/// provider request after the Nth replacement. The Nth replacement is produced by
/// its summarizer request; the model request that would follow it is never made.
#[tokio::test]
async fn n_replacements_without_progress_stall_the_run() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), &["read", "finish"]);

    let provider = ScriptedProvider::new(vec![summary(), read_call("r1"), summary()]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let prompt = big_prompt();
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--max-idle-summaries",
            "2",
            "--env",
            "ctx",
            "--workspace",
            workspace.path().to_str().unwrap(),
            &prompt,
        ],
    )
    .await;

    assert_eq!(
        code,
        p1_host::run::EXIT_STALLED,
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        harness.stderr.text().contains(&stalled_message(2)),
        "stderr: {}",
        harness.stderr.text()
    );
    assert_eq!(
        handle.requests().len(),
        3,
        "summarizer #1, the model request, summarizer #2 — and no model request after the Nth"
    );
    assert_eq!(handle.remaining_steps(), 0);
}

/// §3c must-pass: a mutation between replacements resets the count. With N = 2 the
/// run makes one replacement, then a `write`, then one more: the pair around the
/// reset never reaches two in a row, so the run ends `blocked` (exit 3) instead of
/// stalling (exit 4).
///
/// The spec writes this case as "2N-1 replacements with one mutation in the
/// middle"; that is unsatisfiable for one reset (the runs on either side sum to
/// 2N-1, so one of them is at least N). This uses the largest no-stall sequence,
/// 2N-2, split evenly with the mutation in the middle.
#[tokio::test]
async fn a_mutation_between_replacements_resets_the_count() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), &["read", "write", "shell", "finish"]);

    let (code, harness, handle) = run_ctx(
        environments.path(),
        workspace.path(),
        vec![
            summary(),
            write_call("w1"),
            summary(),
            finish_blocked("f1"),
            // A finish call does not end the turn by itself: the model ends it with
            // a final response, which is one more prepare.
            summary(),
            text_response("done"),
        ],
        &["--yes", "--max-idle-summaries", "2"],
    )
    .await;

    // Without the mutation reset, replacement #2 would be the second in a row and
    // the run would stall; it must not.
    assert_eq!(
        code,
        p1_host::run::EXIT_BLOCKED,
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(!harness.stderr.text().contains("stalled:"));
    assert_eq!(handle.requests().len(), 6);
}

/// §3c must-pass: a `finish` call of any status resets the count. A REJECTED
/// `finish(done)` is enough: the tool's error stays in the turn, and the two
/// replacements around it never form a run of N.
#[tokio::test]
async fn a_finish_call_between_replacements_resets_the_count() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), &["read", "shell", "finish"]);

    let (code, harness, handle) = run_ctx(
        environments.path(),
        workspace.path(),
        vec![
            summary(),
            rejected_finish("f1"),
            summary(),
            finish_blocked("f2"),
            summary(),
            text_response("done"),
        ],
        &["--yes", "--max-idle-summaries", "2"],
    )
    .await;

    assert_eq!(
        code,
        p1_host::run::EXIT_BLOCKED,
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(!harness.stderr.text().contains("stalled:"));
    assert_eq!(handle.requests().len(), 6);
}

/// §3c must-pass: `--max-idle-summaries 0` never stalls. Three replacements in a
/// row are more than the 2 the flag would otherwise allow.
#[tokio::test]
async fn max_idle_summaries_zero_never_stalls() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), &["read", "shell", "finish"]);

    let (code, harness, handle) = run_ctx(
        environments.path(),
        workspace.path(),
        vec![
            summary(),
            read_call("r1"),
            summary(),
            finish_blocked("f1"),
            summary(),
            text_response("done"),
        ],
        &["--yes", "--max-idle-summaries", "0"],
    )
    .await;

    assert_eq!(
        code,
        p1_host::run::EXIT_BLOCKED,
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(!harness.stderr.text().contains("stalled:"));
    assert_eq!(handle.requests().len(), 6);
}

/// §3c must-pass: an interactive run is never stalled; the user sees the summaries
/// and decides. `--max-idle-summaries 1` would stall the same environment headless.
#[tokio::test]
async fn an_interactive_run_is_not_stalled() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), &["read", "finish"]);

    let provider = ScriptedProvider::new(vec![
        summary(),
        text_response("first"),
        summary(),
        text_response("second"),
    ]);
    let mut harness = Harness::new(
        vec![environments.path().to_path_buf()],
        &["one", "two", "/exit"],
    );
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--max-idle-summaries",
            "1",
            "--env",
            "ctx",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(!harness.stderr.text().contains("stalled:"));
}

/// §3c must-pass: the count survives nothing — a resumed run starts at 0. The first
/// run journals two replacements; the resumed run makes two more with N = 3, which
/// would reach the bound only if the count had carried over.
#[tokio::test]
async fn a_resumed_run_starts_its_summary_count_at_zero() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_context_environment(environments.path(), &["read", "shell", "finish"]);
    let session = workspace.path().join("session.jsonl");

    let (code, first, _handle) = run_ctx(
        environments.path(),
        workspace.path(),
        vec![
            summary(),
            read_call("r1"),
            summary(),
            read_call("r2"),
            summary(),
            finish_blocked("f1"),
            summary(),
            text_response("done"),
        ],
        &[
            "--yes",
            "--max-idle-summaries",
            "0",
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(
        code,
        p1_host::run::EXIT_BLOCKED,
        "first run: {}",
        first.stderr.text()
    );

    let (code, second, _handle) = run_ctx(
        environments.path(),
        workspace.path(),
        vec![
            summary(),
            read_call("r3"),
            summary(),
            finish_blocked("f2"),
            summary(),
            text_response("done"),
        ],
        &[
            "--yes",
            "--max-idle-summaries",
            "3",
            "--session",
            session.to_str().unwrap(),
            "--resume",
        ],
    )
    .await;

    assert_eq!(
        code,
        p1_host::run::EXIT_BLOCKED,
        "resumed run: {}",
        second.stderr.text()
    );
    assert!(!second.stderr.text().contains("stalled:"));
}
