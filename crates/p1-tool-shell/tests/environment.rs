//! The `shell` environment allow-list's must-pass examples from
//! `docs/design/tools.md` §"`shell` environment — an allow-list, always".
//!
//! Every test injects an environment snapshot with `with_env_snapshot`; none
//! reads or mutates the process environment (`set_var`/`remove_var` are never
//! called), and no test reads a real credential file. The sandboxed cases need a
//! real `bwrap` and print `SKIP: bwrap unusable here` when the probe fails. On
//! this machine bwrap works, so they run.

use std::ffi::OsString;
use std::path::Path;

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_shell::{Sandbox, ShellTool};
use p1_workspace::{ToolFace, Workspace};

const CANARY_TOKEN: &str = "CANARY_TOKEN";
const CANARY_VALUE: &str = "secret-1";
const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";
const SSH_AUTH_SOCK_VALUE: &str = "/x";
const MY_TOOL_HOME: &str = "MY_TOOL_HOME";
const MY_TOOL_HOME_VALUE: &str = "/opt/t";

/// Whether a throwaway real `bwrap` can run here at all.
fn bwrap_usable() -> bool {
    std::process::Command::new("bwrap")
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

/// The sandboxed cases need a working bwrap; without one they are skipped loudly.
macro_rules! require_bwrap {
    () => {
        if !bwrap_usable() {
            eprintln!("SKIP: bwrap unusable here");
            return;
        }
    };
}

/// The spec's must-pass snapshot. `PATH` is taken from the process so `bash` and
/// `bwrap` can still be resolved by `Command`; nothing here is a real secret and
/// none of it is printed.
fn snapshot() -> Vec<(OsString, OsString)> {
    vec![
        (OsString::from("PATH"), process_path()),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from(CANARY_TOKEN), OsString::from(CANARY_VALUE)),
        (
            OsString::from(SSH_AUTH_SOCK),
            OsString::from(SSH_AUTH_SOCK_VALUE),
        ),
        (
            OsString::from(MY_TOOL_HOME),
            OsString::from(MY_TOOL_HOME_VALUE),
        ),
    ]
}

fn process_path() -> OsString {
    std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin"))
}

/// The value of `NAME=` in `env`-style output, or `None` when the name is absent.
fn value<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
}

fn call(command: &str) -> ToolCall {
    ToolCall {
        call_id: "env-call".into(),
        name: "shell".into(),
        input: ToolInput::Json(serde_json::json!({ "command": command }).to_string()),
    }
}

async fn execute(tool: &ShellTool, command: &str) -> ToolOutcome {
    let call = call(command);
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await
}

/// The default tool with the spec snapshot injected, before any sandbox.
fn tool(root: &Path) -> ShellTool {
    ShellTool::new(Workspace::new(root).unwrap()).with_env_snapshot(snapshot())
}

/// A tool whose pass-list was extended with `MY_TOOL_HOME`.
fn tool_with_pass(root: &Path) -> ShellTool {
    tool(root).with_env_pass(vec![MY_TOOL_HOME.to_string()])
}

// ------------------------------------------------------------------ (plain)

/// `env` sees `PATH` and `LC_ALL` from the snapshot and none of the names the
/// allow-list excludes.
#[tokio::test]
async fn env_allow_list_hides_secrets_and_keeps_allowed_names() {
    let dir = tempfile::tempdir().unwrap();
    let tool = tool(dir.path());

    let outcome = execute(&tool, "env").await;

    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert!(
        value(&outcome.content, "PATH").is_some(),
        "PATH must be passed through: {outcome:?}"
    );
    assert_eq!(value(&outcome.content, "LC_ALL"), Some("C"), "{outcome:?}");
    assert_eq!(
        value(&outcome.content, CANARY_TOKEN),
        None,
        "the canary leaked: {outcome:?}"
    );
    assert_eq!(
        value(&outcome.content, SSH_AUTH_SOCK),
        None,
        "the socket leaked: {outcome:?}"
    );
    assert_eq!(
        value(&outcome.content, MY_TOOL_HOME),
        None,
        "MY_TOOL_HOME needs --env-pass/with_env_pass: {outcome:?}"
    );
}

/// `MY_TOOL_HOME` is absent without the pass-list and present with it; the
/// canary stays absent either way.
#[tokio::test]
async fn env_pass_adds_a_name_to_the_allow_list() {
    let dir = tempfile::tempdir().unwrap();

    let without = execute(&tool(dir.path()), "env").await;
    assert_eq!(value(&without.content, MY_TOOL_HOME), None, "{without:?}");

    let outcome = execute(&tool_with_pass(dir.path()), "env").await;
    assert_eq!(
        value(&outcome.content, MY_TOOL_HOME),
        Some(MY_TOOL_HOME_VALUE),
        "{outcome:?}"
    );
    assert_eq!(value(&outcome.content, CANARY_TOKEN), None, "{outcome:?}");
}

