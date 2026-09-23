//! ADR-0055: a successful command that changes the workspace counts as progress for
//! the §3c stall guard, exactly as a `WritesFiles` call does — a worker whose only
//! edits are shell heredocs is judged by what it did to the workspace, not by which
//! tool it used.
//!
//! The frozen `stall_guard.rs` scenario shape is mirrored: a headless parent starts
//! one always-replacing child (the `[context]` table below crosses its threshold at
//! every request) with `--max-idle-summaries 2`. The child has NO `write` tool — the
//! shell is its only way to change anything — so the two cases differ in exactly one
//! thing: what the command did.
//!
//! The workspace is a REAL temporary git repository, the run has its session journal
//! INSIDE it (the shape most of the frozen host tests use), and no test sleeps.

mod common;

use common::{Harness, provider_hook, run_args};
use p1_contracts::{Item, ProviderRequest};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A context table whose threshold a short prompt already crosses, so `prepare`
/// returns a replacement before every model request (the frozen `stall_guard.rs`
/// value).
const IDLE_CONTEXT: &str = "[context]\nwindow_tokens = 1000000\noutput_headroom_tokens = 1\nsummarize_at_tokens = 100\nkeep_recent_tokens = 1\nuser_verbatim_tokens = 1\n";

/// The child's task: long enough (400 chars ≈ 115 estimated tokens) to cross
/// `summarize_at_tokens` on the child's very first request.
fn task() -> String {
    "x".repeat(400)
}

/// A real git workspace: the fingerprint path that respects `.gitignore` (ADR-0055
/// item 1). The identity a commit needs is passed explicitly, so no machine-wide git
/// configuration can change the result.
fn git_workspace() -> tempfile::TempDir {
    let workspace = tempdir().unwrap();
    std::fs::write(workspace.path().join("README.md"), "one\n").unwrap();
    git(&workspace, &["init", "-q", "."]);
    git(&workspace, &["add", "README.md"]);
    git(
        &workspace,
        &[
            "-c",
            "user.name=p1",
            "-c",
            "user.email=p1@example.invalid",
            "-c",
            "commit.gpgsign=false",
            "commit",
            "-q",
            "-m",
            "one",
        ],
    );
    workspace
}

fn git(workspace: &tempfile::TempDir, args: &[&str]) {
    let status = std::process::Command::new("git")
        .args(args)
        .current_dir(workspace.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .status()
        .expect("git runs");
    assert!(status.success(), "git {args:?} failed");
}

/// One environment: `name` on `provider` with the given tools. With `idle_context`
/// it carries the always-replacing `[context]` table, so every request of that agent
/// is preceded by a summary — the child's always is, the parent's never is.
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

/// The summarizer's reply. The same provider serves the model and the summarizer, so
/// each replacement consumes one scripted step and each model request another.
fn summary() -> Step {
    text_response("SUMMARY")
}

/// A worker editing the ONLY way it can — a shell heredoc. Nothing declares this
/// write: before ADR-0055 the guard saw an idle worker.
fn heredoc(id: &str, file: &str, content: &str) -> Step {
    let command = format!("cat > {file} <<'EOF'\n{content}\nEOF");
    tool_call_response(vec![json_call(
        id,
        "shell",
        &serde_json::json!({ "command": command }).to_string(),
    )])
}

/// A command that reads and changes nothing: not progress, whatever its exit code.
fn ls(id: &str) -> Step {
    tool_call_response(vec![json_call(id, "shell", r#"{"command":"ls"}"#)])
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

/// The parent has no command tool, so `["none"]` is its honest completion.
fn finish_done(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "finish",
        r#"{"status":"done","summary":"read the worker's result","verification":["none"]}"#,
    )])
}

fn finish_blocked(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "finish",
        r#"{"status":"blocked","summary":"cannot","needs":"nothing"}"#,
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

fn stalled_message(n: usize) -> String {
    format!(
        "stalled: {n} context summaries without a change to the workspace — the task does not fit \
         the configured context (see [context] in the environment), or it is too large for one job"
    )
}

/// Run one headless parent (`fake-a`) that starts, waits for and reads the result of
/// one always-replacing child (`fake-b`), then finishes. The child's session journal
/// and the parent's both live INSIDE the workspace.
async fn run_parent_and_child(
    environments: &std::path::Path,
    workspace: &tempfile::TempDir,
    parent_script: Vec<Step>,
    child_script: Vec<Step>,
    child_tools: &[&str],
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
    let session = workspace.path().join("session.jsonl");
    let mut harness = Harness::new(vec![environments.to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));
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
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    (code, harness, parent_handle, child_handle)
}

/// ADR-0055: a child whose only edits are shell heredocs is NOT stalled. Three
/// replacements in a row would reach the bound of 2; each heredoc resets the count,
/// because the workspace really changed.
#[tokio::test]
async fn a_worker_editing_through_shell_heredocs_is_not_stalled() {
    let workspace = git_workspace();
    let environments = tempdir().unwrap();
    let (code, harness, parent, child) = run_parent_and_child(
        environments.path(),
        &workspace,
        vec![
            worker_start("c1", "child", &["read", "shell"]),
            worker_result("c2", "w1"),
            finish_done("f1"),
            text_response("parent done"),
        ],
        vec![
            summary(),
            heredoc("s1", "out.txt", "hi"),
            summary(),
            heredoc("s2", "more.txt", "there"),
            summary(),
            text_response("child done"),
        ],
        &["read", "shell"],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        !harness.stderr.text().contains("stalled: "),
        "the child must not stall: {}",
        harness.stderr.text()
    );
    assert_eq!(
        child.requests().len(),
        6,
        "summary, the heredoc, summary, the heredoc, summary, the final text"
    );
    assert_eq!(child.remaining_steps(), 0);
    assert!(
        parent_saw(&parent.requests(), "Worker w1: finished\n\nchild done"),
        "the parent reads a FINISHED worker"
    );
    assert!(
        !parent_saw(&parent.requests(), "Worker w1: failed"),
        "a heredoc-writing worker is not a stalled one"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "hi\n",
        "the heredoc really wrote the workspace the guard measured"
    );
}

/// ADR-0055's other half: a child looping on `ls` is STILL stalled. Counting every
/// successful command as progress would let it run forever; only a real change
/// counts. The run's own session journal grows inside the workspace as it works and
/// must not be mistaken for the model's work either.
#[tokio::test]
async fn a_worker_running_only_ls_is_still_stalled() {
    let workspace = git_workspace();
    let environments = tempdir().unwrap();
    let (code, harness, parent, child) = run_parent_and_child(
        environments.path(),
        &workspace,
        vec![
            worker_start("c1", "child", &["read", "shell"]),
            worker_result("c2", "w1"),
            finish_blocked("f1"),
            text_response("parent done"),
        ],
        // summary #1, `ls`, summary #2 — the bound of 2.
        vec![summary(), ls("s1"), summary()],
        &["read", "shell"],
    )
    .await;

    assert_eq!(
        code,
        p1_host::run::EXIT_BLOCKED,
        "the parent runs on to its own finish: stderr {}",
        harness.stderr.text()
    );
    assert_eq!(
        child.requests().len(),
        3,
        "summary, the model's `ls`, summary — and no model request after the Nth"
    );
    assert_eq!(child.remaining_steps(), 0);
    assert!(
        parent_saw(
            &parent.requests(),
            &format!("Worker w1: failed\n\n{}", stalled_message(2))
        ),
        "the parent reads the child's stall in the parent's own words"
    );
    assert!(
        !workspace.path().join("out.txt").exists(),
        "`ls` changed nothing, which is exactly why the child stalled"
    );
}
