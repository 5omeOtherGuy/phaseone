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

// ------------------------------------------------ the same guard for a worker

/// §3c for a delegated worker. A worker is ALWAYS unattended — nobody reads its
/// summaries and decides — so the parent's `--max-idle-summaries` bound applies to
/// every child, whatever the parent's own mode is, and one child's stall never
/// cancels the parent or another child. The child ends `Failed` with the parent's
/// exact sentence, not `Cancelled`, so the parent model and the operator read the
/// same words.
#[cfg(feature = "delegation")]
mod delegated {
    use super::*;
    use p1_contracts::{Item, ProviderRequest};

    /// The child's task. Long enough (400 chars ≈ 115 estimated tokens) to cross
    /// `summarize_at_tokens` on the child's very first request, exactly as
    /// [`big_prompt`] does for the parent tests.
    fn task() -> String {
        "x".repeat(400)
    }

    /// One environment: `name` with the given provider key and tools. With
    /// `idle_context` it carries the always-replacing `[context]` table, so every
    /// request of that agent is preceded by a summary.
    fn write_env(
        root: &std::path::Path,
        name: &str,
        provider: &str,
        tools: &[&str],
        idle_context: bool,
    ) {
        let dir = root.join(name);
        std::fs::create_dir_all(&dir).unwrap();
        let mut toml =
            format!("family = \"{name}\"\nprovider = \"{provider}\"\nmodel = \"{name}-model\"\n");
        if idle_context {
            toml.push_str(IDLE_CONTEXT);
        }
        for tool in tools {
            toml.push_str(&format!("[[tools]]\nmodule = \"{tool}\"\n"));
        }
        std::fs::write(dir.join("environment.toml"), toml).unwrap();
        std::fs::write(dir.join("prompt.md"), "test").unwrap();
    }

    fn worker_start(id: &str, environment: &str, tools: &[&str]) -> Step {
        let tools: Vec<String> = tools.iter().map(|tool| format!("\"{tool}\"")).collect();
        tool_call_response(vec![json_call(
            id,
            "worker_start",
            &format!(
                r#"{{"environment":"{environment}","task":"{}","tools":[{}]}}"#,
                task(),
                tools.join(",")
            ),
        )])
    }

