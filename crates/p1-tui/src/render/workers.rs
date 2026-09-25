//! The WORKERS pane (handoff §9.4): one block per worker, ordered by what needs the operator —
//! needs review, running, failed and stalled, queued, done, cancelled, lost. The wide form
//! (grid 48) takes 2–4 rows per worker, the compact form (grid 30) three. Unknown metrics render
//! `—`. Plus the attach band (§9.5) that heads a worker's own transcript.

use std::collections::BTreeMap;

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::band::{Band, Seg};
use crate::glyphs;
use crate::palette;
use crate::workflow::{
    StepState, WorkerActivity, WorkflowPhase, WorkflowRun, WorkflowStep, WorkflowTree, clock,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BlockState {
    NeedsReview,
    Running,
    Failed,
    Stalled,
    Queued,
    Done,
    /// Finished without a command tool — the parent must verify (§7.3 `finish`).
    DoneUnverified,
    Cancelled,
    /// Not restored on resume (ADR-0034).
    Lost,
}

impl BlockState {
    fn glyph(self) -> char {
        match self {
            Self::NeedsReview => glyphs::APPROVAL,
            Self::Running => glyphs::WORKING,
            Self::Failed | Self::Stalled => glyphs::FAILED,
            Self::Queued | Self::Cancelled | Self::Lost => glyphs::PENDING,
            Self::Done | Self::DoneUnverified => glyphs::DONE,
        }
    }

    fn fg(self) -> ratatui::style::Color {
        match self {
            Self::NeedsReview => palette::ATTN,
            Self::Running => palette::LIVE,
            Self::Failed | Self::Stalled => palette::FAIL,
            Self::Queued | Self::Cancelled | Self::Lost => palette::FAINT,
            Self::Done | Self::DoneUnverified => palette::OK,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::NeedsReview => "needs review",
            Self::Running => "running",
            Self::Failed => "failed",
            Self::Stalled => "stalled",
            Self::Queued => "queued",
            Self::Done => "done",
            Self::DoneUnverified => "done · not verified",
            Self::Cancelled => "cancelled",
            Self::Lost => "lost",
        }
    }

    /// Queued/cancelled/lost rows are de-emphasised throughout (mock-
    /// verified on `el-workers-pane@56`: the task and activity text go DIM,
    /// not INK, matching their FAINT glyph — the id and grants values stay
    /// INK, they are still facts, just about something not happening now).
    fn muted(self) -> bool {
        matches!(self, Self::Queued | Self::Cancelled | Self::Lost)
    }

    /// Order (§9.4): needs review, running, failed/stalled, queued, done,
    /// cancelled, lost. Ties (failed vs. stalled) keep the input order
    /// (`sort_by_key` is stable).
    fn rank(self) -> u8 {
        match self {
            Self::NeedsReview => 0,
            Self::Running => 1,
            Self::Failed | Self::Stalled => 2,
            Self::Queued => 3,
            Self::Done | Self::DoneUnverified => 4,
            Self::Cancelled => 5,
            Self::Lost => 6,
        }
    }
}

/// `WORKERS` header counts (§9.4). An empty `pool` (the worker service reports no pool size
/// yet) drops the `· pool` fact rather than showing it blank.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkersHeader {
    pub live: u64,
    pub queued: Option<u64>,
    pub pool: String,
}

/// One worker block. All plain data — the host builds these; `p1-tui` never
/// names `p1-workers` (§7.7).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerBlock {
    pub id: String,
    pub task: String,
    /// `env/profile`.
    pub route: String,
    /// The model actually answering; `None` means it is not known yet.
    pub model: Option<String>,
    pub state: BlockState,
    pub elapsed: Option<String>,
    /// `None` until workers carry a usage tap (§9.4); renders `—`.
    pub cost_micro_usd: Option<u64>,
    /// Tokens used so far; `None` means unknown, never zero.
    pub tokens: Option<u64>,
    /// The worker's context window; `None` means unknown.
    pub context_window: Option<u64>,
    pub grants: String,
    /// The `↳` line: current activity (running) or the end line (settled).
    pub activity: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkersPane {
    pub header: WorkersHeader,
    pub workers: Vec<WorkerBlock>,
    /// The `↑ ↓`-selected row's key — a worker's id, a run's id (`wf1`) or a step's
    /// key (`wf1/3`); its first row gets the amber focus fill (§9.4, the Menu
    /// focused-row convention).
    pub focused: Option<String>,
    /// Workflow runs (ADR-0074): while one exists the pane is their live tree.
    pub tree: WorkflowTree,
    /// What each worker is doing and its tool calls, from its own event stream.
    pub activity: BTreeMap<String, WorkerActivity>,
    /// The TUI's clock at this frame, for live elapsed times.
    pub now_ms: u64,
}

