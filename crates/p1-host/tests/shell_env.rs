//! The host's `--env-pass` must-pass examples from `docs/design/tools.md`
//! §"`shell` environment — an allow-list, always": the injected snapshot is
//! filtered by the shell allow-list, `--env-pass NAME` adds a name, and a
//! malformed name is a usage error (exit 2). The provider is a scripted fake: no
//! network, no real credential file, and the real process environment is never
//! printed.

mod common;

use std::ffi::OsString;
use std::process::Command;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::Item;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

const CANARY_TOKEN: &str = "CANARY_TOKEN";
const SSH_AUTH_SOCK: &str = "SSH_AUTH_SOCK";
const MY_TOOL_HOME: &str = "MY_TOOL_HOME";
const MY_TOOL_HOME_VALUE: &str = "/opt/t";

/// The value of `NAME=` in `env`-style output, or `None` when the name is absent.
fn value<'a>(content: &'a str, name: &str) -> Option<&'a str> {
    content
        .lines()
        .find_map(|line| line.strip_prefix(name)?.strip_prefix('='))
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

/// The injected snapshot. `PATH` comes from the process so `bash` can be found;
/// it is never printed.
fn snapshot() -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("PATH"),
            std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        ),
        (OsString::from("LC_ALL"), OsString::from("C")),
        (OsString::from(CANARY_TOKEN), OsString::from("secret-1")),
        (OsString::from(SSH_AUTH_SOCK), OsString::from("/x")),
        (
            OsString::from(MY_TOOL_HOME),
            OsString::from(MY_TOOL_HOME_VALUE),
        ),
    ]
}

/// Run one scripted `shell` call through the whole host and return the tool
/// results the provider saw. `extra_args` are inserted before `--env`.
async fn run_shell(extra_args: &[&str], with_pass: bool) -> Vec<String> {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "shell", "{\"command\":\"env\"}")]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.shell_env = Some(snapshot());

    let mut args = vec!["--yes"];
    args.extend_from_slice(extra_args);
    if with_pass {
        args.extend_from_slice(&["--env-pass", MY_TOOL_HOME]);
    }
    args.extend_from_slice(&[
        "--env",
        "plain",
        "--workspace",
        workspace.path().to_str().unwrap(),
        "go",
    ]);
    let code = run_args(&mut harness, &args).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    tool_results(&requests.last().unwrap().history)
        .iter()
        .map(|content| content.to_string())
        .collect()
}

/// `--env-pass MY_TOOL_HOME` reaches the `shell` tool: the snapshot's name is
/// passed on, the allow-list still drops the canary.
#[tokio::test]
async fn env_pass_reaches_the_shell_tool() {
    let results = run_shell(&[], true).await;

    assert!(
        results
            .iter()
            .any(|content| value(content, MY_TOOL_HOME) == Some(MY_TOOL_HOME_VALUE)),
        "the passed name must reach the child: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|content| value(content, "LC_ALL") == Some("C")),
        "LC_ALL comes from the allow-list: {results:?}"
    );
    assert!(
        !results
            .iter()
            .any(|content| value(content, CANARY_TOKEN).is_some()),
        "the canary must not reach the child: {results:?}"
    );
    assert!(
        !results
            .iter()
            .any(|content| value(content, SSH_AUTH_SOCK).is_some()),
        "the agent socket must not reach the child: {results:?}"
    );
}

/// Without the flag the name is not in the built-in allow-list, so it stays out.
#[tokio::test]
async fn without_env_pass_the_snapshot_name_is_not_passed() {
    let results = run_shell(&[], false).await;

    assert!(
        !results
            .iter()
            .any(|content| value(content, MY_TOOL_HOME).is_some()),
        "MY_TOOL_HOME needs --env-pass: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|content| value(content, "LC_ALL") == Some("C")),
        "LC_ALL comes from the allow-list: {results:?}"
    );
}

/// A name with `=` or an empty name is a usage error (exit 2), and `--help`
/// lists the flag. These run the real binary, like `tests/cli.rs`.
#[test]
fn a_malformed_env_pass_name_exits_2_and_help_lists_the_flag() {
    let p1 = || Command::new(env!("CARGO_BIN_EXE_p1"));

    let output = p1()
        .args(["--env-pass", "A=B", "--yes", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--env-pass"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = p1()
        .args(["--env-pass", "", "--yes", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));

    let output = p1().arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("--env-pass NAME"),
        "help: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}
