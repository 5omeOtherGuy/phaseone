//! Every full-screen mock of the handoff (§16, `grids.json` screens S01–F01), composed by
//! `render::screen::draw` from a `Screen` built through its public API with the state
//! `lib/p1-screens.js` gives each screen, and compared cell for cell (reduced motion; the
//! hardware cursor painted where the mocks draw it, one amber cell).
//!
//! A screen is asserted WHOLE wherever the state it shows exists. Where a mock needs state or a
//! seam that does not exist yet, or contradicts a lead decision or the handoff's own rules, the
//! test asserts every other cell and its doc comment names the rows left out and why.
mod common;

use std::collections::VecDeque;
use std::sync::Arc;

use common::slab;
use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::band::Seg;
use p1_tui::face::{CallFace, FaceBody, GenericDescriber, ResultFace, TargetKind, ToolDescriber};
use p1_tui::fold::FoldId;
use p1_tui::palette;
use p1_tui::render::diff::{DiffRow, DiffView};
use p1_tui::render::home::HomePrelude;
use p1_tui::render::ledger::{ContextView, FoldRef, SessionView, WorkspaceView};
use p1_tui::render::output::OutputView;
use p1_tui::render::permission::PermissionView;
use p1_tui::render::picker::{Picker, PickerGroup, PickerRow};
use p1_tui::render::screen::draw;
use p1_tui::render::workers::{BlockState, WorkerBlock};
use p1_tui::state::{Approval, AttachedWorker, PaneMode, PaneWidth, Queued, Screen};
use p1_tui::transcript::{
    Block, CommandOutput, CommandRow, NoticeFact, NoticeKind, RowStatus, ToolRow, Transcript,
    TurnNotice, WorkerEnd, WorkerReport,
};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

const EDGE: &str = "crates/p1-context/src/edge.rs";
const ME: &str = "claude/opus-5.5";
/// The turn's clock in these screens starts here; live elapsed times count from it.
const T0: u64 = 100_000;

// ---------- rendering and comparison ----------

/// Draw `screen` at `w`×`h` and paint the hardware cursor cell the way the mocks draw it.
fn render(screen: &mut Screen, w: u16, h: u16, now_ms: u64) -> Buffer {
    let area = Rect::new(0, 0, w, h);
    let mut buf = Buffer::empty(area);
    draw(screen, area, &mut buf, now_ms);
    if let Some((x, y)) = screen.cursor {
        let cell = &mut buf[(x, y)];
        cell.bg = palette::AMBER_FILL;
        cell.fg = palette::ON_FILL;
    }
    buf
}

/// Every row of mock `id` except `skip`, at full width.
fn assert_rows_except(buf: &Buffer, id: &str, skip: &[usize]) {
    let mock = slab::mock(id);
    let width = mock.width() as u16;
    let mut y = 0;
    while y < mock.height() {
        if skip.contains(&y) {
            y += 1;
            continue;
        }
        let start = y;
        while y < mock.height() && !skip.contains(&y) {
            y += 1;
        }
        let area = Rect::new(0, start as u16, width, (y - start) as u16);
        slab::assert_mock_region(buf, area, id, start, 0);
    }
}

/// The cells of mock `id` in `rows` × `cols`.
fn assert_cells(
    buf: &Buffer,
    id: &str,
    rows: std::ops::Range<usize>,
    cols: std::ops::Range<usize>,
) {
    let area = Rect::new(
        cols.start as u16,
        rows.start as u16,
        cols.len() as u16,
        rows.len() as u16,
    );
    slab::assert_mock_region(buf, area, id, rows.start, cols.start);
}

/// One row of the buffer as text.
fn row_text(buf: &Buffer, y: u16) -> String {
    (0..buf.area.width).map(|x| buf[(x, y)].symbol()).collect()
}

// ---------- shared state from p1-screens.js ----------

/// The host describes calls (§7.1); this stands in for it: the raw input is the target, and
/// the kind follows the tool as the handoff's mocks show it. Results are never applied as
/// events here (settled Blocks are built whole), so they get the generic face.
struct Describer;

impl ToolDescriber for Describer {
    fn call(&self, call: &ToolCall) -> CallFace {
        let kind = match call.name.as_str() {
            "read" | "edit" | "write" => TargetKind::Path,
            "shell" => TargetKind::Command,
            _ => TargetKind::Plain,
        };
        CallFace {
            target: call.input.raw().to_string(),
            kind,
        }
    }

    fn result(
        &self,
        call: &ToolCall,
        result: &ToolResultItem,
        elapsed_ms: Option<u64>,
    ) -> ResultFace {
        GenericDescriber.result(call, result, elapsed_ms)
    }
}

fn transcript() -> Transcript {
    let mut t = Transcript::with_describer(Arc::new(Describer));
    t.set_path_exists(|path| path == EDGE);
    t
}

fn screen() -> Screen {
    let mut s = Screen::new(true);
    s.transcript = transcript();
    s.statusbar.model = Some(ME.into());
    s.statusbar.effort = Some("high".into());
    s.statusbar.repo = Some("phaseone".into());
    s.statusbar.branch = Some("main".into());
    s.statusbar.ctx = Some("10%".into());
    s.statusbar.clock = Some("0h14".into());
    s
}

/// `LEDGER` and `strip("ledger")`: every section, and the three modes available.
fn with_ledger(s: &mut Screen) {
    s.goal = Some("fix compaction boundary stall".into());
    s.session = Some(session());
    s.context = Some(ContextView {
        used: Some(12_400),
        window: 120_000,
        summarize_at: 96_000,
        parts: vec![],
    });
    s.workspace = Some(WorkspaceView {
        files: Some(1),
        diff: None,
        journal: Some("12s ago".into()),
    });
    s.spend.responses = 1;
    s.spend.input = Some(38_100);
    s.spend.output = Some(1_900);
    // 16% of the input served from cache.
    s.spend.cached = Some(6_096);
    s.spend.cost_micro_usd = None;
    s.folds = vec![FoldRef {
        handle: "h-0275b8a9".into(),
        kind: "shell".into(),
        lines: 94,
    }];
    s.output = Some(fold_output(0));
    s.workers_ever_started = true;
}

