//! The per-run scratch directory, end to end (ADR-0122, #457).
//!
//! A `write` into `{{scratch}}` succeeds, lands in the run's scratch directory outside
//! the workspace, and is not a file change: `finish` evidence recorded before it stays
//! fresh. A `write` into the workspace still is a file change, so the same evidence goes
//! stale. A run without a session removes its `$TMPDIR/p1-scratch-<hex>/` at the end; a
//! session keeps `FILE.scratch/`, and a `--resume` run gets the same path. The shell sees
//! `P1_SCRATCH`, and with `--sandbox workspace` a write there succeeds.

mod common;

use std::path::Path;
use std::process::Command;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, RecordBody, ToolCall};
use p1_testkit::{ScriptedProvider, Step, json_call, tool_call_response};
use tempfile::{TempDir, tempdir};

const TRAILER_NONE: &str =
    "No run counts right now: run your checks (without a pipe) after your last file change.";

fn with_no_runs(message: &str) -> String {
    format!("{message}\n\n{TRAILER_NONE}")
}

fn environment(root: &Path) {
    write_environment(
        root,
        "scratch-env",
        "fake",
        "fake-model",
        &["shell", "write", "finish"],
        "test",
    );
}

/// A real git workspace: the fingerprint path that respects `.gitignore`, and a
/// `git status` a test can read after the run.
fn git_workspace() -> TempDir {
    let workspace = tempdir().unwrap();
    std::fs::write(workspace.path().join("README.md"), "one\n").unwrap();
    for args in [
        vec!["init", "-q", "."],
        vec!["add", "README.md"],
        vec![
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
    ] {
        let status = Command::new("git")
            .args(&args)
            .current_dir(workspace.path())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .status()
            .expect("git runs");
        assert!(status.success(), "git {args:?} failed");
    }
    workspace
}

fn porcelain(workspace: &Path) -> String {
    let output = Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(workspace)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .output()
        .expect("git runs");
    String::from_utf8(output.stdout).unwrap()
}

fn shell_call(id: &str, command: &str) -> ToolCall {
    json_call(
        id,
        "shell",
        &serde_json::json!({ "command": command }).to_string(),
    )
}

fn write_call(id: &str, path: &str, content: &str) -> ToolCall {
    json_call(
        id,
        "write",
        &serde_json::json!({ "file_path": path, "content": content }).to_string(),
    )
}

/// A `done` whose evidence names `command` (the recorded-commands policy).
fn finish_commands(id: &str, command: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "finish",
        &serde_json::json!({
            "status": "done",
            "summary": "did it",
            "verification": [command],
        })
        .to_string(),
    )])
}

/// A `done` that names no run: `verification: ["none"]`. Under the recorded-commands
/// policy this is accepted with the no-file-changed reason ONLY when nothing in the
/// workspace changed, so it is the discriminator for a scratch-only write.
fn finish_none(id: &str) -> Step {
    tool_call_response(vec![json_call(
        id,
        "finish",
        &serde_json::json!({
            "status": "done",
            "summary": "did it",
            "verification": ["none"],
        })
        .to_string(),
    )])
}

/// Every `finish` result the run committed, in order, from the session journal.
fn finish_results(session: &Path) -> Vec<String> {
    p1_journal::load(session)
        .expect("the session journal loads")
        .records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::ToolFinished { result, .. } if result.name == "finish" => {
                Some(result.content.clone())
            }
            _ => None,
        })
        .collect()
}

/// The stdout of the last `shell` call, with the tool's `[exit code: N]` footer removed.
fn shell_stdout(history: &[Item]) -> String {
    let mut content = history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == "shell" => Some(result.content.clone()),
            _ => None,
        })
        .collect::<Vec<_>>();
    let content = content.pop().expect("a shell call finished");
    content
        .split("[exit code:")
        .next()
        .unwrap_or(&content)
        .trim()
        .to_string()
}

/// The path the shell printed as `$P1_SCRATCH` (the last line of its stdout).
fn printed_scratch(history: &[Item]) -> String {
    shell_stdout(history)
        .lines()
        .last()
        .unwrap_or("")
        .trim()
        .to_string()
}

fn setup_harness(environments: &Path, script: Vec<Step>) -> (Harness, ScriptedProvider) {
    let provider = ScriptedProvider::new(script);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    (harness, handle)
}

