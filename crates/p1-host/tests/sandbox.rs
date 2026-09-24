//! Whole-host tests for the `shell` sandbox: `--sandbox workspace` reaches the
//! `shell` tool through the catalog, `--sandbox-write` is a usage error on its
//! own, and a `SandboxError` fails assembly before any provider request.
//!
//! The sandboxed end-to-end case needs a real `bwrap`; it prints
//! `SKIP: bwrap unusable here` and returns when the probe fails. On this machine
//! bwrap works, so it runs.

mod common;

use std::process::Command;

use common::{Harness, provider_hook, run_args, shipped_environments, write_environment};
use p1_contracts::{Item, ToolCall, ToolInput};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

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

fn tool_results(history: &[Item]) -> Vec<&str> {
    history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.as_str()),
            _ => None,
        })
        .collect()
}

/// Requirement 6 (1): a scripted `shell` call cannot write above the workspace,
/// and one inside it works.
#[tokio::test]
async fn workspace_sandbox_blocks_an_escape_and_allows_an_inside_write() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
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
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            "{\"command\":\"echo x > ../escape.txt\"}",
        )]),
        tool_call_response(vec![json_call(
            "c2",
            "shell",
            "{\"command\":\"echo x > inside.txt\"}",
        )]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        !home.path().join("escape.txt").exists(),
        "the escape file must not exist"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("inside.txt"))
            .unwrap()
            .trim(),
        "x"
    );
    let final_request = handle.requests();
    let results = tool_results(&final_request.last().unwrap().history);
    assert!(
        results
            .iter()
            .any(|content| content.contains("[exit code: 1]")),
        "the escape must report a non-zero exit: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|content| content.contains("[exit code: 0]")),
        "the inside write must succeed: {results:?}"
    );
}

/// `--sandbox-write PATH` keeps an extra path writable end to end: the CLI flag
/// reaches the tool's `Sandbox::writable`.
#[tokio::test]
async fn sandbox_write_keeps_a_path_writable_end_to_end() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let extra = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let write_extra = serde_json::json!({
        "command": format!("echo w > '{}/w.txt'", extra.path().display())
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "shell", &write_extra)]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-write",
            extra.path().to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        std::fs::read_to_string(extra.path().join("w.txt"))
            .unwrap()
            .trim(),
        "w"
    );
}

/// Issue #84: `--sandbox-write DIR` is also a writable root for the FILE tools,
/// so the two agree on where the agent may write. A `write` call with an
/// absolute path under the granted root succeeds; it needs no shell, so this
/// runs even where `bwrap` does not.
#[tokio::test]
async fn sandbox_write_makes_the_file_tools_accept_a_granted_root() {
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let out = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["write"],
        "test",
    );
    let target = out.path().join("result.txt");
    let write_call = serde_json::json!({
        "file_path": target.to_str().unwrap(),
        "content": "verdict"
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "write", &write_call)]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-write",
            out.path().to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "verdict");
}

/// Without a grant the file tools are unchanged: an absolute path outside the
/// workspace is still refused and nothing is written.
#[tokio::test]
async fn without_a_grant_the_file_tools_still_refuse_an_outside_path() {
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let out = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["write"],
        "test",
    );
    let target = out.path().join("result.txt");
    let write_call = serde_json::json!({
        "file_path": target.to_str().unwrap(),
        "content": "verdict"
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "write", &write_call)]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(!target.exists(), "the outside path must not be written");
    let final_request = handle.requests();
    let results = tool_results(&final_request.last().unwrap().history);
    assert!(
        results
            .iter()
            .any(|content| content.contains("escapes workspace")),
        "the refusal must name the workspace boundary: {results:?}"
    );
}

/// Issue #84: inside a granted root the ordinary file-tool rules hold — `read`
/// observes the file and a following `edit` succeeds, with no shell involved.
#[tokio::test]
async fn sandbox_write_makes_read_and_edit_work_inside_a_granted_root() {
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let out = tempdir().unwrap();
    let target = out.path().join("result.txt");
    std::fs::write(&target, "before").unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read", "edit"],
        "test",
    );
    let read_call = serde_json::json!({ "file_path": target.to_str().unwrap() }).to_string();
    let edit_call = serde_json::json!({
        "file_path": target.to_str().unwrap(),
        "old_string": "before",
        "new_string": "after"
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "read", &read_call)]),
        tool_call_response(vec![json_call("c2", "edit", &edit_call)]),
        text_response("done"),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-write",
            out.path().to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "after");
}

