//! Tool Block snapshots against the SLAB handoff oracle.
mod common;

use common::slab;
use p1_contracts::ToolStatus;
use p1_tui::{
    face::{CallFace, FaceBody, ResultFace, TargetKind},
    render::{
        block::{self, DecisionOption, InlineApproval},
        diff::DiffRow,
    },
    transcript::{RowStatus, ToolRow},
};
use ratatui::{buffer::Buffer, layout::Rect, widgets::Widget};
use std::sync::Mutex;

static FAILURES: Mutex<Vec<String>> = Mutex::new(Vec::new());

fn row(
    name: &str,
    target: &str,
    kind: TargetKind,
    status: RowStatus,
    outcome: Option<&str>,
    body: FaceBody,
) -> ToolRow {
    let output = match &body {
        FaceBody::Lines(lines) => Some(lines.join("\n")),
        _ => None,
    };
    ToolRow {
        name: name.into(),
        summary: target.into(),
        status,
        output,
        line_count: 0,
        fold: None,
        elapsed_ms: Some(4200),
        call_id: "id".into(),
        call: None,
        face: CallFace {
            target: target.into(),
            kind,
        },
        result_face: Some(ResultFace {
            outcome: outcome.map(str::to_owned),
            body,
            meta: None,
        }),
        input_preview: None,
    }
}
fn ok(n: &str, t: &str, k: TargetKind, o: &str, b: FaceBody) -> ToolRow {
    row(n, t, k, RowStatus::Settled(ToolStatus::Ok), Some(o), b)
}
fn err(n: &str, t: &str, k: TargetKind, o: &str, b: FaceBody) -> ToolRow {
    row(n, t, k, RowStatus::Settled(ToolStatus::Error), Some(o), b)
}
fn diff() -> FaceBody {
    FaceBody::Diff(vec![
        DiffRow::Context {
            line: 411,
            text: "let pressure = self.pressure_at_edge();".into(),
        },
        DiffRow::Del {
            line: 412,
            text: "if pressure == Pressure::Hard {".into(),
        },
        DiffRow::Del {
            line: 413,
            text: "    block_until_ready(&worker);".into(),
        },
        DiffRow::Del {
            line: 414,
            text: "}".into(),
        },
        DiffRow::Add {
            line: 412,
            text: "if let Some(summary) = ready {".into(),
        },
        DiffRow::Add {
            line: 413,
            text: "    return self.apply_at_boundary(summary);".into(),
        },
        DiffRow::Add {
            line: 414,
            text: "}".into(),
        },
        DiffRow::Context {
            line: 415,
            text: "self.commit_boundary()".into(),
        },
    ])
}
fn verify(id: &str, width: u16, cases: Vec<(usize, ToolRow, Option<InlineApproval>)>) {
    let mut problems = Vec::new();
    for (start, row, approval) in cases {
        let mut rendered =
            block::lines_with_approval(&row, width as usize, false, 0, true, approval.as_ref());
        let mock = slab::mock(id);
        if mock
            .text
            .get(start + rendered.len())
            .is_some_and(|line| line.trim().is_empty())
        {
            rendered.push(ratatui::text::Line::default());
        }
        let height = rendered.len() as u16;
        let area = Rect::new(0, 0, width, height);
        let mut buffer = Buffer::empty(area);
        for (y, line) in rendered.iter().enumerate() {
            line.clone()
                .render(Rect::new(0, y as u16, width, 1), &mut buffer);
        }
        if let Err(problem) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            slab::assert_mock_rows(&buffer, area, id, start..start + height as usize)
        })) {
            let detail = problem
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| problem.downcast_ref::<&str>().copied())
                .unwrap_or("unknown panic");
            problems.push(format!("{id} example at mock row {start}: {detail}"));
        }
    }
    FAILURES.lock().unwrap().extend(problems);
}
fn folded_diff() -> ToolRow {
    let mut row = ok(
        "edit",
        "crates/p1-context/src/lib.rs",
        TargetKind::Path,
        "+21 −4",
        diff(),
    );
    row.line_count = 25;
    row.fold = Some(p1_tui::fold::FoldId("h-9c1e44d0".into()));
    row
}
fn shell_failure() -> ToolRow {
    let mut call = err(
        "shell",
        "cargo test -p p1-context boundary",
        TargetKind::Command,
        "11.4s · exit 101 · 94 lines",
        lines(&[
            "test compaction::case_7 ... ok",
            "failures:",
            "",
            "---- compaction::hard_pressure_waits stdout ----",
            "thread 'compaction::hard_pressure_waits' panicked at crates/p1-context/src/edge.rs:414:9:",
            "assertion `left == right` failed",
            "  left: Hard",
            " right: Ready",
        ]),
    );
    call.line_count = 94;
    call.fold = Some(p1_tui::fold::FoldId("h-0275b8a9".into()));
    call.result_face.as_mut().unwrap().meta =
        Some("cwd ~/dev/phaseone · bubblewrap · writes: workspace · net off".into());
    call
}
fn no_body() -> FaceBody {
    FaceBody::None
}
fn lines(v: &[&str]) -> FaceBody {
    FaceBody::Lines(v.iter().map(|s| s.to_string()).collect())
}