async fn run(harness: &mut Harness, workspace: &Path, session: &Path, extra: &[&str]) -> i32 {
    let session = session.to_str().unwrap().to_string();
    let workspace = workspace.to_str().unwrap().to_string();
    let mut args = vec![
        "--yes".to_string(),
        "--env".to_string(),
        "scratch-env".to_string(),
        "--workspace".to_string(),
        workspace,
        "--session".to_string(),
        session,
    ];
    args.extend(extra.iter().map(|arg| arg.to_string()));
    args.push("go".to_string());
    let borrowed: Vec<&str> = args.iter().map(String::as_str).collect();
    run_args(harness, &borrowed).await
}

/// Requirement 2, main case: the model runs `true`, writes its PR body to
/// `{{scratch}}/body.md`, and finishes naming `true`. The write lands in the scratch
/// directory, never in the workspace, and does NOT make the recorded `true` stale.
#[tokio::test]
async fn a_scratch_write_lands_outside_the_workspace_and_keeps_the_run_fresh() {
    let workspace = git_workspace();
    let session_dir = tempdir().unwrap();
    let environments = tempdir().unwrap();
    environment(environments.path());
    let session = session_dir.path().join("session.jsonl");
    let scratch = p1_host::session::scratch_path(Some(&session));
    assert_eq!(
        scratch,
        session_dir.path().join("session.jsonl.scratch"),
        "a session's scratch is `FILE.scratch/` beside it"
    );

    let body = scratch.join("body.md");
    let (mut harness, _provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![shell_call("s1", "true")]),
            tool_call_response(vec![write_call("w1", body.to_str().unwrap(), "PR body")]),
            finish_commands("f1", "true"),
        ],
    );
    let code = run(&mut harness, workspace.path(), &session, &[]).await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        std::fs::read_to_string(&body).unwrap(),
        "PR body",
        "the write landed at {{scratch}}/body.md"
    );
    assert!(scratch.is_dir(), "a session's scratch survives the run");
    assert_eq!(
        porcelain(workspace.path()),
        "",
        "nothing was written into the workspace"
    );
    assert_eq!(
        finish_results(&session),
        vec!["Finished."],
        "the recorded `true` is not stale: the scratch write is not a file change"
    );
}

/// Requirement 2, variant b: the same run but the write aimed at `notes.md` in the
/// workspace. It IS a file change, so the `finish` naming the earlier `true` is refused
/// as stale (today's rule, unchanged).
#[tokio::test]
async fn a_workspace_write_makes_the_recorded_run_stale() {
    let workspace = git_workspace();
    let session_dir = tempdir().unwrap();
    let environments = tempdir().unwrap();
    environment(environments.path());
    let session = session_dir.path().join("session.jsonl");

    let (mut harness, _provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![shell_call("s1", "true")]),
            tool_call_response(vec![write_call("w1", "notes.md", "notes")]),
            finish_commands("f1", "true"),
            // After the refusal the model re-runs and finishes again, so the run ends.
            tool_call_response(vec![shell_call("s3", "true")]),
            finish_commands("f2", "true"),
        ],
    );
    let code = run(&mut harness, workspace.path(), &session, &[]).await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("notes.md")).unwrap(),
        "notes"
    );
    assert_eq!(
        finish_results(&session),
        vec![
            with_no_runs("You changed files after running `true`. Run it again, then finish."),
            "Finished.".to_string(),
        ],
        "the workspace write is a file change, so the recorded `true` went stale"
    );
}

/// Requirement 2, variant c: only a scratch write and a `finish` that names no run. The
/// finish is accepted with the no-file-changed reason — which it would not be if the
/// scratch write counted as a file change.
#[tokio::test]
async fn only_a_scratch_write_still_allows_a_finish_without_a_run() {
    let workspace = git_workspace();
    let session_dir = tempdir().unwrap();
    let environments = tempdir().unwrap();
    environment(environments.path());
    let session = session_dir.path().join("session.jsonl");
    let scratch = p1_host::session::scratch_path(Some(&session));
    let body = scratch.join("body.md");

    let (mut harness, _provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![write_call("w1", body.to_str().unwrap(), "PR body")]),
            finish_none("f1"),
        ],
    );
    let code = run(&mut harness, workspace.path(), &session, &[]).await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(std::fs::read_to_string(&body).unwrap(), "PR body");
    // A refusal here would be the "this session changed files" trailer, so `Finished.`
    // is the no-file-changed acceptance.
    assert_eq!(
        finish_results(&session),
        vec!["Finished."],
        "the scratch write is no file change, so a runless finish is accepted"
    );
}