/// One selectable WORKERS row (phase rows are not selectable).
#[derive(Debug, Clone, Copy)]
pub enum Selectable<'a> {
    Run(&'a WorkflowRun),
    Step(&'a WorkflowStep),
    Worker(&'a WorkerBlock),
}

impl Selectable<'_> {
    /// The key `WorkersPane::focused` names this row by.
    pub fn key(&self) -> &str {
        match self {
            Self::Run(run) => &run.id,
            Self::Step(step) => &step.key,
            Self::Worker(worker) => &worker.id,
        }
    }
}

/// Render the WORKERS pane: header, then one block per worker in state
/// order, separated by one blank BLOCK row, then the selection footer
/// (§9.4). `compact` selects the 3-row form (grid 30) over the wide 2–4-row
/// form (grid 48); the footer drops `x stop` in the compact form (mock-
/// verified: `el-workers-pane-38`).
pub fn render(pane: &WorkersPane, width: usize, compact: bool) -> Vec<Line<'static>> {
    render_with(pane, width, compact, None)
}

/// Render WORKERS with an optional pending stop confirmation in the footer.
pub fn render_with(
    pane: &WorkersPane,
    width: usize,
    compact: bool,
    stop_pending: Option<&str>,
) -> Vec<Line<'static>> {
    render_in(pane, width, compact, stop_pending, None)
}

/// Render WORKERS into `height` rows when known: a workflow tree taller than that
/// collapses its ended phases (ADR-0074).
pub fn render_in(
    pane: &WorkersPane,
    width: usize,
    compact: bool,
    stop_pending: Option<&str>,
    height: Option<usize>,
) -> Vec<Line<'static>> {
    // The blank row and the footer below the body.
    let body_rows = height.map(|height| height.saturating_sub(2));
    let mut out = body(pane, width, compact, body_rows);
    out.push(blank_row(width));
    out.push(footer_line(pane, width, compact, stop_pending));
    out
}

/// Every selectable WORKERS row in display order (handoff §9.4, ADR-0074): each run's
/// header, its steps in phase order and a running step's worker, then the workers no
/// step references, in state order.
pub fn display_order(pane: &WorkersPane) -> Vec<Selectable<'_>> {
    let mut order = Vec::new();
    for run in &pane.tree.runs {
        order.push(Selectable::Run(run));
        for step in run.steps() {
            order.push(Selectable::Step(step));
            if let Some(worker) = step_block(pane, step) {
                order.push(Selectable::Worker(worker));
            }
        }
    }
    order.extend(flat_workers(pane).into_iter().map(Selectable::Worker));
    order
}

/// Workers no step references, in state order.
fn flat_workers(pane: &WorkersPane) -> Vec<&WorkerBlock> {
    let mut sorted: Vec<&WorkerBlock> = pane
        .workers
        .iter()
        .filter(|worker| !pane.tree.references(&worker.id))
        .collect();
    sorted.sort_by_key(|worker| worker.state.rank());
    sorted
}

/// The worker block shown under a step: only while the step runs.
fn step_block<'a>(pane: &'a WorkersPane, step: &WorkflowStep) -> Option<&'a WorkerBlock> {
    if !step.running() {
        return None;
    }
    let id = step.worker_id.as_deref()?;
    pane.workers.iter().find(|worker| worker.id == id)
}

/// Shared body without the live selection/action footer.
pub(crate) fn render_body(pane: &WorkersPane, width: usize, compact: bool) -> Vec<Line<'static>> {
    body(pane, width, compact, None)
}

fn body(
    pane: &WorkersPane,
    width: usize,
    compact: bool,
    rows: Option<usize>,
) -> Vec<Line<'static>> {
    if !pane.tree.is_empty() {
        return tree_body(pane, width, compact, rows);
    }
    let mut out = vec![header_line(&pane.header, width)];
    for worker in flat_workers(pane) {
        out.push(blank_row(width));
        out.extend(worker_block(pane, worker, width, compact));
    }
    out
}

fn worker_block(
    pane: &WorkersPane,
    worker: &WorkerBlock,
    width: usize,
    compact: bool,
) -> Vec<Line<'static>> {
    let focused = pane.focused.as_deref() == Some(worker.id.as_str());
    if compact {
        compact_block(worker, width, focused)
    } else {
        wide_block(worker, width, focused)
    }
}

