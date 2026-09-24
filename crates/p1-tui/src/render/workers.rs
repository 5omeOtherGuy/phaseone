//! The WORKERS pane (handoff §9.4): one block per worker, ordered by what needs the operator —
//! needs review, running, failed and stalled, queued, done, cancelled, lost. The wide form
//! (grid 48) takes four rows per worker, the compact form (grid 30) two. Unknown cost renders
//! `—`. Plus the attach band (§9.5) that heads a worker's own transcript.

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::glyphs;
use crate::palette;

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
    let mut out = render_body(pane, width, compact);
    out.push(blank_row(width));
    out.push(footer_line(width, compact));
    out
}

/// Shared body without the live selection/action footer.
pub(crate) fn render_body(pane: &WorkersPane, width: usize, compact: bool) -> Vec<Line<'static>> {
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