    fn worker_result(call: &str, worker: &str) -> Step {
        tool_call_response(vec![json_call(
            call,
            "worker_result",
            &format!(r#"{{"id":"{worker}","wait":true}}"#),
        )])
    }

    fn finish_done(id: &str) -> Step {
        tool_call_response(vec![json_call(
            id,
            "finish",
            r#"{"status":"done","summary":"read the worker's result","verification":["none"]}"#,
        )])
    }

    /// Everything the parent was shown, tool results and inbox messages alike.
    fn parent_saw(requests: &[ProviderRequest], needle: &str) -> bool {
        requests.iter().any(|request| {
            request.history.iter().any(|item| match item {
                Item::ToolResult(result) => result.content.contains(needle),
                Item::Inbox { text, .. } => text.contains(needle),
                _ => false,
            })
        })
    }

    /// Run one headless parent (`fake-a`) that starts, waits for and reads the
    /// result of one always-replacing child (`fake-b`), then finishes.
    async fn run_parent_and_child(
        environments: &std::path::Path,
        workspace: &std::path::Path,
        parent_script: Vec<Step>,
        child_script: Vec<Step>,
        child_tools: &[&str],
        max_idle_summaries: &str,
    ) -> (i32, Harness, ScriptedProvider, ScriptedProvider) {
        write_env(
            environments,
            "parent",
            "fake-a",
            &["worker_start", "worker_result", "finish"],
            false,
        );
        write_env(environments, "child", "fake-b", child_tools, true);
        let parent = ScriptedProvider::new(parent_script);
        let child = ScriptedProvider::new(child_script);
        let parent_handle = parent.clone();
        let child_handle = child.clone();
        let mut harness = Harness::new(vec![environments.to_path_buf()], &[]);
        harness.deps.catalog_hook =
            Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));
        let code = run_args(
            &mut harness,
            &[
                "--yes",
                "--max-idle-summaries",
                max_idle_summaries,
                "--env",
                "parent",
                "--workspace",
                workspace.to_str().unwrap(),
                "go",
            ],
        )
        .await;
        (code, harness, parent_handle, child_handle)
    }

    /// §3c must-pass for workers: N replacements with no mutation end THAT child
    /// as `Failed` carrying the parent's own sentence — "failed", not "cancelled" —
    /// and the parent runs on to its own `finish`. The Nth replacement is the last
    /// child request: the model request that would follow it is never made.
    #[tokio::test]
    async fn a_child_that_summarizes_without_progress_fails_with_the_parents_message() {
        let workspace = tempdir().unwrap();
        let environments = tempdir().unwrap();
        let (code, harness, parent, child) = run_parent_and_child(
            environments.path(),
            workspace.path(),
            vec![
                worker_start("c1", "child", &["read"]),
                worker_result("c2", "w1"),
                finish_blocked("f1"),
                text_response("parent done"),
            ],
            // summary #1, the model's read call, summary #2 — the bound.
            vec![summary(), read_call("r1"), summary()],
            &["read"],
            "2",
        )
        .await;

        assert_eq!(
            code,
            p1_host::run::EXIT_BLOCKED,
            "the parent must run on to its own finish: stderr {}",
            harness.stderr.text()
        );
        assert!(
            !harness.stderr.text().contains("stalled: "),
            "the parent did not stall: {}",
            harness.stderr.text()
        );
        assert_eq!(
            child.requests().len(),
            3,
            "summary, the model's read call, summary — and no model request after the Nth"
        );
        assert_eq!(child.remaining_steps(), 0);
        assert!(
            parent_saw(
                &parent.requests(),
                &format!("Worker w1: failed\n\n{}", stalled_message(2))
            ),
            "the parent reads the child's failure in the parent's own words"
        );
        assert!(
            parent_saw(&parent.requests(), "Worker w1 finished (failed)"),
            "the completion notification says failed"
        );
        assert!(
            !parent_saw(&parent.requests(), "Worker w1: cancelled"),
            "a stalled child is not merely cancelled"
        );
    }

    /// §3c must-pass for workers: a mutation between replacements resets that
    /// child's count, so the child is never stopped. With N = 2 the child makes one
    /// replacement, a `write`, one more replacement and a `write` again, then ends
    /// its turn normally: the parent reads a FINISHED worker.
    #[tokio::test]
    async fn a_child_that_mutates_between_replacements_is_not_stopped() {
        let workspace = tempdir().unwrap();
        let environments = tempdir().unwrap();
        let (code, harness, parent, child) = run_parent_and_child(
            environments.path(),
            workspace.path(),
            vec![
                worker_start("c1", "child", &["read", "write"]),
                worker_result("c2", "w1"),
                finish_done("f1"),
                text_response("parent done"),
            ],
            vec![
                summary(),
                write_call("w1"),
                summary(),
                write_call("w2"),
                summary(),
                text_response("child done"),
            ],
            &["read", "write"],
            "2",
        )
        .await;

        assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
        assert!(!harness.stderr.text().contains("stalled: "));
        assert_eq!(child.requests().len(), 6);
        assert_eq!(child.remaining_steps(), 0);
        assert!(
            parent_saw(&parent.requests(), "Worker w1: finished\n\nchild done"),
            "the child finished; the parent read its text"
        );
        assert!(workspace.path().join("out.txt").exists());
    }

    /// §3c must-pass for workers: `--max-idle-summaries 0` disables the guard for
    /// children too. Three replacements in a row are more than the 2 the flag would
    /// otherwise allow, and the child still finishes.
    #[tokio::test]
    async fn max_idle_summaries_zero_never_stops_a_child() {
        let workspace = tempdir().unwrap();
        let environments = tempdir().unwrap();
        let (code, harness, parent, child) = run_parent_and_child(
            environments.path(),
            workspace.path(),
            vec![
                worker_start("c1", "child", &["read"]),
                worker_result("c2", "w1"),
                finish_done("f1"),
                text_response("parent done"),
            ],
            vec![
                summary(),
                read_call("r1"),
                summary(),
                read_call("r2"),
                summary(),
                text_response("child done"),
            ],
            &["read"],
            "0",
        )
        .await;

        assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
        assert!(!harness.stderr.text().contains("stalled: "));
        assert_eq!(child.requests().len(), 6);
        assert!(
            parent_saw(&parent.requests(), "Worker w1: finished\n\nchild done"),
            "with the bound disabled the child is not stopped"
        );
    }

    /// §3c must-pass for workers: one child's stall cancels THAT child only. While
    /// the stalling child (`fake-b`) ends `Failed`, its sibling (`fake-c`) keeps
    /// replacing, mutating and finally finishing, and the parent reads both.
    #[tokio::test]
    async fn one_child_stalling_leaves_its_sibling_running() {
        let workspace = tempdir().unwrap();
        let environments = tempdir().unwrap();
        write_env(
            environments.path(),
            "parent",
            "fake-a",
            &["worker_start", "worker_result", "finish"],
            false,
        );
        write_env(environments.path(), "stalling", "fake-b", &["read"], true);
        write_env(
            environments.path(),
            "working",
            "fake-c",
            &["read", "write"],
            true,
        );

        let parent = ScriptedProvider::new(vec![
            worker_start("c1", "stalling", &["read"]),
            worker_start("c2", "working", &["read", "write"]),
            worker_result("c3", "w1"),
            worker_result("c4", "w2"),
            finish_blocked("f1"),
            text_response("parent done"),
        ]);
        let stalling = ScriptedProvider::new(vec![summary(), read_call("r1"), summary()]);
        let working = ScriptedProvider::new(vec![
            summary(),
            write_call("w1"),
            summary(),
            write_call("w2"),
            summary(),
            text_response("second done"),
        ]);
        let parent_handle = parent.clone();
        let stalling_handle = stalling.clone();
        let working_handle = working.clone();
        let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
        harness.deps.catalog_hook = Some(provider_hook(vec![
            ("fake-a", parent),
            ("fake-b", stalling),
            ("fake-c", working),
        ]));

        let code = run_args(
            &mut harness,
            &[
                "--yes",
                "--max-idle-summaries",
                "2",
                "--env",
                "parent",
                "--workspace",
                workspace.path().to_str().unwrap(),
                "go",
            ],
        )
        .await;

        assert_eq!(
            code,
            p1_host::run::EXIT_BLOCKED,
            "stderr: {}",
            harness.stderr.text()
        );
        assert_eq!(stalling_handle.requests().len(), 3);
        assert_eq!(working_handle.requests().len(), 6);
        assert!(
            parent_saw(
                &parent_handle.requests(),
                &format!("Worker w1: failed\n\n{}", stalled_message(2))
            ),
            "the stalling child fails with the parent's sentence"
        );
        assert!(
            parent_saw(
                &parent_handle.requests(),
                "Worker w2: finished\n\nsecond done"
            ),
            "the sibling was never stopped"
        );
    }
}
