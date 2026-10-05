//! Authored, sanitized terminal-reporter examples, not live pytest captures.
//! Shapes follow pytest's How to manage output / capture / warnings / skipping docs.
//! Tests replay the public guest renderer, including recovery notices, without running pytest.
use p1_shell_guest::{Capture, End, StoredOutput, finished_with_store};

const MARKER: &str = "[output filtered; pass raw:true for the full log]";

struct Sample {
    name: &'static str,
    raw: &'static str,
    code: i32,
    filtered: bool,
    command: &'static str,
}

fn check(sample: &Sample) -> (usize, usize, usize) {
    let stored = StoredOutput {
        handle: "out-515-corpus".into(),
        stored_bytes: sample.raw.len() as u64,
        capture: Capture::Complete,
    };
    let outcome = finished_with_store(
        sample.raw.as_bytes(),
        End::Exited(sample.code),
        sample.command,
        120,
        false,
        Some(&stored),
    );
    let footer = format!("\n[exit code: {}]", sample.code);
    let emitted = outcome
        .content
        .strip_suffix(&footer)
        .expect("exit footer intact");
    let raw = sample.raw.trim_end_matches('\n');
    assert_eq!(
        emitted.contains(MARKER),
        sample.filtered,
        "{}: {emitted}",
        sample.name
    );
    if sample.filtered {
        assert!(
            emitted.len() < raw.len(),
            "{}: notices must count",
            sample.name
        );
        assert!(emitted.contains("[stored output: handle_id out-515-corpus"));
    } else {
        assert_eq!(emitted, raw, "{}: exact raw fallback", sample.name);
    }
    let mut needles = 0;
    for line in raw.lines() {
        let t = line.trim_start();
        if t.starts_with("E ")
            || t.starts_with("> ")
            || [
                "FAILED ", "ERROR ", "XFAIL ", "XPASS ", "SKIPPED ", "PASSED ",
            ]
            .iter()
            .any(|prefix| t.starts_with(prefix))
        {
            needles += 1;
            assert!(emitted.contains(line), "{}: lost {line:?}", sample.name);
        }
    }
    // Preserve entire diagnostic tail verbatim, not merely sampled assertions.
    if let Some(start) = raw.lines().find(|line| {
        line.starts_with("===")
            && [
                "FAILURES",
                "ERRORS",
                "PASSES",
                "short test summary",
                "warnings summary",
            ]
            .iter()
            .any(|kind| line.contains(kind))
    }) {
        let tail = &raw[raw.find(start).unwrap()..];
        assert!(
            emitted.contains(tail),
            "{}: changed diagnostic tail",
            sample.name
        );
    }
    let unfiltered = finished_with_store(
        sample.raw.as_bytes(),
        End::Exited(sample.code),
        sample.command,
        120,
        true,
        Some(&stored),
    );
    assert_eq!(
        unfiltered.content,
        format!("{raw}{footer}"),
        "raw:true bypass"
    );
    (raw.len(), emitted.len(), needles)
}

macro_rules! fixture {
    ($name:ident, $file:literal, $code:expr, $filtered:expr, $command:literal) => {
        #[test]
        fn $name() {
            check(&Sample {
                name: $file,
                raw: include_str!(concat!("fixtures/pytest/", $file, ".txt")),
                code: $code,
                filtered: $filtered,
                command: $command,
            });
        }
    };
}
fixture!(pass, "pass", 0, true, "pytest");
fixture!(fail, "fail", 1, true, "pytest");
fixture!(error, "error", 1, true, "pytest");
fixture!(xfail, "xfail", 0, true, "pytest");
fixture!(xpass, "xpass", 0, true, "pytest");
fixture!(no_tests, "no-tests", 5, true, "pytest");
fixture!(quiet, "quiet", 0, false, "python -m pytest -q");
fixture!(verbose, "verbose", 0, true, "py.test -v");
fixture!(report_all, "report-all", 0, true, "pytest -rA");
fixture!(custom_plugin, "custom-plugin", 0, false, "pytest");
fixture!(mixed_capture, "mixed-capture", 1, true, "pytest");
fixture!(parametrized, "parametrized", 1, true, "python3 -m pytest");
fixture!(collection_error, "collection-error", 2, true, "pytest");
fixture!(keyboard_interrupt, "keyboard-interrupt", 2, false, "pytest");
fixture!(warnings, "warnings", 0, true, "pytest");
fixture!(multiple_failures, "multiple-failures", 1, true, "pytest");
fixture!(strict_xpass, "strict-xpass", 1, true, "pytest");
fixture!(skipped, "skipped", 0, true, "pytest");
fixture!(malformed, "malformed", 0, false, "pytest");
fixture!(unknown_format, "unknown-format", 0, false, "pytest");

