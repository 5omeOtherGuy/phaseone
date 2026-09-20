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
    let left = format!("{}  {}", worker.state.glyph(), worker.summary);
    let state = worker.state.label();
    let pad = grid.saturating_sub(left.chars().count() + state.len());
    out.push(Line::from(vec![
        Span::styled(left, Style::new().fg(palette::INK)),
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