/// A snapshot with no `PATH` gets no `PATH` invented for it. A login `bash`
/// finds commands with a default path of its own, but does not export one, so
/// the child's environment simply has no `PATH`.
#[tokio::test]
async fn a_missing_path_is_not_invented() {
    let dir = tempfile::tempdir().unwrap();
    let tool = ShellTool::new(Workspace::new(dir.path()).unwrap())
        .with_env_snapshot(vec![(OsString::from("LC_ALL"), OsString::from("C"))]);

    // `env` is found and run through bash's own default path...
    let outcome = execute(&tool, "env").await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    // ...but no `PATH` reaches the environment we passed on.
    assert_eq!(
        value(&outcome.content, "PATH"),
        None,
        "the missing PATH must not be invented: {outcome:?}"
    );
    assert_eq!(value(&outcome.content, "LC_ALL"), Some("C"), "{outcome:?}");
}

// ---------------------------------------------------------------- (sandbox)

/// The same allow-list through the real sandbox: `TMPDIR=/tmp` from bubblewrap
/// joins it, the canaries stay out, and `with_env_pass` still adds a name.
#[tokio::test]
async fn env_allow_list_applies_through_the_sandbox() {
    require_bwrap!();
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let sandbox = |tool: ShellTool| {
        tool.sandboxed(Sandbox::for_home(home.path()))
            .expect("the bwrap probe must succeed once bwrap_usable() is true")
    };

    let without = sandbox(tool(&workspace));
    let outcome = execute(&without, "env").await;
    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert!(
        value(&outcome.content, "PATH").is_some(),
        "PATH must be passed through: {outcome:?}"
    );
    assert_eq!(value(&outcome.content, "LC_ALL"), Some("C"), "{outcome:?}");
    assert_eq!(
        value(&outcome.content, "TMPDIR"),
        Some("/tmp"),
        "bubblewrap still sets TMPDIR after the allow-list: {outcome:?}"
    );
    assert_eq!(value(&outcome.content, CANARY_TOKEN), None, "{outcome:?}");
    assert_eq!(value(&outcome.content, SSH_AUTH_SOCK), None, "{outcome:?}");
    assert_eq!(value(&outcome.content, MY_TOOL_HOME), None, "{outcome:?}");

    let with = sandbox(tool_with_pass(&workspace));
    let outcome = execute(&with, "env").await;
    assert_eq!(
        value(&outcome.content, MY_TOOL_HOME),
        Some(MY_TOOL_HOME_VALUE),
        "{outcome:?}"
    );
    assert_eq!(value(&outcome.content, CANARY_TOKEN), None, "{outcome:?}");
}

/// Requirement 3: `with_face` and `sandboxed` compose in either order with the
/// env builders, and the snapshot and pass-list survive.
#[tokio::test]
async fn the_env_snapshot_and_pass_list_survive_face_and_sandbox_in_any_order() {
    require_bwrap!();
    let home = tempfile::tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();

    let env_first = ShellTool::new(Workspace::new(&workspace).unwrap())
        .with_env_snapshot(snapshot())
        .with_env_pass(vec![MY_TOOL_HOME.to_string()])
        .with_face(ToolFace::new("Run", "custom"), "gpt")
        .sandboxed(Sandbox::for_home(home.path()))
        .unwrap();
    let sandbox_first = ShellTool::new(Workspace::new(&workspace).unwrap())
        .sandboxed(Sandbox::for_home(home.path()))
        .unwrap()
        .with_face(ToolFace::new("Run", "custom"), "gpt")
        .with_env_pass(vec![MY_TOOL_HOME.to_string()])
        .with_env_snapshot(snapshot());

    for tool in [&env_first, &sandbox_first] {
        assert_eq!(tool.declaration().name, "Run");
        assert_eq!(tool.identity().variant, "gpt+sandbox");
        let outcome = execute(tool, "env").await;
        assert_eq!(
            value(&outcome.content, MY_TOOL_HOME),
            Some(MY_TOOL_HOME_VALUE),
            "{outcome:?}"
        );
        assert_eq!(value(&outcome.content, CANARY_TOKEN), None, "{outcome:?}");
        assert_eq!(value(&outcome.content, SSH_AUTH_SOCK), None, "{outcome:?}");
        assert_eq!(value(&outcome.content, "LC_ALL"), Some("C"), "{outcome:?}");
    }
}