/// Issue #84 / the reported gpt job: `apply_patch`, the gpt environment's file
/// tool, also accepts an absolute path under a granted root, so a job can write
/// the result file it was asked for.
#[tokio::test]
async fn sandbox_write_makes_apply_patch_accept_a_granted_root() {
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let out = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["apply_patch"],
        "test",
    );
    let target = out.path().join("result.txt");
    let call = ToolCall {
        call_id: "c1".into(),
        name: "apply_patch".into(),
        input: ToolInput::Text(format!(
            "*** Begin Patch\n*** Add File: {}\n+verdict\n*** End Patch\n",
            target.display()
        )),
    };
    let provider =
        ScriptedProvider::new(vec![tool_call_response(vec![call]), text_response("done")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-write",
            out.path().to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(std::fs::read_to_string(&target).unwrap(), "verdict\n");
}

/// `--sandbox-read PATH` keeps an extra path visible READ-ONLY end to end: the
/// CLI flag reaches the tool's `Sandbox::readable`.
#[tokio::test]
async fn sandbox_read_keeps_a_path_readable_end_to_end() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    // A directory inside the hidden home, so only `--sandbox-read` can expose it.
    let shared = home.path().join("shared");
    std::fs::create_dir_all(&shared).unwrap();
    std::fs::write(shared.join("note.txt"), "readable-note").unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let read_note = serde_json::json!({
        "command": format!("cat '{}/note.txt'", shared.display())
    })
    .to_string();
    let write_note = serde_json::json!({
        "command": format!("touch '{}/new.txt'", shared.display())
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "shell", &read_note)]),
        tool_call_response(vec![json_call("c2", "shell", &write_note)]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-read",
            shared.to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        !shared.join("new.txt").exists(),
        "the path must stay read-only"
    );
    let requests = handle.requests();
    let results = tool_results(&requests.last().unwrap().history);
    assert!(
        results
            .iter()
            .any(|content| content.contains("readable-note")),
        "the readable file must be visible: {results:?}"
    );
    assert!(
        results
            .iter()
            .any(|content| content.contains("[exit code: 1]")),
        "writing the readable path must fail: {results:?}"
    );
}

/// A readable path inside a credential directory fails assembly before any
/// provider request (the check needs no `bwrap`).
#[tokio::test]
async fn a_credential_sandbox_read_fails_assembly_before_any_provider_request() {
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![text_response("must not be reached")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-read",
            home.path().join(".ssh").to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 1, "stderr: {}", harness.stderr.text());
    assert!(
        handle.requests().is_empty(),
        "the provider must not be called"
    );
    assert!(
        harness.stderr.text().contains(".ssh"),
        "the error must name the credential directory: {}",
        harness.stderr.text()
    );
}

/// `--sandbox-read $HOME` would re-expose every credential directory, so it
/// fails assembly before any provider request (the check needs no `bwrap`).
#[tokio::test]
async fn sandbox_read_of_the_home_fails_assembly_before_any_provider_request() {
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![text_response("must not be reached")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--sandbox-read",
            home.path().to_str().unwrap(),
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 1, "stderr: {}", harness.stderr.text());
    assert!(
        handle.requests().is_empty(),
        "the provider must not be called"
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(home.path().to_str().unwrap()),
        "the error must name the readable path: {stderr}"
    );
    assert!(
        stderr.contains(".ssh"),
        "the error must name a credential directory: {stderr}"
    );
}