fn session() -> SessionView {
    SessionView {
        model: ME.into(),
        effort: "high".into(),
        access: "full".into(),
        sandbox: "bubblewrap".into(),
    }
}

/// The shell failure's 94 output lines behind `h-0275b8a9`; lines 80–89 are the mock's.
fn fold_output(scroll: usize) -> OutputView {
    let mut lines: Vec<String> = (1..=94).map(|n| format!("output line {n}")).collect();
    for (n, text) in [
        "test compaction::case_6 ... ok",
        "test compaction::case_7 ... ok",
        "",
        "failures:",
        "",
        "---- compaction::hard_pressure_waits stdout ----",
        "thread 'compaction::hard_pressure_waits' panicked at crates/p1-context/src/edge.rs:414:9:",
        "assertion `left == right` failed",
        "  left: Hard",
        " right: Ready",
    ]
    .into_iter()
    .enumerate()
    {
        lines[79 + n] = text.into();
    }
    OutputView {
        id: FoldId("h-0275b8a9".into()),
        lines,
        scroll,
    }
}

fn diff_rows(shift: u32) -> Vec<DiffRow> {
    let context = |line: u32, text: &str| DiffRow::Context {
        line: line + shift,
        text: text.into(),
    };
    let del = |line: u32, text: &str| DiffRow::Del {
        line: line + shift,
        text: text.into(),
    };
    let add = |line: u32, text: &str| DiffRow::Add {
        line: line + shift,
        text: text.into(),
    };
    vec![
        context(411, "let pressure = self.pressure_at_edge();"),
        del(412, "if pressure == Pressure::Hard {"),
        del(413, "    block_until_ready(&worker);"),
        del(414, "}"),
        add(412, "if let Some(summary) = ready {"),
        add(413, "    return self.apply_at_boundary(summary);"),
        add(414, "}"),
        context(415, "self.commit_boundary()"),
    ]
}

/// A settled tool call as the host's describer states it.
fn call(
    name: &str,
    target: &str,
    status: ToolStatus,
    outcome: Option<&str>,
    body: FaceBody,
) -> Block {
    let kind = match name {
        "read" | "edit" | "write" => TargetKind::Path,
        "shell" => TargetKind::Command,
        _ => TargetKind::Plain,
    };
    let output = match &body {
        FaceBody::Lines(lines) if status != ToolStatus::Ok => Some(lines.join("\n")),
        _ => None,
    };
    Block::Call(ToolRow {
        name: name.into(),
        summary: target.into(),
        status: RowStatus::Settled(status),
        output,
        line_count: 0,
        fold: None,
        elapsed_ms: None,
        call_id: format!("{name}-{target}"),
        call: None,
        face: CallFace {
            target: target.into(),
            kind,
        },
        result_face: Some(ResultFace {
            outcome: outcome.map(str::to_string),
            body,
            meta: None,
            target: None,
        }),
        input_preview: None,
    })
}

fn lines(texts: &[&str]) -> FaceBody {
    FaceBody::Lines(texts.iter().map(|s| s.to_string()).collect())
}

fn ask() -> Block {
    Block::Operator {
        text: "why does compaction stall at the turn edge?".into(),
        steering: false,
    }
}

fn reason() -> Block {
    Block::Reasoning {
        lines: vec![],
        expanded: false,
        elapsed_ms: Some(4_200),
        started_ms: None,
    }
}

fn prose() -> Block {
    Block::Prose {
        lines: vec![format!(
            "The hard-pressure wait in `{EDGE}` blocks the turn boundary instead of applying \
             the summary the worker already prepared. Three things line up:"
        )],
    }
}

fn confirmed() -> Block {
    Block::Prose {
        lines: vec![
            "Confirmed — the ready summary never applies. Fixing the boundary and re-running."
                .into(),
        ],
    }
}

fn read() -> Block {
    call(
        "read",
        EDGE,
        ToolStatus::Ok,
        Some("412 lines · 14.2 kB"),
        FaceBody::None,
    )
}

fn grep() -> Block {
    call(
        "grep",
        "block_until_ready crates/",
        ToolStatus::Ok,
        Some("3 hits · 2 files"),
        FaceBody::None,
    )
}

fn edit() -> Block {
    call(
        "edit",
        EDGE,
        ToolStatus::Ok,
        Some("+3 −3"),
        FaceBody::Diff(diff_rows(0)),
    )
}

fn shell_fail() -> Block {
    let mut block = call(
        "shell",
        "cargo test -p p1-context boundary",
        ToolStatus::Error,
        Some("11.4s · exit 101 · 94 lines"),
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
    if let Block::Call(row) = &mut block {
        row.line_count = 94;
        row.fold = Some(FoldId("h-0275b8a9".into()));
        row.result_face.as_mut().unwrap().meta =
            Some("cwd ~/dev/phaseone · bubblewrap · writes: workspace · net off".into());
    }
    block
}

/// A live turn whose shell call started at `T0`: draw at `T0 + 4_200` for `4.2s  ▪▪▪`.
fn start_shell(s: &mut Screen) {
    s.apply(&AgentEvent::TurnStarted, T0);
    s.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "c-shell".into(),
                name: "shell".into(),
                input: ToolInput::Json("cargo test -p p1-context boundary".into()),
            },
        },
        T0,
    );
}

/// `base()`: the SESSION transcript with its running shell, the working composer, LEDGER.
fn base() -> Screen {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![ask(), reason(), prose(), read(), grep(), edit()];
    start_shell(&mut s);
    s
}

const NOW: u64 = T0 + 4_200;

fn worker(
    id: &str,
    task: &str,
    route: &str,
    state: BlockState,
    elapsed: Option<&str>,
    grants: &str,
    activity: &str,
) -> WorkerBlock {
    WorkerBlock {
        id: id.into(),
        task: task.into(),
        route: route.into(),
        state,
        elapsed: elapsed.map(str::to_string),
        cost_micro_usd: None,
        grants: grants.into(),
        activity: activity.into(),
    }
}

