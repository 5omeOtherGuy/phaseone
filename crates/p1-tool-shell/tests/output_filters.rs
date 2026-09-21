//! The `shell` tool's structured output filters through the public seam
//! (`docs/design/tools.md` §"`shell` output filters", research #42).
//!
//! Every fixture under `tests/fixtures/filter_corpus/` is a donor `iris-agent`
//! capture (`src/tools/bash/filter/corpus/`) with the donor's reduction bar and
//! verbatim-survival needles. Each one is replayed by a fake `cargo`/`git`/
//! `npm`/`npx` program in a private `bin`, so every assertion goes through the
//! real tool: `bash -lc` runs the command, the filter seam runs on the captured
//! bytes, the exit-code footer is appended last. No test reads the process
//! environment or a real repository.
//!
//! One line of the fail-safe contract cannot be provoked through a real
//! command — a filter that panics — so it is covered by the crate's
//! `filter::tests::a_panicking_filter_yields_raw` unit test.

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

/// One captured output plus the bars the donor measured it against.
struct Sample {
    /// Command class, for failure messages.
    class: &'static str,
    /// Command string as the model would send it (drives dispatch).
    command: &'static str,
    /// Fake program the command must resolve to.
    program: &'static str,
    /// Captured raw output.
    raw: &'static str,
    /// Exit code of the captured run.
    code: i32,
    /// Donor minimum token reduction (percent); noisy classes only.
    min_reduction: Option<u32>,
    /// Content that must survive filtering verbatim.
    must_contain: &'static [&'static str],
    /// True when no filter covers the class: the output passes through.
    expect_passthrough: bool,
}

const CARGO_TEST_PASS: &str = include_str!("fixtures/filter_corpus/cargo-test-pass.txt");
const CARGO_TEST_MULTI_PASS: &str =
    include_str!("fixtures/filter_corpus/cargo-test-multi-pass.txt");
const CARGO_TEST_FAIL: &str = include_str!("fixtures/filter_corpus/cargo-test-fail.txt");
const CARGO_BUILD_ERROR: &str = include_str!("fixtures/filter_corpus/cargo-build-error.txt");
const CARGO_BUILD_PASS: &str = include_str!("fixtures/filter_corpus/cargo-build-pass.txt");
const CARGO_CHECK_WARN: &str = include_str!("fixtures/filter_corpus/cargo-check-warn.txt");
const GIT_STATUS: &str = include_str!("fixtures/filter_corpus/git-status.txt");
const GIT_DIFF: &str = include_str!("fixtures/filter_corpus/git-diff.txt");
const GIT_DIFF_LOCKFILE: &str = include_str!("fixtures/filter_corpus/git-diff-lockfile.txt");
const GIT_LOG: &str = include_str!("fixtures/filter_corpus/git-log.txt");
const GIT_LOG_ONELINE: &str = include_str!("fixtures/filter_corpus/git-log-oneline.txt");
const NPM_TEST_PASS: &str = include_str!("fixtures/filter_corpus/npm-test-pass.txt");
const NPM_TEST_FAIL: &str = include_str!("fixtures/filter_corpus/npm-test-fail.txt");
const VITEST_PASS: &str = include_str!("fixtures/filter_corpus/vitest-pass.txt");
const VITEST_FAIL: &str = include_str!("fixtures/filter_corpus/vitest-fail.txt");
const NPM_INSTALL: &str = include_str!("fixtures/filter_corpus/npm-install.txt");
const SHELLCHECK: &str = include_str!("fixtures/filter_corpus/shellcheck.txt");

