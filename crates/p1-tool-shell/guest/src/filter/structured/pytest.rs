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

/// The full node id of a verbose `FAILED`/`ERROR` progress line, e.g.
/// `tests/test_a.py::TestA::test_one[param] FAILED [ 50%]`. The compact `.F.`
/// progress form and the passing/`SKIPPED`/`XFAIL` verbose lines return `None`.
fn failed_progress_id(line: &str) -> Option<&str> {
    static RE: OnceLock<Regex> = OnceLock::new();
    let re = RE.get_or_init(|| {
        Regex::new(r"^(?P<id>\S+\.py::.+?) (?:FAILED|ERROR)(?: \([^\r\n]*\))?(?:\s+\[\s*\d+%\])?$")
            .expect("static regex")
    });
    re.captures(line)
        .map(|caps| caps.name("id").expect("id group").as_str())
}

/// True when `text` names `id` as a whole token, so a retained section already
/// carries the failing test's full node id and the progress line is redundant.
fn names_id(text: &str, id: &str) -> bool {
    text.match_indices(id).any(|(start, _)| {
        let before = text[..start].chars().next_back();
        let after = text[start + id.len()..].chars().next();
        before.is_none_or(char::is_whitespace) && after.is_none_or(char::is_whitespace)
    })
}

/// The title of a report in the FAILURES or ERRORS section: `test_x` in
/// `_____ test_x _____`.
fn report_title(trimmed: &str) -> Option<&str> {
    let start = trimmed.find(|c| c != '_')?;
    let end = trimmed.rfind(|c| c != '_')? + 1;
    if start < 2 || trimmed.len() - end < 2 {
        return None;
    }
    trimmed[start..end]
        .strip_prefix(' ')?
        .strip_suffix(' ')
        .filter(|title| !title.is_empty())
}

