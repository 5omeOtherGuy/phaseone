//! The declarative TOML filter tier through the public seam
//! (`docs/design/tools.md` §"`shell` output filters", research #42).
//!
//! The vendored `guest/src/filter/data/*.toml` files cover the tool classes no
//! structured filter parses. These tests prove, through the real tool, that a
//! TOML class is summarised, that a failing run keeps its error lines, and that
//! a structured class (`cargo test`) is still owned by its structured filter.
//!
//! Each replayed output is data in a private `bin` program, never code.

use std::ffi::OsString;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;

use p1_contracts::{
    CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome, ToolStatus,
};
use p1_tool_shell::ShellTool;
use p1_workspace::Workspace;

/// Last line of a summarised result, before the exit-code footer.
const MARKER: &str = "[output filtered; pass raw:true for the full log]";

/// The system half of `PATH`: what `bash -lc` and the replay scripts need.
const SYSTEM_PATH: &str = "/usr/bin:/bin";

/// A shellcheck run: blank lines only are stripped, the `^-- SC....` lines are
/// the signal the model needs.
const SHELLCHECK: &str = "\nIn script.sh line 3:\nif [[ $1 == \"\" ]]\n     ^-- SC2236: Use \
                           -z instead of ! -n.\n\nIn script.sh line 7:\necho $var\n     ^-- \
                           SC2086: Double quote to prevent globbing.\n\n";

/// An installer log that is all deprecation noise: the vendored filter's
/// `on_empty` message stands in for it on success.
const NPM_NOISE: &str = "npm warn deprecated inflight@1.0.6: This module is not supported, and \
                         leaks memory.\nnpm warn deprecated har-validator@5.1.5: this library is \
                         no longer supported\n";

/// `cargo build` chatter: the structured filter's whole job.
const CARGO_BUILD: &str = "   Compiling foo v0.1.0 (/w/foo)\n    Finished `dev` profile [unoptimized \
                           + debuginfo] target(s) in 1.0s\n";

struct Harness {
    workspace: tempfile::TempDir,
    bin: PathBuf,
}

impl Harness {
    fn new() -> Self {
        let workspace = tempfile::tempdir().unwrap();
        let bin = workspace.path().join("bin");
        std::fs::create_dir_all(&bin).unwrap();
        Self { workspace, bin }
    }

    /// Install `program` so it prints `output` byte for byte and exits `code`.
    fn replay(&self, program: &str, output: &str, code: i32) {
        let data = self.bin.join(format!("{program}.out"));
        std::fs::write(&data, output).unwrap();
        let script = format!("#!/bin/sh\ncat '{}'\nexit {code}\n", data.display());
        let path = self.bin.join(program);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Run `command` with the fake programs first on `PATH` and return the
    /// outcome the model would see.
    async fn run(&self, command: &str, raw: bool) -> ToolOutcome {
        let path = format!("{}:{SYSTEM_PATH}", self.bin.display());
        let input = serde_json::json!({ "command": format!("PATH={path} {command}"), "raw": raw })
            .to_string();
        let call = ToolCall {
            call_id: "toml-filter-call".into(),
            name: "shell".into(),
            input: ToolInput::Json(input),
        };
        let tool = ShellTool::new(Workspace::new(self.workspace.path()).unwrap())
            .with_env_snapshot(vec![
                (OsString::from("PATH"), OsString::from(path)),
                (
                    OsString::from("HOME"),
                    self.workspace.path().as_os_str().to_owned(),
                ),
                (OsString::from("LC_ALL"), OsString::from("C")),
            ]);
        tool.execute(
            &call,
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await
    }
}

/// A vendored TOML class is summarised and the marker says the log is one call
/// away.
#[tokio::test]
async fn a_toml_filter_summarises_a_recognised_tool_class() {
    let harness = Harness::new();
    harness.replay("shellcheck", SHELLCHECK, 0);

    let outcome = harness.run("shellcheck script.sh", false).await;

    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert!(outcome.content.contains("SC2236"), "{outcome:?}");
    assert!(outcome.content.contains("SC2086"), "{outcome:?}");
    // Blank lines stripped: the summary is smaller than the capture.
    assert!(
        outcome
            .content
            .ends_with(&format!("{MARKER}\n[exit code: 0]")),
        "{outcome:?}"
    );
}

/// Dispatch looks through shell plumbing, as it does for structured filters.
#[tokio::test]
async fn a_toml_filter_dispatches_through_shell_plumbing() {
    let harness = Harness::new();
    harness.replay("shellcheck", SHELLCHECK, 0);

    for command in [
        // `cd .`: the directory must exist or `shellcheck` never runs.
        "cd . && shellcheck script.sh",
        "SHELLCHECK_OPTS= shellcheck script.sh",
        "command shellcheck script.sh",
    ] {
        let outcome = harness.run(command, false).await;
        assert!(outcome.content.contains(MARKER), "{command}: {outcome:?}");
    }
}

/// A filter that empties the output falls back to the raw log, and where the
/// vendored filter defines `on_empty` the message stands in for it.
#[tokio::test]
async fn a_toml_filter_emptying_output_yields_raw_or_on_empty() {
    let harness = Harness::new();
    harness.replay("npm", NPM_NOISE, 0);

    let outcome = harness.run("npm install", false).await;
    assert_eq!(
        outcome.content,
        format!("ok (installed; log was all noise)\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );

    // The same emptied output on a failed run: the success-flavored message is
    // suppressed and the raw log stands.
    let harness = Harness::new();
    harness.replay("npm", NPM_NOISE, 1);

    let outcome = harness.run("npm install", false).await;
    assert_eq!(
        outcome.content,
        format!("{}\n[exit code: 1]", NPM_NOISE.trim_end()),
        "{outcome:?}"
    );
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}

/// A structured class keeps its structured filter: a TOML filter must not
/// summarise `cargo build` output.
#[tokio::test]
async fn structured_filters_keep_precedence_over_toml_filters() {
    let harness = Harness::new();
    harness.replay("cargo", CARGO_BUILD, 0);

    let outcome = harness.run("cargo build", false).await;

    assert_eq!(
        outcome.content,
        format!("ok\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );
}

/// `raw: true` bypasses the TOML tier as well.
#[tokio::test]
async fn raw_true_bypasses_the_toml_tier() {
    let harness = Harness::new();
    harness.replay("shellcheck", SHELLCHECK, 0);

    let outcome = harness.run("shellcheck script.sh", true).await;

    assert_eq!(
        outcome.content,
        format!("{}\n[exit code: 0]", SHELLCHECK.trim_end_matches('\n')),
        "{outcome:?}"
    );
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}
