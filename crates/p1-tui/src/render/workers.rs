//! The WORKERS pane (SPEC §5): one row per delegate, ordered by what needs
//! the operator — `!` awaiting review, `▪` running, `✓` done, `·` queued.
//! Each worker is a three-part block: glyph + name with its state right-
//! aligned, then `route · profile` with elapsed and cost right-aligned, then
//! detail lines at a single 3-space indent. Unknown cost renders `—`.
//!
//! p1's worker service has no review/apply flow yet (workers write the shared
//! workspace directly), so "needs review" maps to a finished worker whose
//! result the parent has not consumed — flagged in SPEC §9.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::band::{Band, Seg};
use crate::glyphs;
use crate::palette;

/// One worker as the pane shows it. All plain data — the host builds these
/// from its worker service, so `p1-tui` never names `p1-workers`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkerRow {
    pub id: String,
    /// The task summary (first line of the worker's task).
    pub summary: String,
    /// `route · profile` — two workers on one task are usually not the same
    /// model (SPEC §5).
    pub route: String,
    pub state: WorkerState,
    /// Preformatted elapsed (`0m48s`), when known.
    pub elapsed: Option<String>,
    /// Micro-USD when the route reports it; None renders `—`, never 0.
    pub cost_micro_usd: Option<u64>,
    /// Detail lines at the single 3-space indent (owned paths, last note).
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerState {
    Review,
    Running,
    Done,
    Queued,
}

impl WorkerState {
    fn glyph(self) -> char {
        match self {
            Self::Review => glyphs::APPROVAL,
            Self::Running => glyphs::WORKING,
            Self::Done => glyphs::DONE,
            Self::Queued => glyphs::PENDING,
        }
    }

    /// The glyph's colour per the fixed §2 colour column.
    fn fg(self) -> ratatui::style::Color {
        match self {
            Self::Review | Self::Running => palette::INK,
            Self::Done => palette::DIM,
            Self::Queued => palette::FAINT,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Review => "needs review",
            Self::Running => "running",
            Self::Done => "done",
            Self::Queued => "queued",
        }
    }

    fn rank(self) -> u8 {
        match self {
            Self::Review => 0,
            Self::Running => 1,
            Self::Done => 2,
            Self::Queued => 3,
        }
    }
}

/// Render the WORKERS pane on its 48-column grid: header count line, then the
/// blocks ordered by what needs the operator.
pub fn lines(workers: &[WorkerRow], grid: usize) -> Vec<Line<'static>> {
    let mut sorted: Vec<&WorkerRow> = workers.iter().collect();
    sorted.sort_by_key(|w| w.state.rank());
    let live = workers
        .iter()
        .filter(|w| w.state == WorkerState::Running)
        .count();
    let waiting = workers
        .iter()
        .filter(|w| matches!(w.state, WorkerState::Review | WorkerState::Queued))
        .count();
    let mut out = Vec::new();
    out.push(crate::grid::row(
        grid,
        "WORKERS",
        &format!("{live} live · {waiting} waiting"),
    ));
    for worker in sorted {
        out.push(Line::default());
        out.extend(worker_lines(worker, grid));
    }
    out
}

