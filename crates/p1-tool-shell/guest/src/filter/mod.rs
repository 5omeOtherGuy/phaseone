//! Shell-output filters (research #42, donor `iris-agent`
//! `src/tools/bash/filter/`): the structured Rust half and the vendored
//! declarative half.
//!
//! One seam: [`filter_output`] summarises the captured output of a RECOGNISED
//! command. It runs after the command exited and before the head/tail byte
//! bound (`ShellTool::render`). Filters are pure `fn(&str, bool) ->
//! Option<String>` functions keyed on the effective command
//! ([`command::effective_command`]); dispatch lives in [`structured`]. A second,
//! declarative tier sits behind it: the donor's TOML pipelines, converted once
//! to `data/*.json` (the guest's dependencies stay serde, serde_json and regex:
//! ADR-0081), embedded with `include_str!` (the guest is WebAssembly and has no
//! filesystem) and applied by the ported engine ([`engine`]) through the
//! registry in [`data`]. A declarative filter applies only when no structured
//! filter matched. There is no provider or UI type: a filter
//! that declines simply yields the raw output.
//!
//! Fail-safe contract (`docs/design/tools.md` §"`shell` output filters"), every
//! line of which is a test:
//! - an unrecognised command, a filter that declines, errors or panics, a
//!   result that is not SHORTER than its input, and a filter that empties
//!   non-empty output all yield the RAW output (a pure filter's only error is
//!   its `None` decline, so "errors" and "declines" are one case here);
//! - a failing command's error and failure lines survive verbatim (that is
//!   the filters' own contract, asserted per filter);
//! - the `[exit code: <n>]` / timeout footer is the caller's business and is
//!   never touched here;
//! - `raw: true` never reaches this module;
//! - the head/tail byte bound stays the backstop after the filter;
//! - a capture-loss marker in the input (bytes the collector dropped) survives
//!   every summary, so the model still learns that the log is incomplete.

mod command;
mod data;
mod engine;
mod structured;

use std::sync::OnceLock;

use regex::Regex;

/// Summarise `output` (the captured text of `command`, without the footer).
/// `None` whenever the raw output must be used instead: an unrecognised
/// command, a filter that declined or panicked, a result that is not shorter
/// than its input, or one that empties non-empty output. `exit_ok` must be
/// true only when the command exited 0.
pub(super) fn filter_output(command: &str, output: &str, exit_ok: bool) -> Option<String> {
    if output.trim().is_empty() {
        return None;
    }
    let effective = command::effective_command(command)?;
    // Structured first: when one matched, it owns the command class. A decline
    // (None), a panic or a guard hit means raw passthrough, never a
    // declarative fallback. Only a command no structured filter covers reaches
    // the declarative registry.
    let filtered = match structured::find(&effective) {
        Some(filter) => apply_guarded(filter.apply, output, exit_ok)?,
        None => {
            // A file with `on_empty` renders its message here; a pipeline that
            // empties output without one yields "", which the guard below
            // turns into raw output.
            let filter = data::registry().iter().find(|f| f.matches(&effective))?;
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                engine::apply_filter(filter, output, exit_ok)
            }))
            .ok()?
        }
    };
    // Empty-guard: a filter must never swallow non-empty output, and a
    // never-worse guard: a result the same size or larger is not a summary.
    if filtered.trim().is_empty() {
        return None;
    }
    let filtered = keep_capture_loss_markers(output, filtered);
    if filtered.len() >= output.trim_end_matches('\n').len() {
        return None;
    }
    Some(filtered)
}

/// The collector's capture-loss marker (`p1-module-runtime` process capture):
/// middle bytes were dropped before any filter ran.
const CAPTURE_LOSS: &str = "diagnostics may be missing, including in raw output";

/// Re-append every capture-loss marker line of `output` that `filtered` lost: a
/// short-circuit or `on_empty` message, a line cap or a structured summary must
/// not hide that even the raw log is incomplete.
fn keep_capture_loss_markers(output: &str, mut filtered: String) -> String {
    for line in output.lines().filter(|l| l.contains(CAPTURE_LOSS)) {
        if !filtered.contains(line) {
            filtered.push('\n');
            filtered.push_str(line);
        }
    }
    filtered
}