fn blank_row(width: usize) -> Line<'static> {
    Band {
        bg: palette::BLOCK,
        left: vec![],
        right: vec![],
        width,
        pad: 4,
    }
    .render()
}

fn header_line(header: &WorkersHeader, width: usize) -> Line<'static> {
    let mut right = vec![
        Seg::new(palette::INK, header.live.to_string()),
        Seg::new(palette::DIM, " live"),
    ];
    if let Some(queued) = header.queued {
        right.push(Seg::new(palette::DIM, " · "));
        right.push(Seg::new(palette::INK, queued.to_string()));
        right.push(Seg::new(palette::DIM, " queued"));
    }
    if !header.pool.is_empty() {
        right.push(Seg::new(palette::DIM, " · pool "));
        right.push(Seg::new(palette::INK, header.pool.clone()));
    }
    Band {
        bg: palette::BLOCK,
        left: vec![Seg::new(palette::DIM, "WORKERS".to_string())],
        right,
        width,
        pad: 4,
    }
    .render()
}

/// The attach band (§9.5): row 0 of the transcript area while a worker's own transcript is on
/// screen — who, where, what state, and the two keys that leave or stop it.
pub fn attach_band(id: &str, route: &str, state: BlockState, width: usize) -> Line<'static> {
    Band {
        bg: palette::BLOCK_PLUS,
        left: vec![
            Seg::new(palette::DIM, format!("{} ", glyphs::NESTED)),
            Seg::new(palette::INK, format!("attached {id}")),
            Seg::new(palette::DIM, format!(" · {route} · {}", state.label())),
        ],
        right: vec![Seg::new(palette::FAINT, "esc detach   x stop")],
        width,
        pad: 2,
    }
    .render()
}

/// Prompt lines an opened step shows folded (ADR-0074).
const PROMPT_FOLDED: usize = 3;

/// The rows an opened workflow step puts over its worker's transcript (ADR-0074): a stats
/// band — status · model · phase · attempts · elapsed · tokens · tool calls — then the
/// step's prompt, its first three lines folded (`p` expands it, wrapped). Empty when the
/// step is not in the tree.
pub fn step_band(
    pane: &WorkersPane,
    key: &str,
    expanded: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let Some((_, step)) = pane.tree.step(key) else {
        return Vec::new();
    };
    let phase = pane
        .tree
        .phase_of(key)
        .filter(|phase| !phase.is_empty())
        .unwrap_or(super::UNKNOWN);
    let elapsed = step
        .elapsed_ms(pane.now_ms)
        .map(clock)
        .unwrap_or_else(|| super::UNKNOWN.into());
    let tokens = step_tokens(pane, step)
        .flatten()
        .map(super::tokens)
        .unwrap_or_else(|| super::UNKNOWN.into());
    let calls = step_calls(pane, step)
        .map(|calls| calls.to_string())
        .unwrap_or_else(|| super::UNKNOWN.into());
    let status = if step.replayed {
        format!("replayed · {}", step.state.word())
    } else {
        step.state.word().to_string()
    };
    let band = |left: Vec<Seg>, right: Vec<Seg>| {
        Band {
            bg: palette::BLOCK_PLUS,
            left,
            right,
            width,
            pad: 2,
        }
        .render()
    };
    let (glyph, fg) = step_glyph(step);
    let mut out = vec![band(
        vec![
            Seg::new(fg, format!("{glyph} ")),
            Seg::new(palette::INK, step.name().to_string()),
            Seg::new(
                palette::DIM,
                format!(
                    " · {status} · {} · {phase} · ×{} · {elapsed} · {tokens} tok · {calls} calls",
                    step.model, step.attempts
                ),
            ),
        ],
        vec![],
    )];
    let room = width.saturating_sub(4).max(1);
    let lines: Vec<String> = if expanded {
        crate::wrap::wrap_paragraphs(&step.prompt, room)
    } else {
        step.prompt
            .lines()
            .take(PROMPT_FOLDED)
            .map(str::to_string)
            .collect()
    };
    for line in lines {
        out.push(band(vec![Seg::new(palette::DIM, line)], vec![]));
    }
    let more = step.prompt.lines().count().saturating_sub(PROMPT_FOLDED);
    let hint = match (expanded, more) {
        (false, 0) => None,
        (false, more) => Some(format!("… {more} more lines · p expands")),
        (true, _) if more > 0 => Some("p folds".to_string()),
        (true, _) => None,
    };
    if let Some(hint) = hint {
        out.push(band(vec![Seg::new(palette::FAINT, hint)], vec![]));
    }
    out
}