/// Requirement 4, no `--session`: the scratch directory the shell saw is
/// `$TMPDIR/p1-scratch-<hex>/`, and it and its contents are gone after the run.
#[tokio::test]
async fn a_run_without_a_session_removes_its_scratch_directory() {
    let workspace = git_workspace();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "scratch-env",
        "fake",
        "fake-model",
        &["shell", "finish"],
        "test",
    );
    let (mut harness, provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![shell_call(
                "s1",
                r#"echo hi > "$P1_SCRATCH/note.txt" && printf '%s' "$P1_SCRATCH""#,
            )]),
            finish_none("f1"),
        ],
    );
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "scratch-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    // The `&&` guard means the path is printed only if the directory existed and took the
    // write, so the printed path proves the directory was made; after the run it is gone.
    let requests = provider.requests();
    let path = printed_scratch(&requests.last().unwrap().history);
    assert!(
        path.starts_with(std::env::temp_dir().to_str().unwrap()),
        "the scratch directory is under $TMPDIR: {path}"
    );
    assert!(
        path.contains("p1-scratch-"),
        "named as the ADR says: {path}"
    );
    assert!(
        !Path::new(&path).exists(),
        "the run removed its scratch directory at exit"
    );
}

/// Requirement 4, with `--session` and a `--resume`: the scratch directory survives and
/// a resumed run gets the same path, with its notes still there.
#[tokio::test]
async fn a_session_scratch_survives_and_resumes_at_the_same_path() {
    let workspace = git_workspace();
    let session_dir = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "scratch-env",
        "fake",
        "fake-model",
        &["shell", "finish"],
        "test",
    );
    let session = session_dir.path().join("session.jsonl");
    let scratch = p1_host::session::scratch_path(Some(&session));

    let (mut harness, provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![shell_call(
                "s1",
                r#"echo hi > "$P1_SCRATCH/note.txt" && printf '%s' "$P1_SCRATCH""#,
            )]),
            finish_none("f1"),
        ],
    );
    let code = run(&mut harness, workspace.path(), &session, &[]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let first = provider.requests();
    assert_eq!(
        printed_scratch(&first.last().unwrap().history),
        scratch.to_str().unwrap(),
        "the shell saw `FILE.scratch/` beside the session"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("note.txt")).unwrap(),
        "hi\n",
        "the session's scratch directory and its notes survive the run"
    );

    // Resume the same session: the second run gets the same scratch path and finds the note.
    let (mut resumed, resumed_provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![shell_call("s2", r#"printf '%s' "$P1_SCRATCH""#)]),
            finish_none("f2"),
        ],
    );
    let code = run(&mut resumed, workspace.path(), &session, &["--resume"]).await;
    assert_eq!(code, 0, "stderr: {}", resumed.stderr.text());
    let second = resumed_provider.requests();
    assert_eq!(
        printed_scratch(&second.last().unwrap().history),
        scratch.to_str().unwrap(),
        "a resumed run gets the same scratch path"
    );
    assert!(
        scratch.join("note.txt").exists(),
        "the note survived the resume"
    );
}

fn bwrap_usable() -> bool {
    Command::new("bwrap")
        .args([
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "true",
        ])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|status| status.success())
        .unwrap_or(false)
}

/// Requirement 5: the shell sees `P1_SCRATCH` (`echo`/`printf` prints the path), and with
/// `--sandbox workspace` a write into it succeeds.
#[tokio::test]
async fn the_sandbox_binds_the_scratch_directory_writable() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let out = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "scratch-env",
        "fake",
        "fake-model",
        &["shell", "finish"],
        "test",
    );
    let session = out.path().join("session.jsonl");
    let scratch = p1_host::session::scratch_path(Some(&session));

    let (mut harness, provider) = setup_harness(
        environments.path(),
        vec![
            tool_call_response(vec![shell_call(
                "s1",
                r#"printf '%s' "$P1_SCRATCH" && echo x > "$P1_SCRATCH/note.txt""#,
            )]),
            finish_none("f1"),
        ],
    );
    harness.deps.home = Some(home.path().to_path_buf());
    let code = run(
        &mut harness,
        &workspace,
        &session,
        &["--sandbox", "workspace"],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = provider.requests();
    assert_eq!(
        printed_scratch(&requests.last().unwrap().history),
        scratch.to_str().unwrap(),
        "the sandboxed shell printed the scratch path"
    );
    assert_eq!(
        std::fs::read_to_string(scratch.join("note.txt"))
            .unwrap()
            .trim(),
        "x",
        "a sandboxed command may write into the scratch directory"
    );
}