fn samples() -> Vec<Sample> {
    vec![
        Sample {
            class: "cargo test (pass)",
            command: "cargo test",
            program: "cargo",
            raw: CARGO_TEST_PASS,
            code: 0,
            min_reduction: Some(85),
            must_contain: &[
                "unittests src/lib.rs (passcrate): ok. 48 passed",
                "cargo test: ok. 48 passed",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo test (pass, workspace)",
            command: "cargo test",
            program: "cargo",
            raw: CARGO_TEST_MULTI_PASS,
            code: 0,
            min_reduction: Some(85),
            must_contain: &[
                "unittests src/lib.rs (alpha): ok. 2 passed",
                "unittests src/lib.rs (beta): ok. 2 passed",
                "unittests src/lib.rs (gamma): ok. 2 passed",
                "cargo test: ok. 6 passed (6 suites",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo test (fail)",
            command: "cargo test",
            program: "cargo",
            raw: CARGO_TEST_FAIL,
            code: 101,
            min_reduction: None,
            must_contain: &[
                "test tests::broken_math ... FAILED",
                "test tests::broken_greeting ... FAILED",
                "panicked at src/lib.rs:25:9",
                "panicked at src/lib.rs:30:9",
                "assertion `left == right` failed: two plus two should make five",
                "tests::broken_greeting",
                "tests::broken_math",
                "test result: FAILED. 2 passed; 2 failed",
                "error: test failed, to rerun pass `--lib`",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo build (compile error)",
            command: "cargo build",
            program: "cargo",
            raw: CARGO_BUILD_ERROR,
            code: 101,
            min_reduction: None,
            must_contain: &[
                "error[E0425]: cannot find value `missing_var` in this scope",
                "--> src/lib.rs:6:31",
                "error: could not compile `failcrate` (lib) due to 1 previous error",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo build (pass)",
            command: "cargo build",
            program: "cargo",
            raw: CARGO_BUILD_PASS,
            code: 0,
            min_reduction: Some(60),
            must_contain: &["ok"],
            expect_passthrough: false,
        },
        Sample {
            class: "cargo check (warnings)",
            command: "cargo check",
            program: "cargo",
            raw: CARGO_CHECK_WARN,
            code: 0,
            min_reduction: None,
            must_contain: &[
                "warning: unused variable: `unused`",
                "--> crates/beta/src/lib.rs:1:41",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git status",
            command: "git status",
            program: "git",
            raw: GIT_STATUS,
            code: 0,
            // The donor's per-class bar for the long-format status.
            min_reduction: Some(40),
            must_contain: &[
                "On branch feat/bash-output-filtering",
                "modified: src/tools/bash/mod.rs",
                "src/tools/bash/filter/",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git diff (source-heavy)",
            command: "git diff HEAD~2 HEAD~1",
            program: "git",
            raw: GIT_DIFF,
            code: 0,
            // Only lockfile churn may be reduced; source hunks are signal and
            // stay verbatim, so there is no bar on a source-heavy diff.
            min_reduction: None,
            must_contain: &[
                "Cargo.lock +57/-3 (lockfile, hunks omitted)",
                "diff --git a/src/ui/highlight.rs b/src/ui/highlight.rs",
                "@@ -0,0 +1,313 @@",
                "+//! Syntax highlighter for fenced Markdown code blocks (Tier 3).",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git diff (lockfile churn)",
            command: "git diff",
            program: "git",
            raw: GIT_DIFF_LOCKFILE,
            code: 0,
            min_reduction: Some(30),
            must_contain: &[
                "package-lock.json +14/-0 (lockfile, hunks omitted)",
                "-module.exports = function pad5(s) { return leftPad(s, 5); };",
                "+module.exports = function pad5(s) { return leftPad(String(s), 5); };",
                "+    \"inherits\": \"^2.0.4\",",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git log",
            command: "git log -n 12",
            program: "git",
            raw: GIT_LOG,
            code: 0,
            min_reduction: Some(60),
            must_contain: &[
                "c8e2dbba docs(adr): token-efficient tools by design (0036) and native bash output filtering (0037) (#342)",
                "12 commits by dangerouslyskippermissions on 2026-07-04",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "git log --oneline",
            command: "git log --oneline -n 12",
            program: "git",
            raw: GIT_LOG_ONELINE,
            code: 0,
            min_reduction: None,
            must_contain: &[],
            // Already compact and not the default format: the filter declines
            // and the output passes through untouched.
            expect_passthrough: true,
        },
        Sample {
            class: "npm test (pass)",
            command: "npm test -- --verbose",
            program: "npm",
            raw: NPM_TEST_PASS,
            code: 0,
            min_reduction: Some(60),
            must_contain: &["Tests:       5 passed, 5 total"],
            expect_passthrough: false,
        },
        Sample {
            class: "npm test (fail)",
            command: "npm test",
            program: "npm",
            raw: NPM_TEST_FAIL,
            code: 1,
            min_reduction: None,
            must_contain: &[
                "FAIL src/api.test.js",
                "● fetches user",
                "● handles missing user",
                "Expected: 404",
                "Received: 200",
                "at Object.toBe (src/api.test.js:2:47)",
                "at Object.toBe (src/api.test.js:3:55)",
                "Tests:       2 failed, 3 passed, 5 total",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "vitest (pass)",
            command: "npx vitest run",
            program: "npx",
            raw: VITEST_PASS,
            code: 0,
            min_reduction: Some(60),
            must_contain: &["Test Files  1 passed (1)", "Tests  3 passed (3)"],
            expect_passthrough: false,
        },
        Sample {
            class: "vitest (fail)",
            command: "npx vitest run",
            program: "npx",
            raw: VITEST_FAIL,
            code: 1,
            min_reduction: None,
            must_contain: &[
                "FAIL  sum.test.mjs > sum > adds negatives",
                "AssertionError: expected -5 to be -6 // Object.is equality",
                "sum.test.mjs:6:52",
                "AssertionError: expected 'Hello' to be 'HELLO' // Object.is equality",
                "sum.test.mjs:9:50",
                "Tests  2 failed | 1 passed (3)",
            ],
            expect_passthrough: false,
        },
        Sample {
            class: "npm install (installer log)",
            command: "npm install gulp@4 request@2 bower@1",
            program: "npm",
            raw: NPM_INSTALL,
            code: 0,
            min_reduction: None,
            must_contain: &[
                "added 386 packages, and audited 387 packages in 22s",
                "16 vulnerabilities (10 moderate, 4 high, 2 critical)",
            ],
            // The donor filtered this class with a declarative TOML pipeline;
            // p1 does not take the TOML engine, so it passes through.
            expect_passthrough: true,
        },
        Sample {
            class: "shellcheck (linter)",
            command: "shellcheck /tmp/fixgen/sh/deploy.sh",
            program: "shellcheck",
            raw: SHELLCHECK,
            code: 1,
            min_reduction: None,
            must_contain: &["In /tmp/fixgen/sh/deploy.sh line 5:", "SC2045", "SC2086"],
            // A TOML class in the donor; uncovered here, so raw passthrough.
            expect_passthrough: true,
        },
    ]
}

/// A workspace plus a private `bin` directory of fake programs, one per class.
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
    ///
    /// The captured bytes live in a data file and the program `cat`s it: the
    /// output is data, never code, even when it contains shell syntax.
    fn replay(&self, program: &str, output: &str, code: i32) {
        let data = self.bin.join(format!("{program}.out"));
        std::fs::write(&data, output).unwrap();
        let script = format!("#!/bin/sh\ncat '{}'\nexit {code}\n", data.display());
        let path = self.bin.join(program);
        std::fs::write(&path, script).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    /// Run `command` (the fake programs first on `PATH`, so the tool's `bash -lc`
    /// resolves them) and return the tool's outcome for `raw`.
    ///
    /// `HOME` is the harness workspace: `bash -lc` is a login shell, and a real
    /// home would source the machine's profile into the captured output.
    async fn run(&self, command: &str, raw: bool) -> ToolOutcome {
        let path = format!("{}:{SYSTEM_PATH}", self.bin.display());
        let command = format!("PATH={path} {command}");
        let input = serde_json::json!({ "command": command, "raw": raw }).to_string();
        let call = ToolCall {
            call_id: "filter-call".into(),
            name: "shell".into(),
            input: ToolInput::Json(input),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
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
        tool.execute(&call, context).await
    }
}

/// Replay one sample and return what the model would see.
async fn replay(sample: &Sample, raw: bool) -> ToolOutcome {
    let harness = Harness::new();
    harness.replay(sample.program, sample.raw, sample.code);
    harness.run(sample.command, raw).await
}

fn footer(code: i32) -> String {
    format!("[exit code: {code}]")
}

/// What the model sees with no filter at all: the captured bytes and the footer.
fn raw_content(sample: &Sample) -> String {
    format!(
        "{}\n{}",
        sample.raw.trim_end_matches('\n'),
        footer(sample.code)
    )
}

/// The summarised body behind one model-visible content: the footer, then the
/// marker line, stripped. This is the basis the donor's bars were measured on
/// (captured output vs. filter output).
fn summarised_body(content: &str, code: i32) -> &str {
    let body = content.strip_suffix(&footer(code)).unwrap_or(content);
    let body = body.strip_suffix('\n').unwrap_or(body);
    body.strip_suffix(MARKER).unwrap_or(body)
}

/// The donor's estimator (`chars/4`, as `iris-agent` `est_tokens`).
fn est_tokens(text: &str) -> usize {
    text.len().div_ceil(4)
}

// ------------------------------------------------------------------- the seam

/// Fail-safe: an unrecognised command yields the raw output, byte for byte.
#[tokio::test]
async fn an_unrecognised_command_yields_the_raw_output() {
    let harness = Harness::new();
    harness.replay("flake9", CARGO_TEST_PASS, 0);

    let outcome = harness.run("flake9 check", false).await;

    assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
    assert_eq!(
        outcome.content,
        format!("{}\n[exit code: 0]", CARGO_TEST_PASS.trim_end())
    );
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}

/// Fail-safe: a filter that declines (its output cannot be parsed confidently)
/// yields the raw output, byte for byte. Here the class is `git log --oneline`,
/// which the donor's corpus also expects to pass through.
#[tokio::test]
async fn a_declining_filter_yields_the_raw_output() {
    let oneline = samples()
        .into_iter()
        .find(|sample| sample.class == "git log --oneline")
        .expect("the corpus has the oneline sample");
    let outcome = replay(&oneline, false).await;
    assert_eq!(outcome.content, raw_content(&oneline));
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");

    // Garbage under a recognised command: every structured filter declines.
    for (class, program, command) in [
        ("cargo test", "cargo", "cargo test"),
        ("cargo build", "cargo", "cargo build"),
        ("git status", "git", "git status"),
        ("git log", "git", "git log"),
        ("git diff", "git", "git diff"),
        ("npm test", "npm", "npm test"),
    ] {
        let harness = Harness::new();
        harness.replay(program, "complete garbage output\n", 0);
        let outcome = harness.run(command, false).await;
        assert_eq!(
            outcome.content, "complete garbage output\n[exit code: 0]",
            "{class}: {outcome:?}"
        );
    }
}

/// Fail-safe: a filter that empties non-empty output yields the raw output.
#[tokio::test]
async fn a_filter_that_empties_non_empty_output_yields_the_raw_output() {
    // Advice lines only: the git-status filter strips them all.
    let hints = "  (use \"git add <file>...\" to update what will be committed)\n";
    let harness = Harness::new();
    harness.replay("git", hints, 0);

    let outcome = harness.run("git status", false).await;

    assert_eq!(
        outcome.content,
        format!("{}\n[exit code: 0]", hints.trim_end())
    );
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}

/// Fail-safe: a result that is not SHORTER than its input yields the raw
/// output (here the clean-tree status reduces to itself).
#[tokio::test]
async fn a_result_that_is_not_shorter_than_its_input_yields_the_raw_output() {
    let harness = Harness::new();
    harness.replay("git", "On branch main\n", 0);

    let outcome = harness.run("git status", false).await;

    assert_eq!(outcome.content, "On branch main\n[exit code: 0]");
    assert!(!outcome.content.contains(MARKER), "{outcome:?}");
}

/// Fail-safe: `raw: true` bypasses filtering entirely — the content is
/// byte-identical to the unfiltered run of the same captured bytes.
#[tokio::test]
async fn raw_true_bypasses_filtering_entirely() {
    for sample in samples() {
        let unfiltered = replay(&sample, true).await;

        assert_eq!(
            unfiltered.content,
            raw_content(&sample),
            "[{}] raw: true must be the raw output: {unfiltered:?}",
            sample.class
        );
    }
}

/// Fail-safe: the exit-code footer is appended after filtering and is never
/// touched — it is the last thing in the content, exactly once, whatever the
/// filter did (summarised, raw, or declining).
#[tokio::test]
async fn the_exit_code_footer_is_appended_after_filtering_and_never_touched() {
    for sample in samples() {
        let outcome = replay(&sample, false).await;
        let footer = footer(sample.code);
        assert!(
            outcome.content.ends_with(&format!("\n{footer}")),
            "[{}] lost its footer: {outcome:?}",
            sample.class
        );
        assert_eq!(
            outcome.content.matches(&footer).count(),
            1,
            "[{}] footer must appear once: {outcome:?}",
            sample.class
        );
    }
}

/// A summarised result ends with the marker line, before the footer, and the
/// description states the escape hatch.
#[tokio::test]
async fn a_filtered_result_ends_with_the_marker_line() {
    let sample = samples()
        .into_iter()
        .find(|sample| sample.class == "cargo build (pass)")
        .expect("the corpus has the clean build sample");
    let outcome = replay(&sample, false).await;

    assert_eq!(outcome.content, format!("ok\n{MARKER}\n[exit code: 0]"));

    let workspace = tempfile::tempdir().unwrap();
    let description = ShellTool::new(Workspace::new(workspace.path()).unwrap())
        .declaration()
        .description
        .clone();
    assert_eq!(
        description
            .lines()
            .filter(|line| line.contains("`raw: true`"))
            .count(),
        1,
        "one sentence must say that `raw: true` returns the full log: {description:?}"
    );
}

/// Fail-safe: the head/tail byte bound stays the backstop AFTER the filter.
///
/// Both bodies here stay under the collector's cap (25 KB head + 25 KB tail,
/// nothing dropped), so neither carries an omission marker of its own. The
/// first is 25 KB of chatter plus warnings the filter keeps: filtering first
/// leaves a complete summary that fits, while a bound applied BEFORE the
/// filter would have cut the body and left its own marker inside the summary.
/// The second is one chatter line plus a warning list that is still over the
/// bound after filtering: the bound then cuts the SUMMARISED text, marker and
/// footer intact.
#[tokio::test]
async fn the_byte_bound_stays_the_backstop_after_the_filter() {
    let mut raw = String::new();
    for i in 0..600 {
        raw.push_str(&format!("   Compiling crate_{i:04} v0.1.0 (/w/c{i:04})\n"));
    }
    for i in 0..380 {
        raw.push_str(&format!(
            "warning: noisy diagnostic number {i:03} with some padding text here\n"
        ));
    }
    assert!(raw.len() < 50_000, "raw is {} bytes", raw.len());
    let harness = Harness::new();
    harness.replay("cargo", &raw, 0);

    let outcome = harness.run("cargo build", false).await;

    assert!(outcome.content.contains(MARKER), "{outcome:?}");
    assert!(
        !outcome.content.contains("bytes omitted"),
        "the filter must have run on the whole body before the bound: {outcome:?}"
    );
    assert!(
        outcome
            .content
            .contains("warning: noisy diagnostic number 000"),
        "{outcome:?}"
    );
    assert!(
        outcome
            .content
            .contains("warning: noisy diagnostic number 379"),
        "{outcome:?}"
    );
    assert!(
        outcome
            .content
            .ends_with(&format!("{MARKER}\n[exit code: 0]")),
        "{outcome:?}"
    );

    // A summary that is still too long is bounded: head and tail kept, marker
    // and footer untouched.
    let mut raw = String::from("   Compiling big v0.1.0 (/w/big)\n");
    for i in 0..760 {
        raw.push_str(&format!(
            "warning: noisy diagnostic number {i:03} with some padding text here\n"
        ));
    }
    assert!(raw.len() < 50_000, "raw is {} bytes", raw.len());
    let harness = Harness::new();
    harness.replay("cargo", &raw, 0);

    let outcome = harness.run("cargo build", false).await;

    assert!(outcome.content.contains(MARKER), "{outcome:?}");
    assert!(
        outcome.content.contains("bytes omitted"),
        "a summary over the bound must be cut: {outcome:?}"
    );
    assert!(
        outcome
            .content
            .contains("warning: noisy diagnostic number 000"),
        "{outcome:?}"
    );
    assert!(
        outcome
            .content
            .contains("warning: noisy diagnostic number 759"),
        "{outcome:?}"
    );
    assert!(
        outcome
            .content
            .ends_with(&format!("{MARKER}\n[exit code: 0]")),
        "{outcome:?}"
    );
    assert!(outcome.content.len() < 50_000, "{}", outcome.content.len());
}

// ------------------------------------------------------ the quality contract

/// The donor's corpus bars: noisy classes must hit their minimum reduction,
/// measured through the p1 seam.
#[tokio::test]
async fn the_donor_reduction_bars_hold_on_the_corpus() {
    for sample in samples() {
        let Some(bar) = sample.min_reduction else {
            continue;
        };
        let outcome = replay(&sample, false).await;
        assert!(
            outcome.content.contains(MARKER),
            "[{}] expected the filter to apply: {outcome:?}",
            sample.class
        );
        let before = est_tokens(sample.raw.trim_end_matches('\n'));
        let after = est_tokens(summarised_body(&outcome.content, sample.code));
        let reduction = 100.0 * (1.0 - after as f64 / before as f64);
        assert!(
            reduction >= f64::from(bar),
            "[{}] reduction {reduction:.1}% is below the {bar}% bar",
            sample.class
        );
    }
}

/// Every sample keeps the content the donor pinned: summaries on success,
/// error and failure detail verbatim on failure.
#[tokio::test]
async fn every_sample_keeps_its_content_verbatim() {
    for sample in samples() {
        let outcome = replay(&sample, false).await;
        for needle in sample.must_contain {
            assert!(
                outcome.content.contains(needle),
                "[{}] lost {needle:?} in:\n{}",
                sample.class,
                outcome.content
            );
        }
    }
}

/// Fail-safe: a failing command (non-zero exit) keeps every error and failure
/// line verbatim; only known noise is dropped.
#[tokio::test]
async fn a_failing_command_keeps_every_error_line_verbatim() {
    for sample in samples() {
        if sample.code == 0 {
            continue;
        }
        let outcome = replay(&sample, false).await;
        for needle in sample.must_contain {
            assert!(
                outcome.content.contains(needle),
                "[{}] lost {needle:?} in:\n{}",
                sample.class,
                outcome.content
            );
        }
        // Nothing that looks like an error line may disappear: every such line
        // of the captured output is still in the content.
        for line in sample.raw.lines() {
            let signal = line.starts_with("error")
                || line.starts_with("warning:")
                || line.contains("FAILED")
                || line.contains("panicked at")
                || line.contains("AssertionError")
                || line.contains("FAIL ");
            if signal {
                assert!(
                    outcome.content.contains(line.trim_end()),
                    "[{}] dropped {line:?}",
                    sample.class
                );
            }
        }
    }
}

/// Classes no filter covers pass through untouched, marker included.
#[tokio::test]
async fn uncovered_classes_pass_through_untouched() {
    for sample in samples() {
        if !sample.expect_passthrough {
            continue;
        }
        let outcome = replay(&sample, false).await;
        assert_eq!(
            outcome.content,
            raw_content(&sample),
            "[{}] must pass through untouched",
            sample.class
        );
        assert!(!outcome.content.contains(MARKER), "{outcome:?}");
    }
}
