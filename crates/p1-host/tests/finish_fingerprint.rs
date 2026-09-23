//! ADR-0055 item 2 for the `finish` check: a successful command that changed the
//! workspace IS a file change, so ADR-0037's rule ("the run must be newer than the
//! last file change") makes a verification run from BEFORE it stale — a `cargo test`
//! that ran before a heredoc write must be repeated.
//!
//! End to end through the real host with the REAL `shell` and `finish` tools in a
//! real temporary git workspace. Nothing declares the write: the shell heredoc is the
//! model's only edit tool here, exactly as in the run that produced ADR-0055.

mod common;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, ToolCall};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// Every verification rejection ends with what would be accepted right now.
const TRAILER_HEADING: &str =
    "Runs that count right now (successful, not piped, after the last file change):";
const TRAILER_NONE: &str =
    "No run counts right now: run your checks (without a pipe) after your last file change.";

fn with_no_runs(message: &str) -> String {
    format!("{message}\n\n{TRAILER_NONE}")
}

fn with_runs(message: &str, commands: &[&str]) -> String {
    let mut text = format!("{message}\n\n{TRAILER_HEADING}");
    for command in commands {
        text.push_str("\n- ");
        text.push_str(command);
    }
    text
}

/// The environment under test: the shell is the only tool that can change anything,
/// so a file change can only come from a command.
fn shell_environment(root: &std::path::Path) {
    write_environment(
        root,
        "shell-env",
        "fake",
        "fake-model",
        &["shell", "finish"],
        "test",
    );
}

/// A real git workspace: the fingerprint path that respects `.gitignore`, so a
/// change is seen as the repository sees it (ADR-0055 item 1). The identity a commit
/// needs is passed explicitly, so no machine-wide git configuration can change the
/// result.
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

fn shell_call(id: &str, command: &str) -> ToolCall {
    debug_assert!(!command.contains('"') && !command.contains('\\'));
    json_call(id, "shell", &format!(r#"{{"command":"{command}"}}"#))
}

/// A write that only the SHELL performs — the shape issue #53 was filed for.
fn heredoc(id: &str, file: &str, content: &str) -> Step {
    let command = format!("cat > {file} <<'EOF'\n{content}\nEOF");
    tool_call_response(vec![json_call(
        id,
        "shell",
        &serde_json::json!({ "command": command }).to_string(),
    )])
}

fn finish(id: &str, command: &str) -> Step {
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

async fn run(
    environments: &std::path::Path,
    workspace: &tempfile::TempDir,
    script: Vec<Step>,
) -> (i32, Harness, ScriptedProvider) {
    let provider = ScriptedProvider::new(script);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "shell-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    (code, harness, handle)
}

/// ADR-0055 item 2: `true` ran, then a heredoc wrote the workspace, so the recorded
/// `true` is stale and the `finish` that names it is rejected with ADR-0037's exact
/// text. Running `true` again — now the last run, after the change — is accepted.
#[tokio::test]
async fn a_verification_run_before_a_heredoc_write_must_be_repeated() {
    let workspace = git_workspace();
    let environments = tempdir().unwrap();
    shell_environment(environments.path());

    let (code, harness, provider) = run(
        environments.path(),
        &workspace,
        vec![
            tool_call_response(vec![shell_call("s1", "true")]),
            heredoc("s2", "out.txt", "hi"),
            finish("f1", "true"),
            tool_call_response(vec![shell_call("s3", "true")]),
            finish("f2", "true"),
            text_response("done"),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        finish_results(&provider),
        vec![
            with_no_runs("You changed files after running `true`. Run it again, then finish."),
            "Finished.".to_string(),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "hi\n",
        "the heredoc really wrote the workspace the check measured"
    );
}

/// The tie ADR-0055 item 2 decides: the run and the change are ONE record, so the
/// command that produced the change counts as the last file change's own run — the
/// frozen ADR-0037 test requires exactly that (`rm marker` stays a run that counts
/// after it removed the file) — while EVERY earlier run is stale, which is the point
/// of the ADR. The rejection also shows the changing command as the run that would
/// be accepted now.
#[tokio::test]
async fn the_changing_commands_own_run_counts_and_every_earlier_run_is_stale() {
    let workspace = git_workspace();
    let environments = tempdir().unwrap();
    shell_environment(environments.path());

    let (code, harness, provider) = run(
        environments.path(),
        &workspace,
        vec![
            tool_call_response(vec![shell_call("s1", "true")]),
            tool_call_response(vec![shell_call("s2", "echo x > marker.txt")]),
            finish("f1", "true"),
            finish("f2", "echo x > marker.txt"),
            text_response("done"),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        finish_results(&provider),
        vec![
            with_runs(
                "You changed files after running `true`. Run it again, then finish.",
                &["echo x > marker.txt"],
            ),
            "Finished.".to_string(),
        ]
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("marker.txt")).unwrap(),
        "x\n"
    );
}