/// Row 1 of a block: glyph + id + task, state right — or, focused, the Menu
/// convention (amber fill, ground text, glyph `▸`, §6.10/§9.4).
fn block_head(worker: &WorkerBlock, width: usize, focused: bool) -> Line<'static> {
    let id = format!(" {}    ", worker.id);
    if focused {
        Band {
            bg: palette::AMBER_FILL,
            left: vec![
                Seg::new(palette::ON_FILL, glyphs::TOOL.to_string()),
                Seg::new(palette::ON_FILL, id),
                Seg::new(palette::ON_FILL, worker.task.clone()),
            ],
            right: vec![Seg::new(palette::ON_FILL, worker.state.label().to_string())],
            width,
            pad: 4,
        }
        .render()
    } else {
        let task_fg = if worker.state.muted() {
            palette::DIM
        } else {
            palette::INK
        };
        Band {
            bg: palette::BLOCK,
            left: vec![
                Seg::new(worker.state.fg(), worker.state.glyph().to_string()),
                Seg::new(palette::INK, id),
                Seg::new(task_fg, worker.task.clone()),
            ],
            right: vec![Seg::new(palette::DIM, worker.state.label().to_string())],
            width,
            pad: 4,
        }
        .render()
    }
}

fn cost_text(cost_micro_usd: Option<u64>) -> String {
    cost_micro_usd
        .map(|micro| format!("${}.{:04}", micro / 1_000_000, (micro % 1_000_000) / 100))
        .unwrap_or_else(|| super::UNKNOWN.into())
}

const LEAD_INDENT: &str = "  ";

fn lead(worker: &WorkerBlock, right_cells: usize, width: usize) -> Vec<Seg> {
    let Some(model) = &worker.model else {
        return vec![Seg::new(
            palette::DIM,
            format!("{LEAD_INDENT}{}", worker.route),
        )];
    };
    let mut segments = vec![Seg::new(palette::INK, format!("{LEAD_INDENT}{model}"))];
    let room = width.saturating_sub(8).saturating_sub(right_cells + 2);
    let suffix = format!(" · {}", worker.route);
    if worker.route != *model
        && crate::wrap::cell_width(LEAD_INDENT)
            + crate::wrap::cell_width(model)
            + crate::wrap::cell_width(&suffix)
            <= room
    {
        segments.push(Seg::new(palette::DIM, suffix));
    }
    segments
}

fn wide_block(worker: &WorkerBlock, width: usize, focused: bool) -> Vec<Line<'static>> {
    let elapsed = worker
        .elapsed
        .clone()
        .unwrap_or_else(|| super::UNKNOWN.into());
    let tok = worker
        .tokens
        .map(super::tokens)
        .unwrap_or_else(|| super::UNKNOWN.into());
    let ctx = worker
        .context_window
        .map(super::tokens)
        .unwrap_or_else(|| super::UNKNOWN.into());
    let right = vec![
        Seg::new(palette::INK, tok),
        Seg::new(palette::DIM, "/"),
        Seg::new(palette::INK, ctx),
        Seg::new(palette::DIM, " · "),
        Seg::new(palette::INK, elapsed),
        Seg::new(palette::DIM, " · "),
        Seg::new(palette::INK, cost_text(worker.cost_micro_usd)),
    ];
    let right_cells: usize = right.iter().map(|s| crate::wrap::cell_width(&s.text)).sum();
    let mut out = vec![
        block_head(worker, width, focused),
        Band {
            bg: palette::BLOCK,
            left: lead(worker, right_cells, width),
            right,
            width,
            pad: 4,
        }
        .render(),
    ];
    if !worker.grants.is_empty() {
        out.push(
            Band {
                bg: palette::BLOCK,
                left: vec![
                    Seg::new(palette::DIM, "  grants  "),
                    Seg::new(palette::INK, worker.grants.clone()),
                ],
                right: vec![],
                width,
                pad: 4,
            }
            .render(),
        );
    }
    if !worker.activity.is_empty() {
        out.push(
            Band {
                bg: palette::BLOCK,
                left: vec![
                    Seg::new(palette::DIM, "  ↳ "),
                    Seg::new(
                        if worker.state.muted() {
                            palette::DIM
                        } else {
                            palette::INK
                        },
                        worker.activity.clone(),
                    ),
                ],
                right: vec![],
                width,
                pad: 4,
            }
            .render(),
        );
    }
    out
}