fn worker_lines(worker: &WorkerRow, grid: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // Line 1: glyph + name, state right-aligned.
    let name = format!("  {}", worker.summary);
    let state = worker.state.label();
    let pad = grid.saturating_sub(1 + name.chars().count() + state.len());
    out.push(Line::from(vec![
        Span::styled(
            worker.state.glyph().to_string(),
            Style::new().fg(worker.state.fg()),
        ),
        Span::styled(name, Style::new().fg(palette::INK)),
        Span::raw(" ".repeat(pad)),
        Span::styled(state, Style::new().fg(palette::DIM)),
    ]));
    // Line 2: route · profile, elapsed · cost right-aligned.
    let left = format!("   {}", worker.route);
    let elapsed = worker.elapsed.clone().unwrap_or_default();
    let cost = worker
        .cost_micro_usd
        .map(|micro| format!("${}.{:04}", micro / 1_000_000, (micro % 1_000_000) / 100))
        .unwrap_or_else(|| super::UNKNOWN.into());
    let right = if elapsed.is_empty() {
        cost
    } else {
        format!("{elapsed} · {cost}")
    };
    let pad = grid.saturating_sub(left.chars().count() + right.chars().count());
    out.push(Line::from(vec![
        Span::styled(left, Style::new().fg(palette::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::new().fg(palette::INK)),
    ]));
    // Detail lines: single 3-space indent.
    for detail in &worker.details {
        let shown: String = detail.chars().take(grid.saturating_sub(3)).collect();
        out.push(Line::styled(
            format!("   {shown}"),
            Style::new().fg(palette::DIM),
        ));
    }
    out
}

// ---------------------------------------------------------------------------
// SLAB Harness WORKERS (handoff §9.4): wide (4-row) and compact (2-row)
// blocks on `band::Band`, at the pane width. Kept beside `WorkerRow`/`lines`
// above (still what `screen.rs` draws — the composition stage wires this in
// later): the host has no grants/activity/task-first-line tap yet either
// (§14.3), so `WorkerBlock` is the richer shape those seams will fill.

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

/// `WORKERS` header counts (§9.4).
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
    pub state: BlockState,
    pub elapsed: Option<String>,
    /// `None` until workers carry a usage tap (§9.4); renders `—`.
    pub cost_micro_usd: Option<u64>,
    pub grants: String,
    /// The `↳` line: current activity (running) or the end line (settled).
    pub activity: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkersPane {
    pub header: WorkersHeader,
    pub workers: Vec<WorkerBlock>,
    /// The `↑ ↓`-selected worker's id; its row 1 gets the amber focus fill
    /// (§9.4, the Menu focused-row convention).
    pub focused: Option<String>,
}

/// Render the WORKERS pane: header, then one block per worker in state
/// order, separated by one blank BLOCK row, then the selection footer
/// (§9.4). `compact` selects the 2-row form (grid 30) over the wide 4-row
/// form (grid 48); the footer drops `x stop` in the compact form (mock-
/// verified: `el-workers-pane-38`).
pub fn render(pane: &WorkersPane, width: usize, compact: bool) -> Vec<Line<'static>> {
    let mut sorted: Vec<&WorkerBlock> = pane.workers.iter().collect();
    sorted.sort_by_key(|w| w.state.rank());
    let mut out = vec![header_line(&pane.header, width)];
    for worker in sorted {
        out.push(blank_row(width));
        let focused = pane.focused.as_deref() == Some(worker.id.as_str());
        if compact {
            out.extend(compact_block(worker, width, focused));
        } else {
            out.extend(wide_block(worker, width, focused));
        }
    }
    out.push(blank_row(width));
    out.push(footer_line(width, compact));
    out
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
    let mut right = vec![Seg::new(palette::INK, header.live.to_string())];
    match header.queued {
        Some(queued) => {
            right.push(Seg::new(palette::DIM, " live · ".to_string()));
            right.push(Seg::new(palette::INK, queued.to_string()));
            right.push(Seg::new(palette::DIM, " queued · pool ".to_string()));
        }
        None => right.push(Seg::new(palette::DIM, " live · pool ".to_string())),
    }
    right.push(Seg::new(palette::INK, header.pool.clone()));
    Band {
        bg: palette::BLOCK,
        left: vec![Seg::new(palette::DIM, "WORKERS".to_string())],
        right,
        width,
        pad: 4,
    }
    .render()
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

fn wide_block(worker: &WorkerBlock, width: usize, focused: bool) -> Vec<Line<'static>> {
    let elapsed = worker
        .elapsed
        .clone()
        .unwrap_or_else(|| super::UNKNOWN.into());
    vec![
        block_head(worker, width, focused),
        Band {
            bg: palette::BLOCK,
            left: vec![Seg::new(palette::DIM, format!("  {}", worker.route))],
            right: vec![
                Seg::new(palette::INK, elapsed),
                Seg::new(palette::DIM, " · ".to_string()),
                Seg::new(palette::INK, cost_text(worker.cost_micro_usd)),
            ],
            width,
            pad: 4,
        }
        .render(),
        Band {
            bg: palette::BLOCK,
            left: vec![
                Seg::new(palette::DIM, "  grants  ".to_string()),
                Seg::new(palette::INK, worker.grants.clone()),
            ],
            right: vec![],
            width,
            pad: 4,
        }
        .render(),
        Band {
            bg: palette::BLOCK,
            left: vec![
                Seg::new(palette::DIM, "  ↳ ".to_string()),
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
    ]
}

fn compact_block(worker: &WorkerBlock, width: usize, focused: bool) -> Vec<Line<'static>> {
    let elapsed = worker
        .elapsed
        .clone()
        .unwrap_or_else(|| super::UNKNOWN.into());
    vec![
        block_head(worker, width, focused),
        Band {
            bg: palette::BLOCK,
            left: vec![Seg::new(palette::DIM, format!("  {}", worker.route))],
            right: vec![Seg::new(palette::INK, elapsed)],
            width,
            pad: 4,
        }
        .render(),
    ]
}

fn footer_line(width: usize, compact: bool) -> Line<'static> {
    let text = if compact {
        "^F select   a attach"
    } else {
        "^F select   a attach   x stop"
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

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    fn workers() -> Vec<WorkerRow> {
        vec![
            WorkerRow {
                id: "w2".into(),
                summary: "split3b".into(),
                route: "deepseek · v4.1-flash".into(),
                state: WorkerState::Running,
                elapsed: Some("0m52s".into()),
                cost_micro_usd: Some(200),
                details: vec![
                    "crates/p1-provider-http/".into(),
                    "↳ writing route.rs".into(),
                ],
            },
            WorkerRow {
                id: "w1".into(),
                summary: "sandbox-read".into(),
                route: "deepseek · v4.1-flash".into(),
                state: WorkerState::Review,
                elapsed: Some("0m48s".into()),
                cost_micro_usd: Some(200),
                details: vec!["reject cred-dir ancestors".into()],
            },
            WorkerRow {
                id: "w3".into(),
                summary: "t1-measure".into(),
                route: "glm · 5.3".into(),
                state: WorkerState::Queued,
                elapsed: None,
                cost_micro_usd: None,
                details: vec![],
            },
        ]
    }

    #[test]
    fn ordered_by_what_needs_the_operator() {
        let lines = lines(&workers(), 48);
        let text: Vec<String> = lines
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        let expected = format!("WORKERS{}1 live · 2 waiting", " ".repeat(48 - 7 - 18));
        assert_eq!(text[0], expected);
        assert!(text[2].starts_with("!  sandbox-read"));
        assert!(text[2].ends_with("needs review"));
        assert!(text[6].starts_with("▪  split3b"));
        assert!(text[11].starts_with("·  t1-measure"));
        // Unknown cost renders —, and the queued worker has no elapsed.
        assert!(text[12].contains("—"));
    }
}
