//! Structured shell-output filters (research #42, donor `iris-agent`
//! `src/tools/bash/filter/`, structured half only).
//!
//! One seam: [`filter_output`] summarises the captured output of a RECOGNISED
//! command. It runs after the command exited and before the head/tail byte
//! bound (`ShellTool::render`). Filters are pure `fn(&str, bool) ->
//! Option<String>` functions keyed on the effective command
//! ([`command::effective_command`]); dispatch lives in [`structured`]. There
//! is no registry, no TOML engine and no provider or UI type: a filter that
//! declines simply yields the raw output.
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
//! - the head/tail byte bound stays the backstop after the filter.

mod command;
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
    let filter = structured::find(&effective)?;
    let filtered = apply_guarded(filter.apply, output, exit_ok)?;
    // Empty-guard: a filter must never swallow non-empty output, and a
    // never-worse guard: a result the same size or larger is not a summary.
    if filtered.trim().is_empty() || filtered.len() >= output.trim_end_matches('\n').len() {
        return None;
    }
    Some(filtered)
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

    #[test]
    fn ansi_escapes_are_stripped() {
        assert_eq!(strip_ansi("\x1b[31mred\x1b[0m"), "red");
        assert_eq!(strip_ansi("plain"), "plain");
    }
}