#[test]
fn measured_corpus_bytes() {
    let samples = [
        Sample {
            name: "pass",
            raw: include_str!("fixtures/pytest/pass.txt"),
            code: 0,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "fail",
            raw: include_str!("fixtures/pytest/fail.txt"),
            code: 1,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "error",
            raw: include_str!("fixtures/pytest/error.txt"),
            code: 1,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "xfail",
            raw: include_str!("fixtures/pytest/xfail.txt"),
            code: 0,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "xpass",
            raw: include_str!("fixtures/pytest/xpass.txt"),
            code: 0,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "no-tests",
            raw: include_str!("fixtures/pytest/no-tests.txt"),
            code: 5,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "quiet",
            raw: include_str!("fixtures/pytest/quiet.txt"),
            code: 0,
            filtered: false,
            command: "python -m pytest -q",
        },
        Sample {
            name: "verbose",
            raw: include_str!("fixtures/pytest/verbose.txt"),
            code: 0,
            filtered: true,
            command: "py.test -v",
        },
        Sample {
            name: "report-all",
            raw: include_str!("fixtures/pytest/report-all.txt"),
            code: 0,
            filtered: true,
            command: "pytest -rA",
        },
        Sample {
            name: "custom-plugin",
            raw: include_str!("fixtures/pytest/custom-plugin.txt"),
            code: 0,
            filtered: false,
            command: "pytest",
        },
        Sample {
            name: "mixed-capture",
            raw: include_str!("fixtures/pytest/mixed-capture.txt"),
            code: 1,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "parametrized",
            raw: include_str!("fixtures/pytest/parametrized.txt"),
            code: 1,
            filtered: true,
            command: "python3 -m pytest",
        },
        Sample {
            name: "collection-error",
            raw: include_str!("fixtures/pytest/collection-error.txt"),
            code: 2,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "keyboard-interrupt",
            raw: include_str!("fixtures/pytest/keyboard-interrupt.txt"),
            code: 2,
            filtered: false,
            command: "pytest",
        },
        Sample {
            name: "warnings",
            raw: include_str!("fixtures/pytest/warnings.txt"),
            code: 0,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "multiple-failures",
            raw: include_str!("fixtures/pytest/multiple-failures.txt"),
            code: 1,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "strict-xpass",
            raw: include_str!("fixtures/pytest/strict-xpass.txt"),
            code: 1,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "skipped",
            raw: include_str!("fixtures/pytest/skipped.txt"),
            code: 0,
            filtered: true,
            command: "pytest",
        },
        Sample {
            name: "malformed",
            raw: include_str!("fixtures/pytest/malformed.txt"),
            code: 0,
            filtered: false,
            command: "pytest",
        },
        Sample {
            name: "unknown-format",
            raw: include_str!("fixtures/pytest/unknown-format.txt"),
            code: 0,
            filtered: false,
            command: "pytest",
        },
    ];
    let (mut before, mut after, mut needles) = (0, 0, 0);
    for sample in samples {
        let (raw, emitted, checked) = check(&sample);
        println!("BYTES {} {raw} {emitted} NEEDLES {checked}", sample.name);
        before += raw;
        after += emitted;
        needles += checked;
    }
    println!("TOTAL {before} {after} NEEDLES {needles}");
}

#[test]
fn effective_pytest_commands_dispatch_conservatively() {
    let raw = include_str!("fixtures/pytest/pass.txt");
    for command in [
        "pytest",
        "py.test -v",
        "python -m pytest",
        "python3 -m pytest -q",
        "cd /work && pytest",
        "MODE=test pytest",
    ] {
        let outcome =
            finished_with_store(raw.as_bytes(), End::Exited(0), command, 120, false, None);
        assert!(outcome.content.contains(MARKER), "{command}: {outcome:?}");
    }
    for command in [
        "pytest-custom",
        "python -m pytest_custom",
        "python script.py",
        "python -m other pytest",
    ] {
        let outcome =
            finished_with_store(raw.as_bytes(), End::Exited(0), command, 120, false, None);
        assert_eq!(
            outcome.content,
            format!("{}\n[exit code: 0]", raw.trim_end_matches('\n')),
            "{command}"
        );
    }
}

#[test]
fn notice_inclusive_shrink_regression() {
    // Body shrinks (cargo's chatter becomes `ok`), but framing erases the gain.
    for (raw, stored) in [
        ("   Compiling a v0.1.0\n".to_owned(), None),
        (
            "   Compiling a v0.1.0\n".repeat(4),
            Some(StoredOutput {
                handle: "out-515-regression".into(),
                stored_bytes: 84,
                capture: Capture::Complete,
            }),
        ),
        (
            "   Compiling a v0.1.0\n".repeat(3),
            Some(StoredOutput {
                handle: "".into(),
                stored_bytes: 0,
                capture: Capture::StorageFailed,
            }),
        ),
    ] {
        let filtered = finished_with_store(
            raw.as_bytes(),
            End::Exited(0),
            "cargo build",
            120,
            false,
            stored.as_ref(),
        );
        let unfiltered = finished_with_store(
            raw.as_bytes(),
            End::Exited(0),
            "cargo build",
            120,
            true,
            stored.as_ref(),
        );
        assert_eq!(
            filtered, unfiltered,
            "notice-inclusive result must fall back"
        );
    }
}