fn compact_block(worker: &WorkerBlock, width: usize, focused: bool) -> Vec<Line<'static>> {
    let elapsed = worker
        .elapsed
        .clone()
        .unwrap_or_else(|| super::UNKNOWN.into());
    let cost = cost_text(worker.cost_micro_usd);
    let right_cells = crate::wrap::cell_width(&elapsed);
    let tok = worker
        .tokens
        .map(super::tokens)
        .unwrap_or_else(|| super::UNKNOWN.into());
    let ctx = worker
        .context_window
        .map(super::tokens)
        .unwrap_or_else(|| super::UNKNOWN.into());
    vec![
        block_head(worker, width, focused),
        Band {
            bg: palette::BLOCK,
            left: lead(worker, right_cells, width),
            right: vec![Seg::new(palette::INK, elapsed)],
            width,
            pad: 4,
        }
        .render(),
        Band {
            bg: palette::BLOCK,
            left: vec![
                Seg::new(palette::DIM, "  tokens "),
                Seg::new(palette::INK, tok),
                Seg::new(palette::DIM, "/"),
                Seg::new(palette::INK, ctx),
            ],
            right: vec![Seg::new(palette::INK, cost)],
            width,
            pad: 4,
        }
        .render(),
    ]
}

fn footer_line(
    pane: &WorkersPane,
    width: usize,
    compact: bool,
    stop_pending: Option<&str>,
) -> Line<'static> {
    if let Some(id) = stop_pending {
        let text = if pane.tree.run(id).is_some() {
            format!("cancel {id}?   y cancel   n keep")
        } else {
            format!("stop {id}?   y stop   n keep")
        };
        return Band {
            bg: palette::AMBER_FILL,
            left: vec![Seg::new(palette::ON_FILL, text)],
            right: vec![],
            width,
            pad: 4,
        }
        .render();
    }
    let text = match (pane.tree.is_empty(), compact) {
        (true, true) => "^F select   a attach",
        (true, false) => "^F select   a attach   x stop",
        (false, true) => "^F select   ⏎ open",
        (false, false) => "^F select   ⏎ open   a attach   x stop",
    };
    Band {
        bg: palette::BLOCK,
        left: vec![Seg::new(palette::FAINT, text.to_string())],
        right: vec![],
        width,
        pad: 4,
    }
    .render()
}

// ------------------------------------------------------------ workflow tree (ADR-0074)

/// A width at and above which a step shows its second line (activity and metrics).
const STEP_DETAIL_WIDTH: usize = 48;
/// A width at and above which the tree spells counts out and shows cost.
const TREE_WIDE: usize = 56;

/// The tree: header, each run (header, notes, phases, steps, running workers), then the
/// workers no step references under a `workers` group. `rows` is the body's height when
/// known: ended phases collapse, oldest first, until the body fits — never the phase a
/// running run is in, one with a running step, or the selected step's.
fn tree_body(
    pane: &WorkersPane,
    width: usize,
    compact: bool,
    rows: Option<usize>,
) -> Vec<Line<'static>> {
    let mut collapsed: Vec<Vec<bool>> = pane
        .tree
        .runs
        .iter()
        .map(|run| vec![false; run.phases.len()])
        .collect();
    let mut out = tree_lines(pane, width, compact, &collapsed);
    let Some(rows) = rows else {
        return out;
    };
    let selected = pane.focused.as_deref();
    for (r, run) in pane.tree.runs.iter().enumerate() {
        for (p, phase) in run.phases.iter().enumerate() {
            if out.len() <= rows {
                return out;
            }
            let holds_selection = selected.is_some_and(|key| {
                phase.steps.iter().any(|step| {
                    step.key == key || step.worker_id.as_deref() == Some(key) && step.running()
                })
            });
            if phase.name.is_empty()
                || phase.steps.is_empty()
                || !run.phase_ended(p)
                || holds_selection
            {
                continue;
            }
            collapsed[r][p] = true;
            out = tree_lines(pane, width, compact, &collapsed);
        }
    }
    out
}

fn tree_lines(
    pane: &WorkersPane,
    width: usize,
    compact: bool,
    collapsed: &[Vec<bool>],
) -> Vec<Line<'static>> {
    let mut out = vec![header_line(&pane.header, width)];
    for (run, folded) in pane.tree.runs.iter().zip(collapsed) {
        out.push(blank_row(width));
        out.extend(run_rows(pane, run, width, compact));
        for (index, phase) in run.phases.iter().enumerate() {
            if !phase.name.is_empty() {
                out.push(phase_row(pane, run, index, folded[index], width, compact));
            }
            if folded[index] {
                continue;
            }
            for step in &phase.steps {
                out.extend(step_rows(pane, step, width));
                if let Some(worker) = step_block(pane, step) {
                    let inner = width.saturating_sub(TREE_INDENT);
                    out.extend(
                        worker_block(pane, worker, inner, compact)
                            .into_iter()
                            .map(indent),
                    );
                }
            }
        }
    }
    let flat = flat_workers(pane);
    if !flat.is_empty() {
        out.push(blank_row(width));
        out.push(row(width, vec![Seg::new(palette::DIM, "workers")], vec![]));
        for worker in flat {
            out.push(blank_row(width));
            out.extend(worker_block(pane, worker, width, compact));
        }
    }
    out
}