/// `WORKERS` from p1-screens.js, in its order.
fn workers() -> Vec<WorkerBlock> {
    vec![
        worker(
            "w3",
            "audit sandbox read paths",
            "deepseek2/v4.1-flash",
            BlockState::NeedsReview,
            Some("0m48s"),
            "read grep shell finish",
            "shell rm -rf target/ · awaiting approval",
        ),
        worker(
            "w2",
            "split provider-http helpers",
            "deepseek2/v4.1-flash",
            BlockState::Running,
            Some("0m52s"),
            "read edit shell finish",
            "edit crates/p1-provider-http/src/retry.rs",
        ),
        worker(
            "w4",
            "measure summarize threshold",
            "glm/5.3",
            BlockState::Failed,
            Some("1m03s"),
            "read shell finish",
            "RateLimited: HTTP 429",
        ),
        worker(
            "w6",
            "rename ToolFace",
            "deepseek/v4.1-flash",
            BlockState::Stalled,
            Some("6m40s"),
            "read edit finish",
            "6 summaries without a workspace change",
        ),
        worker(
            "w5",
            "doc note for ADR-0050",
            "claude/sonnet-5",
            BlockState::Queued,
            None,
            "read write finish",
            "waiting for a pool slot",
        ),
        worker(
            "w1",
            "reject cred-dir ancestors",
            "gpt/gpt-5.6-luna",
            BlockState::DoneUnverified,
            Some("2m10s"),
            "read edit finish",
            "not verified — parent verification required",
        ),
        worker(
            "w0",
            "resume probe",
            ME,
            BlockState::Lost,
            None,
            "read finish",
            "not restored on resume",
        ),
    ]
}

fn worker_start(id: &str, task: &str, grants: &str) -> Block {
    call(
        "worker_start",
        &format!("{id} · deepseek2/v4.1-flash"),
        ToolStatus::Ok,
        Some("started"),
        lines(&[task, &format!("grants    {grants}")]),
    )
}

/// `WSESSION`: the parent's view of starting workers and one worker's end.
fn worker_session() -> Vec<Block> {
    vec![
        ask(),
        read(),
        worker_start(
            "w2",
            "split provider-http helpers into p1-provider-http (#47)",
            "read edit shell finish",
        ),
        worker_start("w3", "audit sandbox read paths", "read grep shell finish"),
        Block::WorkerReport(WorkerReport {
            id: "w1".into(),
            route: "gpt/gpt-5.6-luna".into(),
            end: WorkerEnd::NotVerified,
            elapsed: Some("2m10s".into()),
            cost_micro_usd: None,
            grants: vec!["read".into(), "edit".into(), "finish".into()],
            line: "done · not verified — parent verification required".into(),
        }),
        call(
            "shell",
            "cargo test -p p1-host --test worker_grants",
            ToolStatus::Ok,
            Some("8.2s · exit 0 · 41 lines"),
            FaceBody::None,
        ),
        Block::Prose {
            lines: vec!["w1's change passes the worker_grants suite. Waiting on w2 and w3.".into()],
        },
    ]
}

/// The WORKERS pane of S04/W01: every state, the snapshot's own counts. The worker needing
/// review self-pins WORKERS (§9.1).
fn with_workers_pane(s: &mut Screen) {
    s.sync_workers(workers());
    s.workers.header.pool = "3/4".into();
    s.pane_mode = PaneMode::Workers;
    s.statusbar.workers = 1;
}

/// W02/W03: the first four workers, `w2` focused. The mock's header says `2 live` for the
/// same states S04 counts as `1 live` (illustrative data); the header is set as the mock says.
/// Its strip is unpinned although `w3` needs review (which self-pins, §9.1): the operator's
/// `^P` released the pin.
fn with_compact_workers(s: &mut Screen) {
    s.sync_workers(workers()[..4].to_vec());
    s.toggle_pin();
    s.workers.header.live = 2;
    s.workers.header.pool = "3/4".into();
    s.workers.focused = Some("w2".into());
    s.pane_mode = PaneMode::Workers;
    s.statusbar.workers = 2;
}

// ---------- S: sessions ----------

/// S01 — whole.
#[test]
fn s01_session_120x40() {
    let mut s = base();
    slab::assert_mock(
        &render(&mut s, 120, 40, NOW),
        Rect::new(0, 0, 120, 40),
        "S01",
    );
}

/// S02 — whole.
#[test]
fn s02_session_80x24() {
    let mut s = base();
    slab::assert_mock(&render(&mut s, 80, 24, NOW), Rect::new(0, 0, 80, 24), "S02");
}

/// S03 — whole.
#[test]
fn s03_session_100x30() {
    let mut s = base();
    slab::assert_mock(
        &render(&mut s, 100, 30, NOW),
        Rect::new(0, 0, 100, 30),
        "S03",
    );
}

/// S04 — whole except row 21 of the transcript column: the mock's composer says a turn is
/// running (`steer the running turn`) while nothing in its transcript runs, and §6.5 then
/// draws the turn working row under the last element (`▪▪▪  waiting`). That row's pane cells
/// are asserted. The strip is unpinned (S04 is "promoted by a live worker"; W01 is
/// the pinned one): the review's self-pin is released with `^P`.
#[test]
fn s04_workers_pane_160x48() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = worker_session();
    s.apply(&AgentEvent::TurnStarted, T0);
    with_workers_pane(&mut s);
    s.toggle_pin();
    let buf = render(&mut s, 160, 48, NOW);
    assert_rows_except(&buf, "S04", &[21]);
    assert_cells(&buf, "S04", 21..22, 100..160);
    assert!(row_text(&buf, 21).contains("▪▪▪  waiting"));
}

/// S05 — whole: focus mode is automatic at 12 rows and the empty composer hides.
#[test]
fn s05_short_screen_focus() {
    let mut s = base();
    slab::assert_mock(
        &render(&mut s, 120, 12, NOW),
        Rect::new(0, 0, 120, 12),
        "S05",
    );
    assert_eq!(s.cursor, None, "a hidden composer places no cursor");
}

/// S06 — whole.
#[test]
fn s06_overlay_80x24() {
    let mut s = base();
    s.ledger_overlay = true;
    slab::assert_mock(&render(&mut s, 80, 24, NOW), Rect::new(0, 0, 80, 24), "S06");
}

// ---------- H: home ----------