/// Requirement 6 (2): `env show` with the flag shows the sandbox paragraph and the
/// `+sandbox` identity variant.
#[tokio::test]
async fn env_show_with_sandbox_workspace_shows_the_sandbox_face() {
    // Assembly probes bwrap, so without it this exits 1 by design (covered by
    // `a_sandbox_error_fails_assembly_before_any_provider_request`).
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempdir().unwrap();
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    harness.deps.home = Some(home.path().to_path_buf());
    common::isolated_environment(&mut harness);

    let code = run_args(
        &mut harness,
        &["env", "show", "claude", "--sandbox", "workspace"],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let json = harness.stdout.text();
    assert!(
        json.contains("Commands run in a sandbox: only the workspace and /tmp are writable"),
        "the sandbox paragraph must be in the shell description: {json}"
    );
    assert!(
        json.contains("+sandbox"),
        "the identity variant must carry the suffix: {json}"
    );
}

/// Requirement 6 (3) and (4): the flag combinations `main` rejects exit 2, and
/// `--help` lists both flags. These run the real binary, like `tests/cli.rs`.
#[test]
fn sandbox_usage_errors_exit_2_and_help_lists_the_flags() {
    let p1 = || Command::new(env!("CARGO_BIN_EXE_p1"));

    let output = p1()
        .args(["--sandbox-write", "/tmp", "--yes", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--sandbox-write requires --sandbox workspace"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = p1()
        .args(["--sandbox", "bogus", "--yes", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("unknown sandbox mode `bogus`"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // `--sandbox-read` mirrors `--sandbox-write`: a usage error without the mode.
    let output = p1()
        .args(["--sandbox-read", "/tmp", "--yes", "hi"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr)
            .contains("--sandbox-read requires --sandbox workspace"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = p1().arg("--help").output().unwrap();
    assert!(output.status.success());
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--sandbox MODE"), "help: {help}");
    assert!(help.contains("--sandbox-write PATH"), "help: {help}");
    assert!(help.contains("--sandbox-read PATH"), "help: {help}");
}

/// The host fills `Sandbox::runtime_dir` from `XDG_RUNTIME_DIR`, so a socket in
/// the real runtime directory is not visible inside the sandbox.
#[tokio::test]
async fn the_host_replaces_the_runtime_dir_inside_the_sandbox() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let Some(base) = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from) else {
        eprintln!("SKIP: no usable XDG_RUNTIME_DIR here");
        return;
    };
    let Ok(runtime) = tempfile::Builder::new()
        .prefix("p1-sandbox-rt-")
        .tempdir_in(&base)
    else {
        eprintln!("SKIP: no usable XDG_RUNTIME_DIR here");
        return;
    };
    std::fs::write(runtime.path().join("agent.sock"), "socket").unwrap();
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let read_socket = serde_json::json!({
        "command": format!("cat '{}/agent.sock'", runtime.path().display())
    })
    .to_string();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "shell", &read_socket)]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.home = Some(home.path().to_path_buf());
    harness.deps.runtime_dir = Some(runtime.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--env",
            "plain",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = handle.requests();
    let results = tool_results(&requests.last().unwrap().history);
    assert!(
        !results.iter().any(|content| content.contains("socket")),
        "the runtime socket must not be visible: {results:?}"
    );
    assert!(
        !results
            .iter()
            .any(|content| content.contains("[exit code: 0]")),
        "reading the socket must fail: {results:?}"
    );
}

/// A `SandboxError` fails assembly — exit 1, before any provider request — with a
/// message that names the remedy. Workspace == home is the cheapest such error
/// and needs no `bwrap`.
#[tokio::test]
async fn a_sandbox_error_fails_assembly_before_any_provider_request() {
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
    let provider = ScriptedProvider::new(vec![text_response("must not be reached")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    // The workspace root IS the home: hiding the home would hide the workspace.
    harness.deps.home = Some(workspace.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 1, "stderr: {}", harness.stderr.text());
    assert!(
        handle.requests().is_empty(),
        "the provider must not be called"
    );
    assert!(
        harness.stderr.text().contains("--sandbox off"),
        "the error must name the remedy: {}",
        harness.stderr.text()
    );
}

/// `--sandbox off` is the default and leaves the description unchanged.
#[tokio::test]
async fn sandbox_off_keeps_the_plain_shell_description() {
    let home = tempdir().unwrap();
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    harness.deps.home = Some(home.path().to_path_buf());
    common::isolated_environment(&mut harness);

    let code = run_args(&mut harness, &["env", "show", "claude"]).await;

    assert_eq!(code, 0);
    assert!(!harness.stdout.text().contains("Commands run in a sandbox"));
    assert!(!harness.stdout.text().contains("+sandbox"));
}

/// The sandbox reaches a WORKER's `shell` too: the child factory assembles from
/// the same catalog, so the child's `shell` is sandboxed with the same home.
#[cfg(feature = "delegation")]
#[tokio::test]
async fn a_workers_shell_runs_in_the_sandbox_too() {
    if !bwrap_usable() {
        eprintln!("SKIP: bwrap unusable here");
        return;
    }
    let home = tempdir().unwrap();
    let workspace = home.path().join("ws");
    std::fs::create_dir_all(&workspace).unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "parent",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result"],
        "PARENT",
    );
    write_environment(
        environments.path(),
        "child",
        "fake-b",
        "model-b",
        &["shell"],
        "CHILD",
    );
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            "{\"environment\":\"child\",\"task\":\"work\",\"tools\":[\"shell\"]}",
        )]),
        // `wait` makes the child's whole turn finish before the parent continues,
        // so the test needs no gate and no timing assumption.
        tool_call_response(vec![json_call(
            "c2",
            "worker_result",
            "{\"id\":\"w1\",\"wait\":true}",
        )]),
        text_response("parent done"),
        // The child's completion notification wakes the parent for one inbox turn.
        text_response("parent notified"),
    ]);
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            "{\"command\":\"echo x > ../escape.txt; echo y > inside.txt\"}",
        )]),
        text_response("child done"),
    ]);
    let child_handle = child.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));
    harness.deps.home = Some(home.path().to_path_buf());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--sandbox",
            "workspace",
            "--env",
            "parent",
            "--workspace",
            workspace.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        !home.path().join("escape.txt").exists(),
        "the child's escape file must not exist"
    );
    assert_eq!(
        std::fs::read_to_string(workspace.join("inside.txt"))
            .unwrap()
            .trim(),
        "y"
    );
    let requests = child_handle.requests();
    let results = tool_results(&requests.last().unwrap().history);
    assert!(
        results
            .iter()
            .any(|content| content.contains("Read-only file system")),
        "the child's shell must be sandboxed: {results:?}"
    );
}