/// How far a step's worker block sits in: one level.
const TREE_INDENT: usize = 2;

fn indent(line: Line<'static>) -> Line<'static> {
    let mut spans = vec![Span::styled(
        " ".repeat(TREE_INDENT),
        Style::new().bg(palette::BLOCK),
    )];
    spans.extend(line.spans);
    Line::from(spans)
}

fn row(width: usize, left: Vec<Seg>, right: Vec<Seg>) -> Line<'static> {
    Band {
        bg: palette::BLOCK,
        left,
        right,
        width,
        pad: 4,
    }
    .render()
}

/// A selected row: the Menu convention (amber fill, ground text, glyph `▸`, §6.10/§9.4).
fn focused_row(width: usize, left: Vec<String>, right: Vec<String>) -> Line<'static> {
    Band {
        bg: palette::AMBER_FILL,
        left: left
            .into_iter()
            .map(|text| Seg::new(palette::ON_FILL, text))
            .collect(),
        right: right
            .into_iter()
            .map(|text| Seg::new(palette::ON_FILL, text))
            .collect(),
        width,
        pad: 4,
    }
    .render()
}

/// A sum over steps whose parts may be unknown: the known part, `+?` when some part is
/// unknown, `—` when nothing is known — never a 0 for an unknown.
#[derive(Debug, Default, Clone, Copy)]
struct Sum {
    known: u64,
    any_known: bool,
    any_unknown: bool,
}

impl Sum {
    fn add(&mut self, part: Option<u64>) {
        match part {
            Some(value) => {
                self.known += value;
                self.any_known = true;
            }
            None => self.any_unknown = true,
        }
    }

    fn text(self, show: impl Fn(u64) -> String) -> String {
        match (self.any_known, self.any_unknown) {
            (false, _) => super::UNKNOWN.into(),
            (true, false) => show(self.known),
            (true, true) => format!("{}+?", show(self.known)),
        }
    }
}

fn step_worker<'a>(pane: &'a WorkersPane, step: &WorkflowStep) -> Option<&'a WorkerBlock> {
    let id = step.worker_id.as_deref()?;
    pane.workers.iter().find(|worker| worker.id == id)
}

/// A step's tokens: its worker's, unknown while the worker row does not have them.
/// `None` for a step that ran no worker (replayed, refused): it is not a part.
fn step_tokens(pane: &WorkersPane, step: &WorkflowStep) -> Option<Option<u64>> {
    step.worker_id.as_ref()?;
    Some(step_worker(pane, step).and_then(|worker| worker.tokens))
}

/// A step's tool calls: the TUI's own count on its worker's stream.
fn step_calls(pane: &WorkersPane, step: &WorkflowStep) -> Option<u64> {
    let id = step.worker_id.as_deref()?;
    Some(
        pane.activity
            .get(id)
            .map_or(0, |activity| activity.tool_calls),
    )
}

fn sums<'a>(pane: &WorkersPane, steps: impl Iterator<Item = &'a WorkflowStep>) -> (Sum, Sum) {
    let (mut tokens, mut calls) = (Sum::default(), Sum::default());
    for step in steps {
        if let Some(part) = step_tokens(pane, step) {
            tokens.add(part);
        }
        if let Some(part) = step_calls(pane, step) {
            calls.add(Some(part));
        }
    }
    (tokens, calls)
}

fn calls_text(calls: Sum) -> String {
    format!("{} calls", calls.text(|n| n.to_string()))
}