fn home(state: Vec<Vec<Seg>>) -> HomePrelude {
    HomePrelude {
        version: "0.1.0".into(),
        path: "~/dev/phaseone".into(),
        branch: Some("main".into()),
        state,
        items: [
            ("/resume", "reopen a previous session"),
            ("/model", "claude/opus-5.5:high"),
            ("/access", "full · --ask to confirm"),
            ("/goal", "set the session objective"),
            ("/help", "commands and keys"),
        ]
        .iter()
        .map(|(c, d)| (c.to_string(), d.to_string()))
        .collect(),
    }
}

fn ink(text: &str) -> Vec<Seg> {
    vec![Seg::new(palette::INK, text)]
}

/// `homeLedger` with no journal: SESSION and an unknown CONTEXT; only LEDGER is available.
fn home_screen(state: Vec<Vec<Seg>>) -> Screen {
    let mut s = screen();
    s.statusbar.ctx = None;
    s.statusbar.clock = Some("0h00".into());
    s.home = Some(home(state));
    s.session = Some(session());
    s.context = Some(ContextView {
        used: None,
        window: 120_000,
        summarize_at: 96_000,
        parts: vec![],
    });
    s
}

/// H01 — whole: the prelude on the Band rule, the monogram centred in the free rows.
#[test]
fn h01_home_first_run() {
    let mut s = home_screen(vec![ink("no journal in this directory.")]);
    slab::assert_mock(&render(&mut s, 120, 40, 0), Rect::new(0, 0, 120, 40), "H01");
}

/// H02 — whole: at 80×24 the free area is under 14 rows, so no monogram.
#[test]
fn h02_home_80x24_not_logged_in() {
    let mut s = home_screen(vec![
        ink("3 sessions in this directory · last today 21:10."),
        vec![
            Seg::new(palette::FAIL, "✗ "),
            Seg::new(palette::INK, ME),
            Seg::new(
                palette::DIM,
                "  no Claude Code login found · sign in to Claude Code, or /model",
            ),
        ],
    ]);
    slab::assert_mock(&render(&mut s, 80, 24, 0), Rect::new(0, 0, 80, 24), "H02");
}

/// H03 — whole. The session list is state the host cannot produce yet (the session index,
/// handoff §14.5); the menu itself is built here from the mock's rows.
#[test]
fn h03_resume_menu() {
    let mut s = home_screen(vec![ink(
        "3 sessions in this directory · last today 21:10.",
    )]);
    s.composer.text = "/resume".into();
    s.composer.cursor = 7;
    let session = |label: &str, description: &str, value: &str| PickerRow {
        label: label.into(),
        description: description.into(),
        value: value.into(),
        ..PickerRow::default()
    };
    s.picker = Some(Picker {
        title: Some("/resume".into()),
        count: "4 sessions · this directory".into(),
        groups: vec![PickerGroup {
            rows: vec![
                session(
                    "today 21:10",
                    "fix compaction boundary stall",
                    "claude/opus-5.5 · 214 items",
                ),
                session(
                    "today 17:42",
                    "split provider-http helpers (#47)",
                    "deepseek2/v4.1-flash · 96",
                ),
                session(
                    "yesterday 23:05",
                    "worker grants: add_tools",
                    "gpt/gpt-5.6-sol · 311",
                ),
                PickerRow {
                    available: false,
                    ..session(
                        "2026-09-20 14:02",
                        "websocket fallback notice",
                        "in use · pid 41210",
                    )
                },
            ],
            ..PickerGroup::default()
        }],
        label_width: 18,
        note: "resumes on its recorded model unless /model is set".into(),
        keys: "↑↓ move   ⏎ resume   esc".into(),
        ..Picker::default()
    });
    slab::assert_mock(&render(&mut s, 120, 40, 0), Rect::new(0, 0, 120, 40), "H03");
}

/// H04 — whole: a resumed session's painted history, then the resume facts; idle.
#[test]
fn h04_resumed() {
    let mut s = screen();
    with_ledger(&mut s);
    s.output = None;
    s.workers_ever_started = false;
    s.transcript.blocks = vec![
        ask(),
        reason(),
        prose(),
        read(),
        grep(),
        edit(),
        call(
            "shell",
            "cargo test -p p1-context boundary",
            ToolStatus::Ok,
            Some("exit 0 · 94 lines"),
            FaceBody::None,
        ),
        Block::Meta {
            text: "· resumed today 21:10 · 214 items · claude/opus-5.5:high".into(),
        },
        Block::Meta {
            text: "· 1 worker not restored on resume".into(),
        },
    ];
    slab::assert_mock(&render(&mut s, 120, 40, 0), Rect::new(0, 0, 120, 40), "H04");
}

// ---------- C: commands ----------

/// `[ask, read, edit, confirmed]`, idle (the menus' composer rows do not depend on the turn).
fn command_screen() -> Screen {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![ask(), read(), edit(), confirmed()];
    s
}

/// C01 — whole: `/` in the empty composer opens completion, docked above the composer.
#[test]
fn c01_command_completion() {
    let mut s = command_screen();
    s.open_completion();
    s.picker.as_mut().unwrap().set_value("/access", "full");
    slab::assert_mock(&render(&mut s, 120, 40, 0), Rect::new(0, 0, 120, 40), "C01");
}

fn model_row(label: &str, description: &str, value: &str) -> PickerRow {
    PickerRow {
        label: label.into(),
        description: description.into(),
        value: value.into(),
        efforts: description
            .split(' ')
            .filter(|w| ["low", "medium", "high", "max"].contains(w))
            .map(str::to_string)
            .collect(),
        ..PickerRow::default()
    }
}