#[test]
fn generic_and_tool_blocks_match_the_mock_examples() {
    let path = "crates/p1-context/src/edge.rs";
    let plain = TargetKind::Plain;
    let path_kind = TargetKind::Path;
    let cmd = TargetKind::Command;
    let mut states = vec![
        (
            0,
            row(
                "",
                "*** Begin Patch",
                plain,
                RowStatus::Running,
                Some("1.4 kB"),
                lines(&[
                    "+    return self.apply_at_boundary(summary);",
                    "+}",
                    " self.commit_boundary()",
                ]),
            ),
            None,
        ),
        (
            5,
            row(
                "shell",
                "cargo test -p p1-context boundary",
                cmd,
                RowStatus::Running,
                None,
                no_body(),
            ),
            None,
        ),
        (
            7,
            row(
                "shell",
                "cargo build --release",
                cmd,
                RowStatus::Running,
                Some("awaiting approval"),
                no_body(),
            ),
            None,
        ),
        (
            9,
            ok("read", path, path_kind, "412 lines · 14.2 kB", no_body()),
            None,
        ),
        (
            11,
            err(
                "lint",
                "crates/p1-tui",
                plain,
                "2.1s · 3 findings",
                lines(&[
                    "warning: unused variable `pad` at render/diff.rs:131",
                    "warning: needless borrow at render/ledger.rs:88",
                    "error: this `if` has identical blocks at state.rs:412",
                ]),
            ),
            None,
        ),
        (
            16,
            err(
                "shell",
                "rm -rf target/",
                cmd,
                "denied · by operator",
                no_body(),
            ),
            None,
        ),
        (
            18,
            row(
                "shell",
                "cargo test --workspace",
                cmd,
                RowStatus::Settled(ToolStatus::Cancelled),
                Some("cancelled"),
                no_body(),
            ),
            None,
        ),
        (
            20,
            row(
                "read",
                "crates/p1-host/src/run.rs",
                path_kind,
                RowStatus::Settled(ToolStatus::Unknown),
                Some("unknown · turn ended"),
                no_body(),
            ),
            None,
        ),
        (
            22,
            row(
                "search",
                r#"{"pattern":"retries"}"#,
                plain,
                RowStatus::Settled(ToolStatus::Unavailable),
                Some("unavailable · not assembled"),
                no_body(),
            ),
            None,
        ),
        (
            24,
            ok(
                "worker_continue",
                "w1 +edit",
                plain,
                "resumed · +edit",
                no_body(),
            ),
            None,
        ),
    ];
    states[1].1.elapsed_ms = Some(4200);
    verify("el-tool-states@76", 76, states);

    verify(
        "el-tool-states-56@56",
        56,
        vec![
            (
                0,
                ok("read", path, path_kind, "412 lines · 14.2 kB", no_body()),
                None,
            ),
            (
                2,
                ok(
                    "shell",
                    "cargo test -p p1-provider-http --all-features -- --nocapture",
                    cmd,
                    "3.1s · exit 0 · 212 lines",
                    no_body(),
                ),
                None,
            ),
            (
                4,
                row(
                    "shell",
                    "cargo test -p p1-context boundary",
                    cmd,
                    RowStatus::Running,
                    None,
                    no_body(),
                ),
                None,
            ),
            (
                6,
                ok(
                    "apply_patch",
                    "3 files",
                    plain,
                    "+48 −12 · 3 files",
                    no_body(),
                ),
                None,
            ),
        ],
    );

    verify(
        "el-tools@76",
        76,
        vec![
            (
                0,
                ok(
                    "read",
                    "crates/p1-context/src/edge.rs:380-460",
                    path_kind,
                    "81 lines · 3.0 kB",
                    no_body(),
                ),
                None,
            ),
            (
                2,
                ok(
                    "write",
                    "docs/design/tui/NOTES.md",
                    path_kind,
                    "48 lines · 1.9 kB · new",
                    no_body(),
                ),
                None,
            ),
            (4, ok("edit", path, path_kind, "+3 −3", diff()), None),
            (14, folded_diff(), None),
            (
                25,
                ok(
                    "apply_patch",
                    "3 files",
                    plain,
                    "+48 −12 · 3 files",
                    FaceBody::Files(vec![
                        (path.into(), "+12 −3".into()),
                        ("crates/p1-context/src/lib.rs".into(), "+30 −9".into()),
                        ("crates/p1-context/tests/boundary.rs".into(), "+6 −0".into()),
                    ]),
                ),
                None,
            ),
            (
                30,
                ok(
                    "grep",
                    "block_until_ready crates/",
                    plain,
                    "3 hits · 2 files",
                    no_body(),
                ),
                None,
            ),
            (
                32,
                ok(
                    "shell",
                    "cargo test -p p1-context boundary",
                    cmd,
                    "3.1s · exit 0 · 94 lines",
                    no_body(),
                ),
                None,
            ),
            (
                34,
                ok(
                    "finish",
                    "done",
                    plain,
                    "verified · 1 command",
                    lines(&["✓ cargo test -p p1-context  3.1s · after the last change"]),
                ),
                None,
            ),
        ],
    );

    verify(
        "el-tools-fail@76",
        76,
        vec![
            (0, shell_failure(), None),
            (
                12,
                err(
                    "read",
                    "crates/p1-context/src/boundary.rs",
                    path_kind,
                    "no such file",
                    no_body(),
                ),
                None,
            ),
            (
                14,
                err(
                    "edit",
                    path,
                    path_kind,
                    "old_string not found",
                    lines(&[
                        "old_string not found in crates/p1-context/src/edge.rs (read it again before editing)",
                    ]),
                ),
                None,
            ),
            (
                17,
                err(
                    "finish",
                    "done",
                    plain,
                    "rejected",
                    lines(&[
                        "no successful run of \"cargo test -p p1-context\" after the last change",
                    ]),
                ),
                None,
            ),
            (
                20,
                err(
                    "finish",
                    "blocked",
                    plain,
                    "blocked · needs edit",
                    lines(&["the worker was granted read, grep, finish; the fix needs edit"]),
                ),
                None,
            ),
            (
                23,
                ok(
                    "finish",
                    "done",
                    plain,
                    "done · not verified — parent verification required",
                    no_body(),
                ),
                None,
            ),
        ],
    );

    verify(
        "el-workers-tools@76",
        76,
        vec![
            (
                0,
                ok(
                    "worker_start",
                    "w2 · deepseek2/v4.1-flash",
                    plain,
                    "started",
                    lines(&[
                        "split provider-http helpers into p1-provider-http (#47)",
                        "grants    read edit shell finish",
                    ]),
                ),
                None,
            ),
            (
                4,
                err(
                    "worker_start",
                    "w7 · glm/5.3",
                    plain,
                    "not started",
                    lines(&["apply_patch is freeform; the glm route has function tools only"]),
                ),
                None,
            ),
            (
                7,
                ok(
                    "worker_continue",
                    "w1 +edit",
                    plain,
                    "resumed · +edit",
                    no_body(),
                ),
                None,
            ),
            (
                9,
                ok(
                    "worker_result",
                    "w1",
                    plain,
                    "blocked · 6 lines",
                    lines(&[
                        "tools    read grep finish",
                        "finish    blocked",
                        "needs    edit",
                        "tried    edit ×2",
                    ]),
                ),
                None,
            ),
            (
                15,
                ok("worker_cancel", "w4", plain, "cancelled", no_body()),
                None,
            ),
        ],
    );

    let option = |key: &str, label: &str, unavailable: Option<&str>| DecisionOption {
        key: key.into(),
        label: label.into(),
        unavailable: unavailable.map(str::to_owned),
    };
    verify(
        "el-approval-permission@76",
        76,
        vec![(
            0,
            row(
                "shell",
                "cargo build --release",
                cmd,
                RowStatus::Running,
                Some("awaiting approval"),
                no_body(),
            ),
            Some(InlineApproval {
                permission_rows: vec![
                    ("cwd".into(), "~/dev/phaseone".into(), true),
                    (
                        "sandbox".into(),
                        "bubblewrap · writes: workspace".into(),
                        false,
                    ),
                    ("network".into(), "off".into(), false),
                    ("effect".into(), "runs a process".into(), false),
                ],
                diff: false,
                options: vec![
                    option("y", "allow once", None),
                    option("a", "session", None),
                    option("p", "project", Some("not available — no trust store yet")),
                    option("n", "deny", None),
                ],
                hints: vec![],
            }),
        )],
    );
    verify(
        "el-approval-floor@76",
        76,
        vec![(
            0,
            row(
                "shell",
                "rm -rf target/",
                cmd,
                RowStatus::Running,
                Some("awaiting approval"),
                no_body(),
            ),
            Some(InlineApproval {
                permission_rows: vec![
                    ("from".into(), "w3 · deepseek2/v4.1-flash".into(), false),
                    ("cwd".into(), "~/dev/phaseone".into(), true),
                    (
                        "sandbox".into(),
                        "bubblewrap · writes: workspace".into(),
                        false,
                    ),
                    ("network".into(), "off".into(), false),
                    (
                        "effect".into(),
                        "runs a process · destructive".into(),
                        false,
                    ),
                ],
                diff: false,
                options: vec![
                    option("y", "allow once", None),
                    option("a", "session", Some("not grantable — destructive floor")),
                    option("p", "project", Some("not grantable — destructive floor")),
                    option("n", "deny", None),
                ],
                hints: vec!["1 of 2 pending".into()],
            }),
        )],
    );
    verify(
        "el-approval-edit@76",
        76,
        vec![(
            0,
            ok("edit", path, path_kind, "+3 −3 · 1 of 1 files", diff()),
            Some(InlineApproval {
                permission_rows: vec![],
                diff: true,
                options: vec![
                    option("y", "allow once", None),
                    option("a", "session", None),
                    option("p", "project", None),
                    option("n", "deny", None),
                ],
                hints: vec!["^D review".into()],
            }),
        )],
    );
    let failures = FAILURES.lock().unwrap();
    assert!(failures.is_empty(), "{}", failures.join("\\n\\n"));
}