fn run_rows(
    pane: &WorkersPane,
    run: &WorkflowRun,
    width: usize,
    compact: bool,
) -> Vec<Line<'static>> {
    let wide = !compact && width >= TREE_WIDE;
    let phase = run
        .current_phase()
        .map(|phase| phase.name.clone())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| super::UNKNOWN.into());
    let resumed = run
        .resumed_from
        .as_ref()
        .map(|from| format!(" {} {from}", glyphs::REPLAYED))
        .unwrap_or_default();
    let elapsed = clock(run.elapsed_ms(pane.now_ms));
    let total = run.total();
    let right = if wide {
        format!("{total} steps · {elapsed}")
    } else {
        elapsed
    };
    let mut out = Vec::new();
    if pane.focused.as_deref() == Some(run.id.as_str()) {
        out.push(focused_row(
            width,
            vec![
                format!("{} {}", glyphs::TOOL, run.id),
                format!(" · {phase}{resumed}"),
            ],
            vec![right],
        ));
    } else {
        let (glyph, fg) = match run.ended.as_ref().map(|ended| ended.outcome.as_str()) {
            None => (glyphs::WORKING, palette::LIVE),
            Some("completed") => (glyphs::DONE, palette::OK),
            Some("completed_with_issues") => (glyphs::DONE, palette::ATTN),
            Some("cancelled") => (glyphs::STOPPED, palette::FAINT),
            Some(_) => (glyphs::FAILED, palette::FAIL),
        };
        out.push(row(
            width,
            vec![
                Seg::new(fg, glyph.to_string()),
                Seg::new(palette::INK, format!(" {}", run.id)),
                Seg::new(palette::DIM, " · "),
                Seg::new(palette::INK, phase),
                Seg::new(palette::DIM, resumed),
            ],
            vec![Seg::new(palette::DIM, right)],
        ));
    }
    let (running, done, failed) = (
        run.count(StepState::Running),
        run.count(StepState::Done),
        run.count(StepState::Failed),
    );
    let counts = if wide {
        vec![
            Seg::new(palette::INK, format!("  {running}")),
            Seg::new(palette::DIM, " running · "),
            Seg::new(palette::INK, done.to_string()),
            Seg::new(palette::DIM, " done · "),
            Seg::new(palette::INK, failed.to_string()),
            Seg::new(palette::DIM, " failed · "),
            Seg::new(palette::INK, run.queued.to_string()),
            Seg::new(palette::DIM, " queued"),
        ]
    } else {
        vec![
            Seg::new(palette::DIM, format!("  {}", glyphs::WORKING)),
            Seg::new(palette::INK, running.to_string()),
            Seg::new(palette::DIM, format!(" {}", glyphs::DONE)),
            Seg::new(palette::INK, done.to_string()),
            Seg::new(palette::DIM, format!(" {}", glyphs::FAILED)),
            Seg::new(palette::INK, failed.to_string()),
            Seg::new(palette::DIM, format!(" {}", glyphs::PENDING)),
            Seg::new(palette::INK, run.queued.to_string()),
            Seg::new(palette::DIM, " of "),
            Seg::new(palette::INK, total.to_string()),
        ]
    };
    out.push(row(width, counts, vec![]));
    let (tokens, calls) = sums(pane, run.steps());
    out.push(row(
        width,
        vec![
            Seg::new(palette::DIM, "  tokens "),
            Seg::new(palette::INK, tokens.text(super::tokens)),
        ],
        vec![Seg::new(palette::DIM, calls_text(calls))],
    ));
    let mut note = |text: String| {
        out.push(row(
            width,
            vec![Seg::new(palette::DIM, format!("  {text}"))],
            vec![],
        ));
    };
    if let Some(ended) = &run.ended {
        match ended
            .error
            .as_deref()
            .and_then(|error| error.lines().next())
        {
            Some(error) => note(format!("{} — {error}", ended.outcome)),
            None => note(ended.outcome.clone()),
        }
    }
    if let Some(error) = &run.note {
        note(format!(
            "{} {}",
            glyphs::FAILED,
            error.lines().next().unwrap_or_default()
        ));
    }
    if let Some(log) = &run.last_log {
        note(log.lines().next().unwrap_or_default().to_string());
    }
    out
}

fn phase_row(
    pane: &WorkersPane,
    run: &WorkflowRun,
    index: usize,
    folded: bool,
    width: usize,
    compact: bool,
) -> Line<'static> {
    let phase: &WorkflowPhase = &run.phases[index];
    let current = run.running() && run.current == Some(index);
    // The phase a running run is in also owns the jobs not started yet.
    let known = phase.steps.len() + if current { run.queued } else { 0 };
    let glyph = if folded {
        glyphs::PHASE_FOLDED
    } else {
        glyphs::PHASE_OPEN
    };
    let (glyph_fg, name_fg) = if current {
        (palette::LIVE, palette::INK)
    } else {
        (palette::DIM, palette::DIM)
    };
    let elapsed = clock(run.phase_elapsed_ms(index, pane.now_ms));
    let right = if !compact && width >= TREE_WIDE {
        let (tokens, calls) = sums(pane, phase.steps.iter());
        vec![Seg::new(
            palette::DIM,
            format!(
                "{} · {} · {elapsed}",
                tokens.text(super::tokens),
                calls_text(calls)
            ),
        )]
    } else {
        vec![Seg::new(palette::DIM, elapsed)]
    };
    row(
        width,
        vec![
            Seg::new(glyph_fg, format!("  {glyph} ")),
            Seg::new(name_fg, phase.name.clone()),
            Seg::new(palette::INK, format!(" {}/{known}", phase.done())),
        ],
        right,
    )
}