fn model_menu() -> Picker {
    let group = |header: &str, right: &str, rows| PickerGroup {
        header: header.into(),
        right: right.into(),
        rows,
    };
    let borrowed = "oauth · borrowed";
    let mut sol = model_row("gpt/gpt-5.6-sol", "low medium high", borrowed);
    sol.effort = 1;
    Picker {
        title: Some("/model".into()),
        count: "11 models · 5 environments".into(),
        groups: vec![
            group(
                "CLAUDE",
                "anthropic-subscription",
                vec![
                    model_row(ME, "low medium high max", "current"),
                    model_row("claude/sonnet-5", "low medium high max", borrowed),
                    model_row("claude/opus-5", "low medium high", borrowed),
                ],
            ),
            group(
                "DEEPSEEK",
                "opencode-go-subscription",
                vec![model_row("deepseek/v4.1-flash", "default", "api key")],
            ),
            group(
                "DEEPSEEK2",
                "opencode-go-2-subscription",
                vec![model_row("deepseek2/v4.1-flash", "default", "api key")],
            ),
            group(
                "GLM",
                "glm-subscription",
                vec![PickerRow {
                    available: false,
                    ..model_row("glm/5.3", "default", "account exhausted")
                }],
            ),
            group(
                "GPT",
                "openai-codex-subscription",
                vec![
                    model_row("gpt/gpt-6-astra", "low medium high", borrowed),
                    sol,
                    model_row("gpt/gpt-5.6-terra", "low medium high", borrowed),
                    model_row("gpt/gpt-5.6-luna", "low medium", borrowed),
                ],
            ),
        ],
        selected: 7,
        label_width: 22,
        note: "switches at the next turn".into(),
        keys: "↑↓ move   ←→ effort   ⏎ switch   esc".into(),
        ..Picker::default()
    }
}

/// C02 — whole.
#[test]
fn c02_model_menu_120x40() {
    let mut s = command_screen();
    s.composer.text = "/model".into();
    s.composer.cursor = 6;
    s.picker = Some(model_menu());
    slab::assert_mock(&render(&mut s, 120, 40, 0), Rect::new(0, 0, 120, 40), "C02");
}

/// C03 — whole: the menu keeps its rows and the transcript shows its last four.
#[test]
fn c03_model_menu_80x24() {
    let mut s = command_screen();
    s.composer.text = "/model".into();
    s.composer.cursor = 6;
    s.picker = Some(model_menu());
    slab::assert_mock(&render(&mut s, 80, 24, 0), Rect::new(0, 0, 80, 24), "C03");
}

/// C04 — whole. Row 21 keeps the mock's key column and description, with a 2-cell gap after
/// `^G  PgUp PgDn  esc`; the description is therefore shifted two cells right.
#[test]
fn c04_help_command_output() {
    let mut s = screen();
    with_ledger(&mut s);
    let entry = |key: &str, text: &str| CommandRow::Entry {
        key: key.into(),
        text: text.into(),
    };
    s.transcript.blocks = vec![
        confirmed(),
        Block::CommandOutput(CommandOutput {
            command: "/help".into(),
            argument: String::new(),
            facts: "10 commands · 14 keys".into(),
            body: vec![
                CommandRow::Head("COMMANDS".into()),
                entry(
                    "/model [REF]",
                    "switch model or effort · claude/opus-5.5:high",
                ),
                entry("/effort LEVEL", "low medium high max"),
                entry(
                    "/goal [TEXT]",
                    "set or clear the session objective · ^G edits",
                ),
                entry("/focus [on|off]", "transcript only"),
                entry("/status", "session facts"),
                entry("/resume", "reopen a previous session"),
                entry("/access", "access and sandbox (fixed per process)"),
                entry("/models", "every model p1 can run"),
                entry("/exit", "quit"),
                CommandRow::Head("KEYS".into()),
                entry("⏎  ⌥⏎", "send · newline; while working: steer · follow-up"),
                entry("^C", "cancel the turn · quit when idle"),
                entry("^O  ^R", "open the latest fold · toggle reasoning"),
                entry("^Tab ^N  ^W  ^P", "pane mode · width · pin"),
                entry("^F  ^L", "focus the pane · pane overlay under 100 cols"),
                entry("^G  PgUp PgDn  esc", "goal · scroll · live tail / dismiss"),
            ],
        }),
    ];
    let buf = render(&mut s, 120, 40, 0);
    assert_rows_except(&buf, "C04", &[21]);
    let actual = row_text(&buf, 21);
    let expected = slab::mock("C04").text[21].clone();
    let key = "^G  PgUp PgDn  esc";
    let key_start = expected.find(key).unwrap();
    let description_start = key_start + key.len();
    assert_eq!(p1_tui::wrap::cell_width(key), 18);
    assert_eq!(
        p1_tui::wrap::cell_width(&actual[key_start..description_start]),
        18
    );
    assert!(actual[key_start + 18..].starts_with("  goal · scroll · live tail / dismiss"));
    let description = "goal · scroll · live tail / dismiss";
    assert!(actual[key_start + 20..].starts_with(description));
    let facts = "in                     38.1k";
    let expected_facts = expected.find(facts).unwrap();
    let actual_facts = actual.find(facts).unwrap();
    assert_eq!(
        p1_tui::wrap::cell_width(&expected[..expected_facts]),
        p1_tui::wrap::cell_width(&actual[..actual_facts]),
        "right-hand facts retain their mock column"
    );
    assert_eq!(&actual[actual_facts..], &expected[expected_facts..]);
}

const C05_GOAL: &str = "fix compaction boundary stall without changing the summary format";

/// C05 — whole except rows 34–35 of the transcript column. Lead decision: the composer
/// soft-wraps the long goal (tests/slab_composer.rs) where the mock cuts it with `…`, so the
/// composer is three rows (34–36) and grows upward; rows 34–35 are asserted as the wrap.
#[test]
fn c05_goal_editor() {
    let mut s = command_screen();
    // `^G` prefills the goal; the operator types the rest.
    s.edit_goal();
    for ch in C05_GOAL["fix compaction boundary stall".len()..].chars() {
        s.composer.insert(ch);
    }
    let buf = render(&mut s, 120, 40, 0);
    assert_rows_except(&buf, "C05", &[34, 35]);
    assert_cells(&buf, "C05", 34..36, 78..120);
    assert_eq!(
        row_text(&buf, 34)[..80].trim_end(),
        "    › /goal fix compaction boundary stall without changing the summary"
    );
    assert_eq!(row_text(&buf, 35)[..80].trim_end(), "      format");
    assert_eq!(s.cursor, Some((4 + "format".len() as u16 + 2, 35)));
}

// ---------- T: turns ----------