/// Run one pure filter with panic containment: a panicking filter yields the
/// raw output instead of poisoning the tool call.
fn apply_guarded(
    apply: fn(&str, bool) -> Option<String>,
    output: &str,
    exit_ok: bool,
) -> Option<String> {
    let filtered =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| apply(output, exit_ok)));
    filtered.ok().flatten()
}

fn ansi_re() -> &'static Regex {
    static ANSI: OnceLock<Regex> = OnceLock::new();
    ANSI.get_or_init(|| Regex::new(r"\x1b\[[0-9;]*[a-zA-Z]").expect("ANSI pattern is static"))
}

/// Strip ANSI escape sequences (colors, styles) from `text`.
pub(super) fn strip_ansi(text: &str) -> String {
    ansi_re().replace_all(text, "").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_recognised_command_is_summarised() {
        let raw = "   Compiling foo v0.1.0 (/w/foo)\n    Finished `dev` profile \
                   [unoptimized + debuginfo] target(s) in 1.0s\n";
        assert_eq!(
            filter_output("cargo build", raw, true).as_deref(),
            Some("ok")
        );
    }

    #[test]
    fn an_unrecognised_command_yields_raw() {
        assert!(filter_output("some-unknown-tool --flag", "a\n\nb\n", true).is_none());
    }

    #[test]
    fn empty_output_yields_raw() {
        assert!(filter_output("git status", "   \n", true).is_none());
    }

    #[test]
    fn dispatch_looks_through_a_cd_prefix_and_an_assignment() {
        let raw = "   Compiling foo v0.1.0 (/w/foo)\n";
        assert!(filter_output("cd /some/dir && cargo build", raw, true).is_some());
        assert!(filter_output("CARGO_TERM_COLOR=never cargo build", raw, true).is_some());
        assert!(filter_output("env FOO=1 time cargo build", raw, true).is_some());
    }

    #[test]
    fn a_declining_filter_yields_raw() {
        // Garbage under a recognised command is not that command's output:
        // decline, never guess.
        for command in [
            "cargo test",
            "cargo build",
            "git status",
            "git log",
            "git diff",
            "npm test",
        ] {
            assert!(
                filter_output(command, "complete garbage output\n", true).is_none(),
                "{command}"
            );
        }
    }

    #[test]
    fn a_filter_that_empties_nonempty_output_yields_raw() {
        // The git-status filter strips hint lines; an output of hint lines
        // only would be emptied entirely, so the guard must return raw.
        let hints = "  (use \"git add <file>...\" to update what will be committed)\n";
        assert!(filter_output("git status", hints, true).is_none());
    }

    #[test]
    fn a_result_that_is_not_shorter_than_its_input_yields_raw() {
        // A clean tree with no tracking note reduces to itself: not shorter,
        // so the raw output is used (and no misleading marker is added).
        assert!(filter_output("git status", "On branch main\n", true).is_none());
    }

    #[test]
    fn a_panicking_filter_yields_raw() {
        fn boom(_: &str, _: bool) -> Option<String> {
            panic!("boom")
        }
        assert_eq!(apply_guarded(boom, "text", true), None);
        assert_eq!(
            apply_guarded(|_, _| Some("ok".to_string()), "text", true),
            Some("ok".to_string())
        );
    }

    /// A `helm` log: klog `W....` warnings and blank lines are chatter.
    const HELM: &str = "W0930 10:00:00.000000 1 warnings.go:70] deprecated API\n\nNAME: app\n\
                        STATUS: deployed\n\nREVISION: 3\n";

    #[test]
    fn a_declarative_filter_applies_when_no_structured_filter_matched() {
        // `helm` is a vendored declarative class; the structured tier has no
        // filter for it, so the data pipeline runs.
        assert_eq!(
            filter_output("helm upgrade app ./chart", HELM, true).as_deref(),
            Some("NAME: app\nSTATUS: deployed\nREVISION: 3")
        );
    }

    #[test]
    fn a_declarative_filter_dispatches_through_shell_plumbing() {
        for command in [
            "cd /some/dir && helm upgrade app ./chart",
            "KUBECONFIG=/k helm upgrade app ./chart",
            "sudo helm upgrade app ./chart",
        ] {
            assert!(
                filter_output(command, HELM, true).is_some(),
                "{command} must reach the vendored filter"
            );
        }
    }

    #[test]
    fn structured_filters_keep_precedence_over_declarative_filters() {
        // `cargo test` is a structured class: its summary is the structured
        // filter's, and a structured decline does not fall through to the
        // declarative registry.
        let raw = "     Running unittests src/lib.rs (target/debug/deps/foo-9c1b2a)\n\nrunning 2 \
                    tests\ntest tests::adds ... ok\ntest tests::sub ... ok\n\ntest result: ok. 2 \
                    passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s\n";
        assert_eq!(
            filter_output("cargo test", raw, true).as_deref(),
            Some(
                "unittests src/lib.rs (foo): ok. 2 passed in 0.01s\ncargo test: ok. 2 passed (1 suite in 0.01s)"
            )
        );
        assert!(filter_output("cargo test", "complete garbage output\n", true).is_none());
    }

    #[test]
    fn a_declarative_filter_emptying_output_yields_raw_or_on_empty() {
        // make's vendored filter strips sub-make directory chatter and defines
        // `on_empty`: on success that message is the summary, on a failed run
        // it is suppressed and the raw output stands.
        let chatter = "make[1]: Entering directory '/w/sub'\nmake[1]: Leaving directory '/w/sub'\n";
        assert_eq!(
            filter_output("make all", chatter, true).as_deref(),
            Some("make: ok")
        );
        assert!(filter_output("make all", chatter, false).is_none());
        // helm's filter has no `on_empty`: warnings only are emptied, so the
        // raw output stands.
        let warnings = "W0930 10:00:00.000000 1 warnings.go:70] deprecated API\n";
        assert!(filter_output("helm upgrade app ./chart", warnings, true).is_none());
    }

    #[test]
    fn a_failing_recursive_make_keeps_its_error_lines() {
        let raw = "make[1]: Entering directory '/w/sub'\ngcc foo.c\nfoo.c:1:1: error: boom\n\
                   make[1]: *** [Makefile:10: all] Error 2\nmake[1]: Leaving directory '/w/sub'\n\
                   make: *** [Makefile:5: subdir] Error 2\n";
        assert_eq!(
            filter_output("make", raw, false).as_deref(),
            Some(
                "gcc foo.c\nfoo.c:1:1: error: boom\nmake[1]: *** [Makefile:10: all] Error 2\n\
                 make: *** [Makefile:5: subdir] Error 2"
            )
        );
    }

    #[test]
    fn a_capture_loss_marker_survives_every_summary() {
        let marker =
            "[… 1234 bytes omitted; diagnostics may be missing, including in raw output …]";
        // A declarative short-circuit message.
        let uv =
            format!("Resolved 42 packages in 123ms\n{marker}\nAudited 42 packages in 0.05ms\n");
        assert_eq!(
            filter_output("uv sync", &uv, true),
            Some(format!("ok (up to date)\n{marker}"))
        );
        // A declarative line cap that cuts the marker off.
        let mut make = String::new();
        for i in 0..55 {
            make.push_str(&format!("gcc -c f{i}.c\n"));
        }
        make.push_str(&format!("{marker}\n"));
        for i in 55..60 {
            make.push_str(&format!("gcc -c f{i}.c\n"));
        }
        let filtered = filter_output("make", &make, true).expect("make is summarised");
        assert!(filtered.contains("lines truncated"), "{filtered}");
        assert!(filtered.ends_with(&format!("\n{marker}")), "{filtered}");
    }

    #[test]
    fn ansi_escapes_are_stripped() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
