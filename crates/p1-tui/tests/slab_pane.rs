//! Pane mocks (handoff §9): LEDGER, OUTPUT and WORKERS (wide and compact), built from the
//! `lib/p1-screens.js` inputs and checked cell for cell against `grids.json` through the
//! `common/slab` oracle. Plus unit coverage for mode availability/cycling and the LEDGER drop
//! order (§9.2), which have no standalone mock (they are behaviour, not a screen).

mod common;

use common::slab;
use p1_tui::band::Seg;
use p1_tui::fold::FoldId;
use p1_tui::palette;
use p1_tui::render::ledger::{
    self, ContextPartView, ContextView, FoldRef, LedgerPane, LedgerSpend, SessionView,
    WorkspaceView,
};
use p1_tui::render::output::{self, OutputPane};
use p1_tui::render::pane;
use p1_tui::render::workers::{self, BlockState, WorkerBlock, WorkersHeader, WorkersPane};
use p1_tui::state::{PaneMode, Screen};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

/// Blit rendered pane lines into a fresh buffer at their own width, for the oracle.
fn buffer_of(lines: &[Line<'static>], width: usize) -> Buffer {
    let area = Rect::new(0, 0, width as u16, lines.len() as u16);
    let mut buf = Buffer::empty(area);
    for (y, line) in lines.iter().enumerate() {
        buf.set_line(0, y as u16, line, width as u16);
    }
    buf
}

/// The `LEDGER` fixture from `p1-screens.js` (`var LEDGER = {...}`), first mock example:
/// full pane, context well under the summarize threshold.
fn ledger_fixture() -> LedgerPane {
    LedgerPane {
        goal: Some("fix compaction boundary stall".into()),
        session: Some(SessionView {
            model: "claude/opus-5.5".into(),
            effort: "high".into(),
            access: "full".into(),
            sandbox: "bubblewrap".into(),
        }),
        context: Some(ContextView {
            used: Some(12_400),
            window: 120_000,
            summarize_at: 96_000,
            parts: vec![],
        }),
        workspace: Some(WorkspaceView {
            files: Some(1),
            diff: None,
            journal: Some("12s ago".into()),
        }),
        spend: Some(LedgerSpend {
            input: Some(38_100),
            output: Some(1_900),
            cache_hit_percent: Some(16),
            cost_micro_usd: None,
        }),
        workers: None,
        folds: vec![FoldRef {
            handle: "h-0275b8a9".into(),
            kind: "shell".into(),
            lines: 94,
        }],
    }
}

/// The second `el-ledger` example: context alone, at the summarize threshold, with the parts
/// breakdown (the context-stats seam's shape once it exists).
fn ledger_context_at_threshold() -> LedgerPane {
    LedgerPane {
        context: Some(ContextView {
            used: Some(97_100),
            window: 120_000,
            summarize_at: 96_000,
            parts: vec![
                ContextPartView {
                    label: "system".into(),
                    count: None,
                    tokens: 1_200,
                },
                ContextPartView {
                    label: "files".into(),
                    count: Some(4),
                    tokens: 61_800,
                },
                ContextPartView {
                    label: "tools".into(),
                    count: Some(11),
                    tokens: 3_100,
                },
                ContextPartView {
                    label: "recent".into(),
                    count: None,
                    tokens: 31_000,
                },
            ],
        }),
        ..Default::default()
    }
}

#[test]
fn el_ledger_38() {
    let full = ledger::render(&ledger_fixture(), 38, None);
    let buf = buffer_of(&full, 38);
    slab::assert_mock_rows(&buf, buf.area, "el-ledger@38", 0..full.len());

    let threshold = ledger::render(&ledger_context_at_threshold(), 38, None);
    let buf = buffer_of(&threshold, 38);
    let mock = slab::mock("el-ledger@38");
    let start = mock.height() - threshold.len();
    slab::assert_mock_rows(&buf, buf.area, "el-ledger@38", start..mock.height());
}

/// The `F.shellFail` fixture's fold, opened in OUTPUT at 56 (`el-output@56`).
fn output_fixture() -> OutputPane {
    OutputPane {
        handle: "h-0275b8a9".into(),
        source: vec![
            Seg::new(
                palette::DIM,
                "shell · cargo test -p p1-context · ".to_string(),
            ),
            Seg::new(palette::FAIL, "✗".to_string()),
            Seg::new(palette::DIM, " exit 101".to_string()),
        ],
        range: "80–94 of 94".into(),
        lines: vec![
            (80, "test compaction::case_6 ... ok".into()),
            (81, "test compaction::case_7 ... ok".into()),
            (82, String::new()),
            (83, "failures:".into()),
            (84, String::new()),
            (
                85,
                "---- compaction::hard_pressure_waits stdout ----".into(),
            ),
            (
                86,
                "thread 'compaction::hard_pressure_waits' panicked at crates/p1-context/src/edge.rs:414:9:"
                    .into(),
            ),
            (87, "assertion `left == right` failed".into()),
            (88, "  left: Hard".into()),
            (89, " right: Ready".into()),
        ],
    }
}

#[test]
fn el_output_56() {
    let lines = output::render(&output_fixture(), 56);
    let buf = buffer_of(&lines, 56);
    slab::assert_mock(&buf, buf.area, "el-output@56");
}

/// The `WORKERS` fixture from `p1-screens.js`, in its canonical (already state-sorted) order.
fn workers_fixture() -> Vec<WorkerBlock> {
    vec![
        WorkerBlock {
            id: "w3".into(),
            task: "audit sandbox read paths".into(),
            route: "deepseek2/v4.1-flash".into(),
            state: BlockState::NeedsReview,
            elapsed: Some("0m48s".into()),
            cost_micro_usd: None,
            grants: "read grep shell finish".into(),
            activity: "shell rm -rf target/ · awaiting approval".into(),
        },
        WorkerBlock {
            id: "w2".into(),
            task: "split provider-http helpers".into(),
            route: "deepseek2/v4.1-flash".into(),
            state: BlockState::Running,
            elapsed: Some("0m52s".into()),
            cost_micro_usd: None,
            grants: "read edit shell finish".into(),
            activity: "edit crates/p1-provider-http/src/retry.rs".into(),
        },
        WorkerBlock {
            id: "w4".into(),
            task: "measure summarize threshold".into(),
            route: "glm/5.3".into(),
            state: BlockState::Failed,
            elapsed: Some("1m03s".into()),
            cost_micro_usd: None,
            grants: "read shell finish".into(),
            activity: "RateLimited: HTTP 429".into(),
        },
        WorkerBlock {
            id: "w6".into(),
            task: "rename ToolFace".into(),
            route: "deepseek/v4.1-flash".into(),
            state: BlockState::Stalled,
            elapsed: Some("6m40s".into()),
            cost_micro_usd: None,
            grants: "read edit finish".into(),
            activity: "6 summaries without a workspace change".into(),
        },
        WorkerBlock {
            id: "w5".into(),
            task: "doc note for ADR-0050".into(),
            route: "claude/sonnet-5".into(),
            state: BlockState::Queued,
            elapsed: None,
            cost_micro_usd: None,
            grants: "read write finish".into(),
            activity: "waiting for a pool slot".into(),
        },
        WorkerBlock {
            id: "w1".into(),
            task: "reject cred-dir ancestors".into(),
            route: "gpt/gpt-5.6-luna".into(),
            state: BlockState::DoneUnverified,
            elapsed: Some("2m10s".into()),
            cost_micro_usd: None,
            grants: "read edit finish".into(),
            activity: "not verified — parent verification required".into(),
        },
        WorkerBlock {
            id: "w0".into(),
            task: "resume probe".into(),
            route: "claude/opus-5.5".into(),
            state: BlockState::Lost,
            elapsed: None,
            cost_micro_usd: None,
            grants: "read finish".into(),
            activity: "not restored on resume".into(),
        },
    ]
}

#[test]
fn el_workers_pane_56_wide() {
    let pane = WorkersPane {
        header: WorkersHeader {
            live: 1,
            queued: Some(1),
            pool: "3/4".into(),
        },
        workers: workers_fixture(),
        focused: None,
    };
    let lines = workers::render(&pane, 56, false);
    let buf = buffer_of(&lines, 56);
    slab::assert_mock(&buf, buf.area, "el-workers-pane@56");
}

#[test]
fn el_workers_pane_38_compact() {
    let pane = WorkersPane {
        header: WorkersHeader {
            live: 1,
            queued: None,
            pool: "3/4".into(),
        },
        workers: workers_fixture().into_iter().take(4).collect(),
        focused: None,
    };
    let lines = workers::render(&pane, 38, true);
    let buf = buffer_of(&lines, 38);
    slab::assert_mock(&buf, buf.area, "el-workers-pane-38@38");
}

// ---------------------------------------------------------------------------
// Mode strip against the screens that carry it (no standalone element mock exists — the
// oracle's own `assert_mock_region` is built for exactly this, per its doc comment).

#[test]
fn mode_strip_matches_s01_and_w01() {
    let line = pane::mode_strip(
        38,
        &[PaneMode::Ledger, PaneMode::Output, PaneMode::Workers],
        PaneMode::Ledger,
        false,
    );
    let buf = buffer_of(&[line], 38);
    slab::assert_mock_region(&buf, Rect::new(0, 0, 38, 1), "S01", 36, 80);

    let line = pane::mode_strip(
        56,
        &[PaneMode::Ledger, PaneMode::Output, PaneMode::Workers],
        PaneMode::Workers,
        true,
    );
    let buf = buffer_of(&[line], 56);
    slab::assert_mock_region(&buf, Rect::new(0, 0, 56, 1), "W01", 44, 102);
}

#[test]
fn peek_matches_p02() {
    let lines = pane::peek(
        38,
        "shell failed",
        "exit 101 · hard_pressure_waits",
        Some(3),
    );
    let buf = buffer_of(&lines, 38);
    slab::assert_mock_region(&buf, Rect::new(0, 0, 38, 2), "P02", 2, 80);
}

// ---------------------------------------------------------------------------
// Mode availability, cycling and the LEDGER drop order (§9.1/§9.2): behaviour, not a mock.

#[test]
fn ledger_drops_sections_lowest_priority_first_when_the_pane_is_short() {
    let full = ledger_fixture();
    let unbounded = ledger::render(&full, 38, None);
    let total = unbounded.len();

    // Just under the full height: FOLDS (the lowest priority) goes first —
    // its ref handle disappears, everything above it is unaffected.
    let dropped_folds = ledger::render(&full, 38, Some(total - 1));
    assert!(dropped_folds.len() < total);
    assert!(
        dropped_folds
            .iter()
            .all(|l| !l.to_string().contains("h-0275b8a9")),
        "FOLDS is the first section dropped"
    );
    assert!(
        dropped_folds.iter().any(|l| l.to_string().contains("GOAL")),
        "GOAL is not dropped yet"
    );

    // Just enough for GOAL (its header plus one wrapped line): every other
    // section is gone, GOAL (dropped last) is still whole.
    let goal_len = ledger::render(&full, 38, Some(1_000))[..2].len();
    let starved = ledger::render(&full, 38, Some(goal_len));
    assert_eq!(starved.len(), goal_len);
    assert!(starved[0].to_string().contains("GOAL"));

    // Too short even for GOAL whole: sections drop as WHOLE units (§9.2
    // never renders a bare header with its body cut), so the pane goes
    // empty rather than half-showing GOAL.
    let nothing = ledger::render(&full, 38, Some(goal_len - 1));
    assert!(nothing.is_empty());
}

#[test]
fn available_modes_gate_ledger_output_workers_and_never_offer_diff() {
    let mut screen = Screen::new(false);
    assert_eq!(screen.available_modes(), vec![PaneMode::Ledger]);
    screen.output = Some(p1_tui::render::output::OutputView {
        id: FoldId::of("x"),
        lines: vec!["line".into()],
        scroll: 0,
    });
    assert!(screen.available_modes().contains(&PaneMode::Output));
    screen.workers_ever_started = true;
    assert!(screen.available_modes().contains(&PaneMode::Workers));
    assert!(
        !screen.available_modes().contains(&PaneMode::Diff),
        "DIFF has no seam yet (handoff §14.4)"
    );
}

#[test]
fn ctrl_tab_cycles_only_through_available_modes() {
    let mut screen = Screen::new(false);
    screen.cycle_mode();
    assert_eq!(
        screen.pane_mode,
        PaneMode::Ledger,
        "with nothing else available, cycling is a no-op"
    );
    screen.workers_ever_started = true;
    screen.cycle_mode();
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    screen.cycle_mode();
    assert_eq!(screen.pane_mode, PaneMode::Ledger, "wraps");
}