/// T01 — whole: expanded reasoning, streaming prose, the turn working row.
#[test]
fn t01_streaming() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![ask()];
    s.apply(&AgentEvent::TurnStarted, 0);
    s.apply(&AgentEvent::RequestStarted { request_index: 0 }, 0);
    s.apply(
        &AgentEvent::ReasoningDelta {
            text: "The wait only exists for the no-summary case. If a summary is ready the \
                   boundary can apply it directly; the hard-pressure branch predates the \
                   worker summary path."
                .into(),
        },
        0,
    );
    s.apply(
        &AgentEvent::TextDelta {
            text: "The hard-pressure wait blocks the turn boundary instead of applying the \
                   summary the worker already"
                .into(),
        },
        4_200,
    );
    s.transcript.toggle_reasoning();
    slab::assert_mock(
        &render(&mut s, 120, 40, 6_000),
        Rect::new(0, 0, 120, 40),
        "T01",
    );
}

/// T02 — whole except row 11, the preparing Block's header. Seams: a `ToolInputDelta` carries
/// no size, so the header has no `1.4 kB`; and §7.6 names the LAST input line as the target
/// (`+}`), where the mock shows the first (`*** Begin Patch`). Its body (the last three input
/// lines) and the working row `preparing · 2.4s  request 3` are asserted.
#[test]
fn t02_streaming_tool_arguments() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![ask(), reason(), prose(), read()];
    s.apply(&AgentEvent::TurnStarted, 0);
    s.apply(&AgentEvent::RequestStarted { request_index: 2 }, 0);
    s.apply(
        &AgentEvent::ToolInputDelta {
            call_id: "c-patch".into(),
            text: "*** Begin Patch\n+if let Some(summary) = ready {\n+    return \
                   self.apply_at_boundary(summary);\n+}"
                .into(),
        },
        1_000,
    );
    let buf = render(&mut s, 120, 40, 2_400);
    assert_rows_except(&buf, "T02", &[11]);
    assert_cells(&buf, "T02", 11..12, 78..120);
}

/// T03 — whole except row 20. Seam: there is no `retrying` phase (proposal §14.2 — the TUI
/// does not retry), so the working row reads `waiting`; its right side `request 8` is asserted.
#[test]
fn t03_notices() {
    let mut s = screen();
    with_ledger(&mut s);
    s.context = Some(ContextView {
        used: Some(31_000),
        window: 120_000,
        summarize_at: 96_000,
        parts: vec![],
    });
    s.transcript.blocks = vec![
        Block::Operator {
            text: "switch the websocket test to the sse path".into(),
            steering: false,
        },
        Block::Meta {
            text: "· transport: WebSocket unavailable (426) — using HTTP (SSE) for the rest of \
                   this session"
                .into(),
        },
        call(
            "read",
            "crates/p1-provider-openai/tests/websocket.rs",
            ToolStatus::Ok,
            Some("388 lines · 13.0 kB"),
            FaceBody::None,
        ),
        Block::MetaFacts {
            text: "· context summarized · 214 → 31 items".into(),
            facts: "in 18.2k · out 1.1k".into(),
        },
        Block::Operator {
            text: "keep the fallback notice text constant".into(),
            steering: true,
        },
        Block::Meta {
            text: "· 1 inbox message delivered".into(),
        },
        Block::WorkerReport(WorkerReport {
            id: "w2".into(),
            route: "deepseek2/v4.1-flash".into(),
            end: WorkerEnd::Done,
            elapsed: Some("2m10s".into()),
            cost_micro_usd: None,
            grants: vec![
                "read".into(),
                "edit".into(),
                "shell".into(),
                "finish".into(),
            ],
            line: "done · verified · cargo test -p p1-provider-http".into(),
        }),
        Block::Meta {
            text: "· Transport: chat stream ended before [DONE] · retry 1 of 3".into(),
        },
    ];
    s.apply(&AgentEvent::TurnStarted, 0);
    s.apply(&AgentEvent::RequestStarted { request_index: 7 }, 0);
    let buf = render(&mut s, 120, 40, 24_000);
    assert_rows_except(&buf, "T03", &[20]);
    assert_cells(&buf, "T03", 20..21, 64..120);
}

// ---------- E: endings ----------

/// E01 — whole except row 26. Lead decision: token counts follow `render::tokens`, so the
/// cancelled turn's `out 0.4k` is `out 400`.
#[test]
fn e01_cancelled() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![
        ask(),
        reason(),
        prose(),
        read(),
        grep(),
        edit(),
        call(
            "shell",
            "cargo test -p p1-context boundary",
            ToolStatus::Cancelled,
            None,
            FaceBody::None,
        ),
        Block::Notice(TurnNotice {
            kind: NoticeKind::Stopped,
            headline: "cancelled at 12.4s".into(),
            facts: vec![
                (
                    NoticeFact::Cost,
                    format!(
                        "request 3 · in {} · out {} · —",
                        p1_tui::render::tokens(14_200),
                        p1_tui::render::tokens(400)
                    ),
                ),
                (
                    NoticeFact::Kept,
                    "journal · edge.rs edited · dropped 1 queued".into(),
                ),
            ],
        }),
    ];
    let buf = render(&mut s, 120, 40, 0);
    assert_rows_except(&buf, "E01", &[26]);
    assert_cells(&buf, "E01", 26..27, 78..120);
    assert_eq!(
        row_text(&buf, 26)[..80].trim_end(),
        "      cost      request 3 · in 14.2k · out 400 · —"
    );
}

/// E02 — whole except row 23, the statusline. Seam: the mock's route has no effort levels and
/// its chip shows none; `StatusBar` has no "no effort" state (`None` is the adapter default,
/// `:default`).
#[test]
fn e02_provider_failed_80x24() {
    let mut s = screen();
    with_ledger(&mut s);
    s.statusbar.model = Some("deepseek2/v4.1-flash".into());
    s.statusbar.effort = None;
    s.transcript.blocks = vec![
        ask(),
        read(),
        edit(),
        Block::Notice(TurnNotice {
            kind: NoticeKind::Failed,
            headline: "account exhausted · deepseek2 (opencode-go-2-subscription) · not retried"
                .into(),
            facts: vec![
                (NoticeFact::Cost, "request 5 · in 22.9k · —".into()),
                (NoticeFact::Kept, "journal · 1 file changed".into()),
                (
                    NoticeFact::Next,
                    "/model to continue on another route".into(),
                ),
            ],
        }),
    ];
    let buf = render(&mut s, 80, 24, 0);
    assert_rows_except(&buf, "E02", &[23]);
}

// ---------- A: approvals ----------

