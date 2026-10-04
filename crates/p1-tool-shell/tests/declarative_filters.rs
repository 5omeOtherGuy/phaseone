//! The declarative filter tier through the public seam
//! (`docs/design/tools.md` §"`shell` output filters", research #42).
//!
//! The vendored `guest/src/filter/data/*.json` files (the donor's TOML filters,
//! converted) cover the tool classes no structured filter parses. These tests
//! prove, through the real tool, that such a class is summarised, that a
//! failing run keeps its error lines, and that a structured class
//! (`cargo build`) is still owned by its structured filter.
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

/// A helm run: klog `W....` warnings and blank lines are stripped, the release
/// lines are the signal the model needs.
const HELM: &str = "W0930 10:00:00.000000 1 warnings.go:70] deprecated API\n\nNAME: app\n\
                    STATUS: deployed\n\nREVISION: 3\n";

/// A make log that is all sub-make directory chatter: the vendored filter's
/// `on_empty` message stands in for it on success.
const MAKE_CHATTER: &str =
    "make[1]: Entering directory '/w/sub'\nmake[1]: Leaving directory '/w/sub'\n";

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
            call_id: "declarative-filter-call".into(),
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

/// A vendored declarative class is summarised and the marker says the log is
/// one call away.
#[tokio::test]
async fn a_declarative_filter_summarises_a_recognised_tool_class() {
    let harness = Harness::new();
    harness.replay("helm", HELM, 0);

    let outcome = harness.run("helm upgrade app ./chart", false).await;

    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert_eq!(
        outcome.content,
        format!("NAME: app\nSTATUS: deployed\nREVISION: 3\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );
}

/// Dispatch looks through shell plumbing, as it does for structured filters.
#[tokio::test]
async fn a_declarative_filter_dispatches_through_shell_plumbing() {
    let harness = Harness::new();
    harness.replay("helm", HELM, 0);

    for command in [
        // `cd .`: the directory must exist or `helm` never runs.
        "cd . && helm upgrade app ./chart",
        "KUBECONFIG=/k helm upgrade app ./chart",
        "command helm upgrade app ./chart",
    ] {
        let outcome = harness.run(command, false).await;
        assert!(outcome.content.contains(MARKER), "{command}: {outcome:?}");
    }
}

/// A filter that empties the output falls back to the raw log, and where the
/// vendored filter defines `on_empty` the message stands in for it.
#[tokio::test]
async fn a_declarative_filter_emptying_output_yields_raw_or_on_empty() {
    let harness = Harness::new();
    harness.replay("make", MAKE_CHATTER, 0);

    let outcome = harness.run("make all", false).await;
    assert_eq!(
        outcome.content,
        format!("make: ok\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );

    // The same emptied output on a failed run: the success-flavored message is
    // suppressed and the raw log stands.
    let harness = Harness::new();
    harness.replay("make", MAKE_CHATTER, 2);

    let outcome = harness.run("make all", false).await;
    assert_eq!(
        outcome.content,
        format!("{}\n[exit code: 2]", MAKE_CHATTER.trim_end()),
        "{outcome:?}"
    );
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}

/// A failing recursive make keeps every error line, the sub-make's included.
#[tokio::test]
async fn a_failing_recursive_make_keeps_its_error_lines() {
    let raw = "make[1]: Entering directory '/w/sub'\ngcc foo.c\nfoo.c:1:1: error: boom\n\
               make[1]: *** [Makefile:10: all] Error 2\nmake[1]: Leaving directory '/w/sub'\n\
               make: *** [Makefile:5: subdir] Error 2\n";
    let harness = Harness::new();
    harness.replay("make", raw, 2);

    let outcome = harness.run("make", false).await;

    assert_eq!(
        outcome.content,
        format!(
            "gcc foo.c\nfoo.c:1:1: error: boom\nmake[1]: *** [Makefile:10: all] Error 2\n\
             make: *** [Makefile:5: subdir] Error 2\n{MARKER}\n[exit code: 2]"
        ),
        "{outcome:?}"
    );
}

/// A structured class keeps its structured filter: a declarative filter must
/// not summarise `cargo build` output.
#[tokio::test]
async fn structured_filters_keep_precedence_over_declarative_filters() {
    let harness = Harness::new();
    harness.replay("cargo", CARGO_BUILD, 0);

    let outcome = harness.run("cargo build", false).await;

    assert_eq!(
        outcome.content,
        format!("ok\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );
}

/// `raw: true` bypasses the declarative tier as well.
#[tokio::test]
async fn raw_true_bypasses_the_declarative_tier() {
    let harness = Harness::new();
    harness.replay("helm", HELM, 0);

    let outcome = harness.run("helm upgrade app ./chart", true).await;

    assert_eq!(
        outcome.content,
        format!("{}\n[exit code: 0]", HELM.trim_end_matches('\n')),
        "{outcome:?}"
    );
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}

/// A gradle run with up-to-date tasks: #507, the gradle pattern never matched.
const GRADLE: &str = "> Configuring project :app\n> Task :app:compileJava UP-TO-DATE\n\
                      > Task :app:compileKotlin UP-TO-DATE\n> Task :app:test\n\n\
                      3 tests completed, 1 failed\n\nBUILD FAILED in 12s\n";

/// A g++ run behind an include chain: #507, the gcc pattern never matched `g++`.
const GCC: &str = "In file included from /usr/include/stdio.h:42:\n                 from main.c:1:\n\
                   main.c:10:5: error: use of undeclared identifier 'foo'\n    foo();\n    ^\n\
                   1 error generated.\n";

/// #507: `gradle build`, `./gradlew build`, `g++ main.cpp` and `gcc -O2 a.c` select
/// their filters through the real tool.
#[tokio::test]
async fn gradle_and_gcc_commands_select_their_filters() {
    let harness = Harness::new();
    harness.replay("gradle", GRADLE, 1);
    harness.replay("g++", GCC, 1);
    harness.replay("gcc", GCC, 1);
    std::fs::copy(
        harness.bin.join("gradle"),
        harness.workspace.path().join("gradlew"),
    )
    .unwrap();

    for command in [
        "gradle build",
        "./gradlew build",
        "g++ main.cpp",
        "gcc -O2 a.c",
    ] {
        let outcome = harness.run(command, false).await;
        assert!(outcome.content.contains(MARKER), "{command}: {outcome:?}");
        assert!(
            outcome.content.contains("error") || outcome.content.contains("BUILD FAILED"),
            "{command}: the failure lines survive: {outcome:?}"
        );
    }
}

/// #509 item 5: a sequence whose earlier segment prints mixes that output into
/// the capture, so the declarative tier leaves it raw; a silent `cd` prefix
/// does not.
#[tokio::test]
async fn an_ambiguous_shell_shape_keeps_the_raw_output() {
    let harness = Harness::new();
    harness.replay("helm", HELM, 0);

    for command in [
        "echo start; helm upgrade app ./chart",
        "echo start && helm upgrade app ./chart",
    ] {
        let outcome = harness.run(command, false).await;
        assert!(!outcome.content.contains(MARKER), "{command}: {outcome:?}");
        assert!(
            outcome.content.starts_with("start\n"),
            "{command}: {outcome:?}"
        );
    }
    let outcome = harness.run("cd . && helm upgrade app ./chart", false).await;
    assert!(outcome.content.contains(MARKER), "{outcome:?}");
}

/// A jq result padded with blank lines: the vendored filter strips them.
const JQ: &str = "{\n\n  \"name\": \"app\",\n\n  \"version\": \"1.0\"\n\n}\n";

/// #521: the shell ends a program's name at a redirection, so `jq<input.json`
/// and `make<Makefile` select their filters through the real tool.
#[tokio::test]
async fn a_redirection_right_after_a_name_selects_its_filter() {
    let harness = Harness::new();
    harness.replay("jq", JQ, 0);
    harness.replay("make", MAKE_CHATTER, 0);
    for file in ["input.json", "Makefile"] {
        std::fs::write(harness.workspace.path().join(file), "").unwrap();
    }

    let outcome = harness.run("jq<input.json", false).await;
    assert_eq!(
        outcome.content,
        format!("{{\n  \"name\": \"app\",\n  \"version\": \"1.0\"\n}}\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );

    let outcome = harness.run("make<Makefile", false).await;
    assert_eq!(
        outcome.content,
        format!("make: ok\n{MARKER}\n[exit code: 0]"),
        "{outcome:?}"
    );
}