/// The report titles a short-summary `FAILED`/`ERROR` line may belong to: pytest titles a
/// failure by its node id after the file, `::` as `.` (`TestA.test_m[1]`), a setup or
/// teardown error `ERROR at setup of …`, and a collection error `ERROR collecting <path>`.
fn summary_report_titles(line: &str) -> Vec<String> {
    let (verb, rest) = line.split_once(' ').unwrap_or((line, ""));
    let id = rest.split_once(" - ").map_or(rest, |(id, _)| id).trim();
    let Some((_, test)) = id.split_once("::") else {
        return vec![format!("ERROR collecting {id}")];
    };
    let head = test.replace("::", ".");
    if verb == "FAILED" {
        vec![head]
    } else {
        vec![
            format!("ERROR at setup of {head}"),
            format!("ERROR at teardown of {head}"),
        ]
    }
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
    let mut failing_count = 0u64;
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
            if matches!(outcome, "failed" | "error") {
                failing_count += count;
            }
        }
    }
    if exit_ok && (failed || summary.starts_with("no tests ran")) {
        return None;
    }

    let mut state = ParseState::Header;
    let mut first_diagnostic = None;
    let mut failure_section = false;
    let mut in_short_summary = false;
    let mut in_reports = false;
    let mut failure_progress: Vec<(&str, &str)> = Vec::new();
    let mut named_failures = 0u64;
    // The short summary's FAILED/ERROR lines, and the report titles (`____ test_x ____`)
    // of the FAILURES and ERRORS sections they must match to count.
    let mut summary_lines: Vec<&str> = Vec::new();
    let mut report_titles = std::collections::HashSet::new();
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
                    in_short_summary = title == "short test summary info";
                    in_reports = matches!(title, "FAILURES" | "ERRORS");
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
            if in_reports && let Some(title) = report_title(trimmed) {
                report_titles.insert(title);
            }
            if in_short_summary && (trimmed.starts_with("FAILED ") || trimmed.starts_with("ERROR "))
            {
                summary_lines.push(trimmed);
            }
            continue;
        }
        if trimmed.is_empty() {
            continue;
        }
        if collected_re().is_match(trimmed) || progress_re().is_match(trimmed) {
            state = ParseState::TestProgress;
            if let Some(id) = failed_progress_id(trimmed) {
                failure_progress.push((*line, id));
            }
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
    let retained = lines[start..=last].join("\n");
    // `-rN` disables the short test summary info, the only retained section that
    // names a failing test in full. Keep the verbose FAILED/ERROR progress lines
    // then, unless a retained line already names that id (the default `-r`, where
    // the progress line is redundant). Never drop an id: every failing test in the
    // final count line must be named by the output we keep, either by a short
    // summary `FAILED`/`ERROR` line or by a retained verbose progress line. A
    // partial `-r` set (e.g. `-rE` with a failure and an error) names only some of
    // them, so decline to the raw output rather than dropping the unnamed id.
    // A short-summary line names one failing test in full when its report is above it; a
    // look-alike line inside a whole multi-line message names no report and counts nothing.
    named_failures += summary_lines
        .iter()
        .filter(|line| {
            summary_report_titles(line)
                .iter()
                .any(|title| report_titles.contains(title.as_str()))
        })
        .count() as u64;
    let mut kept: Vec<&str> = Vec::new();
    for (line, id) in failure_progress {
        if !names_id(&retained, id) {
            kept.push(line);
            named_failures += 1;
        }
    }
    // More names than failures means a line was miscounted; fewer means an id is lost.
    if failed && named_failures != failing_count {
        return None;
    }
    if kept.is_empty() {
        return Some(retained);
    }
    let mut filtered = String::new();
    for line in kept {
        filtered.push_str(line);
        filtered.push('\n');
    }
    filtered.push_str(&retained);
    Some(filtered)
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

    #[test]
    fn verbose_r_n_failure_keeps_full_node_id() {
        // `-rN` turns the short test summary off, so the only line carrying the
        // full node id is the verbose progress line.
        let raw = [
            "============================= test session starts ==============================",
            "platform linux -- Python 3.12.3, pytest-8.3.5, pluggy-1.5.0",
            "rootdir: /work/pytest-sample",
            "collected 3 items",
            "",
            "tests/test_math.py::test_one PASSED [ 33%]",
            "tests/test_math.py::TestMath::test_add[param] FAILED [ 66%]",
            "tests/test_math.py::test_three PASSED [100%]",
            "",
            "=================================== FAILURES ===================================",
            "___________________________ TestMath.test_add[param] ___________________________",
            "",
            "    def test_add(self, param):",
            ">       assert param == 3",
            "",
            " tests/test_math.py:21: AssertionError",
            "========================= 1 failed, 2 passed in 0.04s ==========================",
        ]
        .join("\n");
        let filtered = apply(&raw, false).expect("filtered");
        assert!(
            filtered.contains("tests/test_math.py::TestMath::test_add[param] FAILED"),
            "{filtered}"
        );
        assert!(!filtered.contains("test_one"), "{filtered}");
        assert!(!filtered.contains("PASSED"), "{filtered}");
    }

    #[test]
    fn verbose_r_n_error_keeps_full_node_id() {
        let raw = [
            "============================= test session starts ==============================",
            "platform linux -- Python 3.12.3, pytest-8.3.5, pluggy-1.5.0",
            "collected 3 items",
            "",
            "tests/test_db.py::test_database ERROR [ 50%]",
            "tests/test_db.py::test_ok PASSED [100%]",
            "",
            "==================================== ERRORS ====================================",
            "________________________ ERROR at setup of test_database ________________________",
            "",
            "    @pytest.fixture",
            "    def database():",
            ">       raise RuntimeError(\"database unavailable\")",
            "E       RuntimeError: database unavailable",
            "",
            " tests/conftest.py:8: RuntimeError",
            "========================== 2 passed, 1 error in 0.05s ==========================",
        ]
        .join("\n");
        let filtered = apply(&raw, false).expect("filtered");
        assert!(
            filtered.contains("tests/test_db.py::test_database ERROR"),
            "{filtered}"
        );
        assert!(!filtered.contains("test_ok"), "{filtered}");
    }

    #[test]
    fn verbose_with_short_summary_stays_unchanged() {
        // The default `-r` short summary already names the failing id, so the
        // progress line stays dropped: output is exactly the diagnostic tail.
        let raw = [
            "============================= test session starts ==============================",
            "platform linux -- Python 3.12.3, pytest-8.3.5, pluggy-1.5.0",
            "collected 3 items",
            "",
            "tests/test_math.py::test_one PASSED [ 33%]",
            "tests/test_math.py::TestMath::test_add[param] FAILED [ 66%]",
            "tests/test_math.py::test_three PASSED [100%]",
            "",
            "=================================== FAILURES ===================================",
            "___________________________ TestMath.test_add[param] ___________________________",
            "",
            "    def test_add(self, param):",
            ">       assert param == 3",
            "",
            " tests/test_math.py:21: AssertionError",
            "=========================== short test summary info ============================",
            "FAILED tests/test_math.py::TestMath::test_add[param] - assert 1 == 2",
            "========================= 1 failed, 2 passed in 0.04s ==========================",
        ]
        .join("\n");
        let expected = [
            "=================================== FAILURES ===================================",
            "___________________________ TestMath.test_add[param] ___________________________",
            "",
            "    def test_add(self, param):",
            ">       assert param == 3",
            "",
            " tests/test_math.py:21: AssertionError",
            "=========================== short test summary info ============================",
            "FAILED tests/test_math.py::TestMath::test_add[param] - assert 1 == 2",
            "========================= 1 failed, 2 passed in 0.04s ==========================",
        ]
        .join("\n");
        let filtered = apply(&raw, false).expect("filtered");
        assert_eq!(filtered, expected);
        assert_eq!(filtered.matches("FAILED").count(), 1, "{filtered}");
    }

    #[test]
    fn non_verbose_r_n_failure_declines() {
        // Without the short summary the compact `.F.` progress form carries no
        // full id, so no retained line can name the failure: decline, never drop.
        let raw = [
            "============================= test session starts ==============================",
            "platform linux -- Python 3.12.3, pytest-8.3.5, pluggy-1.5.0",
            "collected 3 items",
            "",
            "tests/test_math.py .F. [100%]",
            "",
            "=================================== FAILURES ===================================",
            "_________________________________ test_add _____________________________________",
            "",
            "    def test_add():",
            ">       assert add(1, 1) == 3",
            "E       assert 2 == 3",
            "",
            " tests/test_math.py:12: AssertionError",
            "========================= 1 failed, 2 passed in 0.04s ==========================",
        ]
        .join("\n");
        assert_eq!(apply(&raw, false), None);
    }

    /// A non-verbose report with an error, then a failure, and the given short summary
    /// lines and final count line (#596).
    fn errors_then_failures(summary: &[&str], counts: &str) -> String {
        let mut lines = vec![
            "============================= test session starts ==============================",
            "platform linux -- Python 3.12.3, pytest-8.3.5, pluggy-1.5.0",
            "collected 3 items",
            "",
            "tests/test_math.py .FE [100%]",
            "",
            "==================================== ERRORS ====================================",
            "_______________________ ERROR at setup of test_div _____________________________",
            "",
            "    @pytest.fixture",
            "    def zero():",
            ">       raise ValueError(\"no zero\")",
            "E       ValueError: no zero",
            "",
            "tests/test_math.py:5: ValueError",
            "=================================== FAILURES ===================================",
            "_________________________________ test_add _____________________________________",
            "",
            "    def test_add():",
            ">       assert add(1, 1) == 3",
            "E       assert 2 == 3",
            "",
            "tests/test_math.py:12: AssertionError",
            "=========================== short test summary info ============================",
        ];
        lines.extend_from_slice(summary);
        lines.push(counts);
        lines.join("\n")
    }

    #[test]
    fn non_verbose_r_e_never_drops_the_failure_id() {
        let raw = errors_then_failures(
            &["ERROR tests/test_math.py::test_div - ValueError: no zero"],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        );
        let out = apply(&raw, false);
        assert!(
            out.as_deref()
                .is_none_or(|s| s.contains("tests/test_math.py::test_add")),
            "failure id dropped: {out:?}"
        );
    }

    #[test]
    fn default_r_with_errors_first_still_filters() {
        let raw = errors_then_failures(
            &[
                "FAILED tests/test_math.py::test_add - assert 2 == 3",
                "ERROR tests/test_math.py::test_div - ValueError: no zero",
            ],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        );
        let out = apply(&raw, false).expect("default -r report must still be filtered");
        assert!(out.contains("tests/test_math.py::test_add"), "{out}");
        assert!(out.contains("tests/test_math.py::test_div"), "{out}");
    }

    /// #596: `-rE` names only the error, and the compact `.FE` progress line carries no
    /// id, so the failure's node id could only be dropped: the filter declines.
    #[test]
    fn non_verbose_partial_r_set_with_failure_and_error_declines() {
        let raw = errors_then_failures(
            &["ERROR tests/test_math.py::test_div - ValueError: no zero"],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        );
        assert_eq!(apply(&raw, false), None);
    }

    /// The default `-r` names both, so every id survives and the progress line goes.
    #[test]
    fn non_verbose_default_r_with_failure_and_error_is_kept() {
        let raw = errors_then_failures(
            &[
                "FAILED tests/test_math.py::test_add - assert 2 == 3",
                "ERROR tests/test_math.py::test_div - ValueError: no zero",
            ],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        );
        let filtered = apply(&raw, false).expect("filtered");
        assert!(filtered.starts_with("========"), "{filtered}");
        assert!(!filtered.contains(".FE"), "{filtered}");
    }

    /// `-v -rE`: the summary names the error and the kept verbose progress line names the
    /// failure, so both ids survive.
    #[test]
    fn verbose_r_e_keeps_the_failure_progress_line() {
        let raw = errors_then_failures(
            &["ERROR tests/test_math.py::test_div - ValueError: no zero"],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        )
        .replace(
            "tests/test_math.py .FE [100%]",
            "tests/test_math.py::test_one PASSED [ 33%]\ntests/test_math.py::test_add FAILED [ 66%]\ntests/test_math.py::test_div ERROR [100%]",
        );
        let filtered = apply(&raw, false).expect("filtered");
        assert!(
            filtered.starts_with("tests/test_math.py::test_add FAILED [ 66%]\n"),
            "{filtered}"
        );
        assert!(!filtered.contains("test_one PASSED"), "{filtered}");
        assert!(
            filtered.contains("ERROR tests/test_math.py::test_div"),
            "{filtered}"
        );
    }

    /// On CI pytest prints a message whole, so a continuation line can look like a summary
    /// line for another test. It names no report above, so it counts nothing and the
    /// failure it would have hidden makes the filter decline.
    #[test]
    fn a_multi_line_summary_message_declines() {
        let raw = errors_then_failures(
            &[
                "ERROR tests/test_math.py::test_div - AssertionError: assert 'x' in 'collected 1 item",
                "FAILED inner/test_inner.py::test_inner - boom'",
            ],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        );
        assert_eq!(apply(&raw, false), None);
        // A whole message without such a line still filters: both ids are named.
        let raw = errors_then_failures(
            &[
                "ERROR tests/test_math.py::test_div - ValueError: first line",
                "  second line of the message",
                "FAILED tests/test_math.py::test_add - assert 2 == 3",
            ],
            "=================== 1 failed, 1 passed, 1 error in 0.04s ===================",
        );
        assert!(apply(&raw, false).is_some());
    }

    /// Summary lines map to the titles pytest gives their reports.
    #[test]
    fn summary_lines_map_to_report_titles() {
        assert_eq!(
            summary_report_titles("FAILED t.py::TestA::test_m[1-2] - assert 0"),
            vec!["TestA.test_m[1-2]".to_string()]
        );
        assert_eq!(
            summary_report_titles("ERROR t.py::test_db - RuntimeError: down"),
            vec![
                "ERROR at setup of test_db".to_string(),
                "ERROR at teardown of test_db".to_string()
            ]
        );
        assert_eq!(
            summary_report_titles("ERROR tests/test_x.py - ImportError"),
            vec!["ERROR collecting tests/test_x.py".to_string()]
        );
        assert_eq!(report_title("_____ test_x_ _____"), Some("test_x_"));
        assert_eq!(
            report_title("____ ERROR at setup of test_db ____"),
            Some("ERROR at setup of test_db")
        );
        assert_eq!(report_title("_test_x_"), None);
        assert_eq!(report_title("tests/test_math.py:12: AssertionError"), None);
    }
}