fn permission(rows: &[(&str, &str)], grantable: bool) -> Approval {
    Approval::Permission(PermissionView {
        command: String::new(),
        rows: rows
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        grantable,
    })
}

/// A live turn with an approval on screen: the working row gives way to the inline Block.
fn approval_screen(blocks: Vec<Block>, tool: &str, approval: Approval) -> Screen {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = blocks;
    s.apply(&AgentEvent::TurnStarted, 0);
    s.approval = Some(approval);
    s.approval_tool = tool.into();
    s
}

fn with_command(approval: Approval, command: &str) -> Approval {
    match approval {
        Approval::Permission(mut view) => {
            view.command = command.into();
            Approval::Permission(view)
        }
        other => other,
    }
}

/// A01 — whole: the permission approval inline, the composer dim.
#[test]
fn a01_permission_inline() {
    let approval = with_command(
        permission(
            &[
                ("cwd", "~/dev/phaseone"),
                ("sandbox", "bubblewrap · writes: workspace"),
                ("network", "off"),
                ("effect", "runs a process"),
            ],
            true,
        ),
        "cargo build --release",
    );
    let mut s = approval_screen(vec![ask(), read(), edit()], "shell", approval);
    slab::assert_mock(&render(&mut s, 120, 40, 0), Rect::new(0, 0, 120, 40), "A01");
    assert_eq!(s.cursor, None, "a decision on screen takes the cursor away");
}

/// A02 — whole: the destructive floor, asked by a worker, a second request parked.
#[test]
fn a02_destructive_floor_80x24() {
    let approval = with_command(
        permission(
            &[
                ("from", "w3 · deepseek2/v4.1-flash"),
                ("cwd", "~/dev/phaseone"),
                ("sandbox", "bubblewrap · writes: workspace"),
                ("network", "off"),
                ("effect", "runs a process · destructive"),
            ],
            false,
        ),
        "rm -rf target/",
    );
    let mut s = approval_screen(vec![ask(), edit()], "shell", approval);
    s.approvals_waiting = 1;
    slab::assert_mock(&render(&mut s, 80, 24, 0), Rect::new(0, 0, 80, 24), "A02");
}

fn edge_diff(tool: &str, summary: &str, rows: Vec<DiffRow>) -> Approval {
    Approval::Diff(DiffView {
        tool: tool.into(),
        file: EDGE.into(),
        summary: summary.into(),
        position: (1, 1),
        rows,
        grantable: true,
    })
}

/// A03 — whole: `p` is faint in the decision band with no separate reason row.
#[test]
fn a03_edit_diff_inline() {
    let approval = edge_diff("edit", "replace exact string · once", diff_rows(0));
    let mut s = approval_screen(vec![ask(), reason(), prose(), read()], "edit", approval);
    let buf = render(&mut s, 120, 40, 0);
    slab::assert_mock(&buf, Rect::new(0, 0, 120, 40), "A03");
}

/// A04 — whole except rows 1, 2, 34 and 36. Seam: an approval carries ONE prepared diff
/// (`Approval::Diff(DiffView)`), so the review cannot page `1 of 3 files` (nor offer `tab next
/// file` in the hint row), count the whole file (`+12 −3` — it counts the rows shown) or say
/// `all 3 files` on the decision band.
#[test]
fn a04_full_review_three_files() {
    let rows = [diff_rows(0), diff_rows(40)].concat();
    let mut s = approval_screen(
        vec![ask(), read()],
        "apply_patch",
        edge_diff("apply_patch", "update file · hunk 1 of 1", rows),
    );
    s.review.open = true;
    let buf = render(&mut s, 120, 40, 0);
    assert_rows_except(&buf, "A04", &[1, 2, 34, 36]);
    assert!(row_text(&buf, 1).contains("! apply_patch crates/p1-context/src/edge.rs"));
    assert!(row_text(&buf, 34).contains(" y  allow once"));
    assert_eq!(s.cursor, None);
}

/// A05 — whole except row 1. Seam as A04: the summary row counts the rows shown (`+6 −6`),
/// not the call's `+3 −3`; the decision and hint rows are checked whole.
#[test]
fn a05_full_review_80x24() {
    let rows = [diff_rows(0), diff_rows(0)].concat();
    let mut s = approval_screen(
        vec![ask(), read()],
        "edit",
        edge_diff("edit", "replace exact string · once", rows),
    );
    s.review.open = true;
    let buf = render(&mut s, 80, 24, 0);
    assert_rows_except(&buf, "A05", &[1]);
    assert!(row_text(&buf, 21).contains(" y  allow once    a  session    p  project    n  deny"));
}

// ---------- W: workers ----------

/// W01 — whole except row 21 of the transcript column (the working row, as S04).
#[test]
fn w01_workers_every_state_pinned() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = worker_session();
    s.apply(&AgentEvent::TurnStarted, T0);
    with_workers_pane(&mut s);
    // A worker needing review self-pins WORKERS (§9.1).
    assert!(s.pinned);
    let buf = render(&mut s, 160, 48, NOW);
    assert_rows_except(&buf, "W01", &[21]);
    assert_cells(&buf, "W01", 21..22, 100..160);
}

/// W02 — whole except row 38, the statusline. The attached worker's own transcript is state
/// the host does not buffer yet (`child_event_sink` per worker, §9.5); it is built here from
/// the mock. Seam in row 38: the worker's route reports no effort, which `StatusBar` cannot
/// show (see E02).
#[test]
fn w02_attached_worker() {
    let mut s = screen();
    with_ledger(&mut s);
    with_compact_workers(&mut s);
    let mut t = transcript();
    t.blocks = vec![
        Block::Operator {
            text: "split provider-http helpers into p1-provider-http (#47)".into(),
            steering: false,
        },
        call(
            "read",
            "crates/p1-provider-openai-chat/src/lib.rs",
            ToolStatus::Ok,
            Some("612 lines · 21.4 kB"),
            FaceBody::None,
        ),
        call(
            "grep",
            "http_error_code crates/",
            ToolStatus::Ok,
            Some("4 hits · 3 files"),
            FaceBody::None,
        ),
    ];
    t.apply(&AgentEvent::TurnStarted, Some(T0));
    t.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "c-edit".into(),
                name: "edit".into(),
                input: ToolInput::Json("crates/p1-provider-http/src/retry.rs".into()),
            },
        },
        Some(T0),
    );
    s.attached = Some(AttachedWorker {
        id: "w2".into(),
        route: "deepseek2/v4.1-flash".into(),
        state: BlockState::Running,
        transcript: t,
    });
    let buf = render(&mut s, 120, 40, T0 + 300);
    assert_rows_except(&buf, "W02", &[38]);
    assert_eq!(s.cursor, None, "an attached transcript is read-only");
}

