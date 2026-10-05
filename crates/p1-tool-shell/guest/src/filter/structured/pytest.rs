//! Pure pytest terminal-reporter parser, adapted from RTK's
//! `src/cmds/python/pytest_cmd.rs` at 6d4b77e (Apache-2.0).
//! Unlike the donor, never cap, truncate or reindent diagnostic sections,
//! infer an empty run from missing counts, or change the executed command.

use std::sync::OnceLock;

use regex::Regex;

#[derive(Clone, Copy)]
enum ParseState {
    Header,
    TestProgress,
    Diagnostics,
}

fn summary_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^(?:no tests ran|\d+ (?:passed|failed|skipped|deselected|xfailed|xpassed|errors?|warnings?)",
            r"(?:, \d+ (?:passed|failed|skipped|deselected|xfailed|xpassed|errors?|warnings?))*)",
            r" in \d+\.\d+s(?: \(\d+:\d\d(?::\d\d)?\))?$",
        ))
        .expect("static regex")
    })
}

fn progress_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(concat!(
            r"^(?:(?:\S+\.py [\.FsxXE]+|[\.FsxXE]+)",
            r"(?:\s+\[\s*\d+%\])?",
            r"|\S+\.py::.+ (?:PASSED|FAILED|ERROR|SKIPPED|XFAIL|XPASS)",
            r"(?: \([^\r\n]*\))?(?:\s+\[\s*\d+%\])?)$",
        ))
        .expect("static regex")
    })
}

fn collected_re() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"^collected \d+ items?(?: / \d+ (?:errors?|deselected|selected))*$")
            .expect("static regex")
    })
}

fn section(line: &str) -> Option<&str> {
    let trimmed = line.trim();
    if trimmed.starts_with("===") && trimmed.ends_with("===") {
        Some(trimmed.trim_matches('=').trim())
    } else {
        None
    }
}

pub(super) fn apply(output: &str, exit_ok: bool) -> Option<String> {
    // Preserve diagnosis bytes exactly. Colored/custom reporters are outside
    // this grammar and decline rather than stripping their formatting.
    if output.contains('\u{1b}') || output.contains("KeyboardInterrupt") {
        return None;
    }
    let lines: Vec<&str> = output.lines().collect();
    let last = lines.iter().rposition(|line| !line.trim().is_empty())?;
    let summary = section(lines[last]).unwrap_or(lines[last].trim());
    if !summary_re().is_match(summary) {
        return None;
    }
    let mut outcomes = std::collections::HashSet::new();
    let mut failed = false;
    if !summary.starts_with("no tests ran") {
        let (counts, _) = summary.rsplit_once(" in ")?;
        for part in counts.split(", ") {
            let (count, outcome) = part.split_once(' ')?;
            let count = count.parse::<u64>().ok()?;
            let outcome = match outcome {
                "errors" => "error",
                "warnings" => "warning",
                value => value,
            };
            if !outcomes.insert(outcome) {
                return None;
            }
            failed |= count > 0 && matches!(outcome, "failed" | "error");
        }
    }
    if exit_ok && (failed || summary.starts_with("no tests ran")) {
        return None;
    }

    let mut state = ParseState::Header;
    let mut first_diagnostic = None;
    let mut failure_section = false;
    for (index, line) in lines[..last].iter().enumerate() {
        let trimmed = line.trim();
        if trimmed.starts_with("plugins:") || trimmed.starts_with("===") && section(line).is_none()
        {
            return None;
        }
        if let Some(title) = section(line) {
            match title {
                "test session starts" if matches!(state, ParseState::Header) => continue,
                "FAILURES"
                | "ERRORS"
                | "PASSES"
                | "XFAILURES"
                | "XPASSES"
                | "short test summary info"
                | "warnings summary" => {
                    failure_section |= matches!(title, "FAILURES" | "ERRORS");
                    first_diagnostic.get_or_insert(index);
                    state = ParseState::Diagnostics;
                    continue;
                }
                _ => return None,
            }
        }
        // Everything inside recognized sections is retained verbatim, including
        // captured stdout/stderr, chained tracebacks, xfail reasons and warnings.
        if matches!(state, ParseState::Diagnostics) {
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if collected_re().is_match(trimmed) || progress_re().is_match(trimmed) {
            state = ParseState::TestProgress;
        } else if matches!(state, ParseState::Header)
            && (trimmed.starts_with("platform ") && trimmed.contains(", pytest-")
                || ["rootdir: ", "configfile: ", "testpaths: ", "cachedir: "]
                    .iter()
                    .any(|prefix| trimmed.starts_with(prefix)))
        {
            continue;
        } else {
            // Unknown header, mixed process output or custom reporter: don't
            // guess whether it is noise, even when counts look familiar.
            return None;
        }
    }
    if failed && !failure_section {
        return None;
    }
    if !exit_ok && !failed && !summary.starts_with("no tests ran") {
        return None;
    }
    let start = first_diagnostic.unwrap_or(last);
    Some(lines[start..=last].join("\n"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn malformed_or_incomplete_reports_decline() {
        for raw in [
            "3 passed in soon",
            "3 passed in 0.01s\nplugin result: lost diagnostic",
            "1 failed in 0.01s",
            "3 passed, 4 passed in 0.01s",
            "=== custom report ===\n3 passed in 0.01s",
            "stderr from separate process\n3 passed in 0.01s",
        ] {
            assert_eq!(apply(raw, false), None, "{raw}");
        }
        assert_eq!(apply("3 passed in 0.01s", false), None);
        assert_eq!(apply("no tests ran in 0.01s", true), None);
    }

    #[test]
    fn quiet_long_duration_and_expected_outcomes_keep_counts() {
        let raw = "..xX [100%]\n2 passed, 1 xfailed, 1 xpassed in 65.12s (0:01:05)";
        assert_eq!(
            apply(raw, true).as_deref(),
            Some("2 passed, 1 xfailed, 1 xpassed in 65.12s (0:01:05)")
        );
    }
}