fn step_glyph(step: &WorkflowStep) -> (char, ratatui::style::Color) {
    if step.replayed {
        return (glyphs::REPLAYED, palette::DIM);
    }
    match step.state {
        StepState::Running => (glyphs::WORKING, palette::LIVE),
        StepState::Done => (glyphs::DONE, palette::OK),
        StepState::Failed => (glyphs::FAILED, palette::FAIL),
        StepState::Blocked | StepState::Cancelled => (glyphs::STOPPED, palette::FAINT),
    }
}

/// What a step's worker is doing now, or how the step ended.
fn step_activity(pane: &WorkersPane, step: &WorkflowStep) -> String {
    if !step.running() {
        return step.outcome();
    }
    step.worker_id
        .as_deref()
        .and_then(|id| pane.activity.get(id))
        .map(|activity| activity.now.clone())
        .filter(|now| !now.is_empty())
        .unwrap_or_else(|| "thinking".into())
}

fn step_rows(pane: &WorkersPane, step: &WorkflowStep, width: usize) -> Vec<Line<'static>> {
    let detail = width >= STEP_DETAIL_WIDTH;
    let elapsed = step
        .elapsed_ms(pane.now_ms)
        .map(clock)
        .unwrap_or_else(|| super::UNKNOWN.into());
    let right = if step.attempts > 1 {
        format!("×{} · {elapsed}", step.attempts)
    } else {
        elapsed
    };
    let activity = step_activity(pane, step);
    // Below the detail width the activity is a trailing suffix of the one row.
    let suffix = if detail {
        format!(" · {}", step.model)
    } else {
        format!(" · {activity}")
    };
    let mut out = Vec::new();
    if pane.focused.as_deref() == Some(step.key.as_str()) {
        out.push(focused_row(
            width,
            vec![format!("    {} {}", glyphs::TOOL, step.name()), suffix],
            vec![right],
        ));
    } else {
        let (glyph, fg) = step_glyph(step);
        out.push(row(
            width,
            vec![
                Seg::new(fg, format!("    {glyph} ")),
                Seg::new(
                    if step.running() {
                        palette::INK
                    } else {
                        palette::DIM
                    },
                    step.name().to_string(),
                ),
                Seg::new(palette::DIM, suffix),
            ],
            vec![Seg::new(palette::DIM, right)],
        ));
    }
    if !detail {
        return out;
    }
    let mut right = Vec::new();
    if step.worker_id.is_some() {
        let worker = step_worker(pane, step);
        let tok = worker
            .and_then(|worker| worker.tokens)
            .map(super::tokens)
            .unwrap_or_else(|| super::UNKNOWN.into());
        let ctx = worker
            .and_then(|worker| worker.context_window)
            .map(super::tokens)
            .unwrap_or_else(|| super::UNKNOWN.into());
        let calls = step_calls(pane, step).unwrap_or_default();
        right.push(Seg::new(palette::INK, tok));
        right.push(Seg::new(palette::DIM, "/"));
        right.push(Seg::new(palette::INK, ctx));
        right.push(Seg::new(palette::DIM, format!(" · {calls} calls")));
        if width >= TREE_WIDE {
            right.push(Seg::new(palette::DIM, " · "));
            right.push(Seg::new(
                palette::INK,
                cost_text(worker.and_then(|worker| worker.cost_micro_usd)),
            ));
        }
    }
    out.push(row(
        width,
        vec![
            Seg::new(palette::DIM, format!("      {} ", glyphs::NESTED)),
            Seg::new(
                if step.running() {
                    palette::INK
                } else {
                    palette::DIM
                },
                activity,
            ),
        ],
        right,
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_header_drops_what_is_not_known() {
        let header = WorkersHeader {
            live: 2,
            queued: None,
            pool: String::new(),
        };
        let text = header_line(&header, 38).to_string();
        assert_eq!(text.trim_end(), "    WORKERS                 2 live");
    }
}