/// W03 — whole except rows 32–33 and 35–36 of the transcript column. Seam: there is no
/// stop-confirmation state (`x` on a focused worker asking `y stop  n keep`, §9.4), so neither
/// the docked question nor the dim `decide above` composer it brings exist; those rows' pane
/// cells are asserted.
#[test]
fn w03_stop_a_worker() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = worker_session();
    with_compact_workers(&mut s);
    let buf = render(&mut s, 120, 40, 0);
    assert_rows_except(&buf, "W03", &[32, 33, 35, 36]);
    assert_cells(&buf, "W03", 32..37, 78..120);
}

// ---------- P: pane ----------

/// P01 — whole except the OUTPUT source row (row 3) and body rows 16–20 of the pane. Seams:
/// no per-handle source fact exists, so the source row restates the call's Block (`shell ·
/// <target> · ✗ 11.4s · exit 101 · 94 lines`) where the mock says `✗ exit 101`; and the
/// mock's range claims `80–94 of 94` but lists only lines 80–89 (illustrative data), where the
/// pane shows 90–94 too.
#[test]
fn p01_output_pane_wide() {
    let mut s = screen();
    with_ledger(&mut s);
    s.pane_width = PaneWidth::Wide;
    s.transcript.blocks = vec![ask(), read(), grep(), shell_fail(), confirmed()];
    s.open_output(fold_output(79));
    let buf = render(&mut s, 120, 40, 0);
    assert_rows_except(&buf, "P01", &[3, 16, 17, 18, 19, 20]);
    for row in [3, 16, 17, 18, 19, 20] {
        assert_cells(&buf, "P01", row..row + 1, 0..62);
    }
    assert!(row_text(&buf, 3).contains("shell · cargo test -p p1-context"));
}

/// P02 — whole except row 19 of the transcript column: as S04, the mock's composer says a turn
/// runs while nothing in the transcript does, and §6.5 draws the working row.
#[test]
fn p02_peek_over_the_ledger() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![ask(), read(), grep(), shell_fail()];
    s.apply(&AgentEvent::TurnStarted, 0);
    s.peek(
        [
            "shell failed".into(),
            "exit 101 · hard_pressure_waits".into(),
        ],
        1_000,
    );
    let buf = render(&mut s, 120, 40, 1_000);
    assert_rows_except(&buf, "P02", &[19]);
    assert_cells(&buf, "P02", 19..20, 78..120);
}

/// P03 — everything but the pane. Seam: the DIFF pane mode has no session-diff seam (before-
/// images of touched files, §14.4), so DIFF is never available and the pane shows LEDGER.
#[test]
fn p03_diff_pane_planned() {
    let mut s = command_screen();
    let buf = render(&mut s, 160, 48, 0);
    assert_cells(&buf, "P03", 0..48, 0..102);
    assert_cells(&buf, "P03", 0..48, 158..160);
    assert_cells(&buf, "P03", 0..1, 0..160);
    assert_cells(&buf, "P03", 45..48, 0..160);
}

// ---------- Q, R, F ----------

/// Q01 — whole: steering and a follow-up queued above the composer.
#[test]
fn q01_queue() {
    let mut s = base();
    s.queued = VecDeque::from([
        Queued {
            follow_up: false,
            text: "use a VecDeque for the pending queue".into(),
        },
        Queued {
            follow_up: true,
            text: "then run clippy on p1-tui".into(),
        },
    ]);
    slab::assert_mock(
        &render(&mut s, 120, 40, NOW),
        Rect::new(0, 0, 120, 40),
        "Q01",
    );
}

/// R01 — whole except row 33, the scroll mark's counts: the mock's `14 new rows below` and
/// `row 1 of 47` are illustrative; this transcript is 48 rows, 32 shown under the mark, so the
/// mark says 16 below and `row 1 of 48`. Its layout is asserted in tests/slab_composer.rs.
#[test]
fn r01_scrolled_back() {
    let mut s = screen();
    with_ledger(&mut s);
    s.transcript.blocks = vec![
        ask(),
        reason(),
        prose(),
        read(),
        grep(),
        edit(),
        shell_fail(),
        confirmed(),
        edit(),
    ];
    start_shell(&mut s);
    s.scroll_top = Some(0);
    let buf = render(&mut s, 120, 40, NOW);
    assert_rows_except(&buf, "R01", &[33]);
    assert_cells(&buf, "R01", 33..34, 78..120);
    assert!(
        row_text(&buf, 33).contains("· 16 new rows below · ▸ shell running"),
        "{}",
        row_text(&buf, 33)
    );
    assert!(row_text(&buf, 33).contains("row 1 of 48   esc live tail"));
}

/// F01 — whole: `/focus` at 120×40 hides the pane and the empty composer.
#[test]
fn f01_focus_mode() {
    let mut s = base();
    s.focus_explicit = Some(true);
    slab::assert_mock(
        &render(&mut s, 120, 40, NOW),
        Rect::new(0, 0, 120, 40),
        "F01",
    );
}

/// Every screen id in grids.json has a test above.
#[test]
fn every_screen_mock_is_covered() {
    let covered = [
        "S01", "S02", "S03", "S04", "S05", "S06", "H01", "H02", "H03", "H04", "C01", "C02", "C03",
        "C04", "C05", "T01", "T02", "T03", "E01", "E02", "A01", "A02", "A03", "A04", "A05", "W01",
        "W02", "W03", "P01", "P02", "P03", "Q01", "R01", "F01",
    ];
    let screens: Vec<String> = slab::ids()
        .into_iter()
        .filter(|id| !id.starts_with("el-"))
        .collect();
    assert_eq!(screens.len(), covered.len());
    for id in screens {
        assert!(covered.contains(&id.as_str()), "{id} has no screen test");
    }
}
