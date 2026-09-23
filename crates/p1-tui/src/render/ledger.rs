//! The LEDGER pane (handoff §9.2): GOAL, SESSION, CONTEXT, WORKSPACE, SPEND, WORKERS and
//! FOLDS, one `band::Band` per row at the pane width (pad 4, grid = width − 8). Unknown values
//! render `—`, never 0; a section with no data at all is absent, not empty. `render` drops
//! sections lowest-priority-first when the pane is too short for all of them.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::band::{Band, Seg};
use crate::palette;
use crate::wrap::cell_width;

use super::{UNKNOWN, cost_string, tokens};

/// Everything the LEDGER pane can show, as plain data (§9.1/§9.2). Each
/// section is absent (not empty) until its data exists; `render` decides,
/// from `height`, which absent-vs-present sections survive a short pane.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LedgerPane {
    pub goal: Option<String>,
    pub session: Option<SessionView>,
    pub context: Option<ContextView>,
    pub workspace: Option<WorkspaceView>,
    pub spend: Option<LedgerSpend>,
    pub workers: Option<WorkersSummary>,
    pub folds: Vec<FoldRef>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionView {
    pub model: String,
    pub effort: String,
    pub access: String,
    pub sandbox: String,
}

/// The CONTEXT section (§9.2). `used` is `None` before the first response
/// (header `— / window`, empty bar, percent `—`); `parts` stays empty until
/// the host's context-stats seam exists (§14.10) — an empty `Vec` omits the
/// breakdown rows, it never renders as `—` rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextView {
    pub used: Option<u64>,
    pub window: u64,
    pub summarize_at: u64,
    pub parts: Vec<ContextPartView>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextPartView {
    pub label: String,
    pub count: Option<u64>,
    pub tokens: u64,
}

/// TASK becomes WORKSPACE (§13 #14: p1 has no task ids).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkspaceView {
    pub files: Option<u64>,
    /// Added / removed lines; `None` renders `—` until a diff seam counts
    /// every workspace change (§10, §14.4).
    pub diff: Option<(u64, u64)>,
    pub journal: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LedgerSpend {
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_hit_percent: Option<u64>,
    pub cost_micro_usd: Option<u64>,
}

/// The pane's 1-line WORKERS summary (§9.2); the WORKERS mode itself shows
/// the full blocks (`render/workers.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkersSummary {
    pub live: u64,
    pub done: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FoldRef {
    pub handle: String,
    pub kind: String,
    pub lines: u64,
}

/// The inner content width (pane width minus the 4-cell pad on each side).
fn ledger_grid(width: usize) -> usize {
    width.saturating_sub(8)
}

/// The bar's cell count (§9.2: `n = grid − 7` — 5 cells for `! NNN%`, 2 for
/// the Band's minimum gap when the right side is non-empty).
fn bar_cells(grid: usize) -> usize {
    grid.saturating_sub(7)
}

/// The inner stop a context part's count right-aligns to (§9.2: `grid − 12`).
fn count_stop(grid: usize) -> usize {
    grid.saturating_sub(12)
}

/// Which sections are shown this pass; `drop_next` removes the lowest-
/// priority survivor (§9.2's drop order), for `render`'s shrink loop.
struct Show {
    goal: bool,
    session: bool,
    context: bool,
    context_parts: bool,
    workspace: bool,
    spend: bool,
    workers: bool,
    folds: bool,
}

impl Show {
    fn all(pane: &LedgerPane) -> Self {
        Self {
            goal: pane.goal.is_some(),
            session: pane.session.is_some(),
            context: pane.context.is_some(),
            context_parts: pane.context.as_ref().is_some_and(|c| !c.parts.is_empty()),
            workspace: pane.workspace.is_some(),
            spend: pane.spend.is_some(),
            workers: pane.workers.is_some(),
            folds: !pane.folds.is_empty(),
        }
    }

    /// Drop order (§9.2), lowest priority first. `false` once nothing is
    /// left to drop (an empty pane is the floor, never a panic).
    fn drop_next(&mut self) -> bool {
        for slot in [
            &mut self.folds,
            &mut self.workspace,
            &mut self.context_parts,
            &mut self.session,
            &mut self.workers,
            &mut self.spend,
            &mut self.context,
            &mut self.goal,
        ] {
            if *slot {
                *slot = false;
                return true;
            }
        }
        false
    }
}

/// Render the LEDGER pane: one `Band` per row at `width` (pad 4, grid =
/// `width − 8`), sections separated by one blank BLOCK row. `height` bounds
/// the pane; when the full ledger overflows it, sections drop lowest-
/// priority-first (§9.2) until it fits or nothing is left.
pub fn render(pane: &LedgerPane, width: usize, height: Option<usize>) -> Vec<Line<'static>> {
    let mut show = Show::all(pane);
    loop {
        let built = build(pane, width, &show);
        match height {
            Some(h) if built.len() > h && show.drop_next() => continue,
            _ => return built,
        }
    }
}

fn build(pane: &LedgerPane, width: usize, show: &Show) -> Vec<Line<'static>> {
    let mut sections: Vec<Vec<Line<'static>>> = Vec::new();
    if show.goal
        && let Some(goal) = &pane.goal
    {
        sections.push(goal_lines(goal, width));
    }
    if show.session
        && let Some(session) = &pane.session
    {
        sections.push(session_lines(session, width));
    }
    if show.context
        && let Some(context) = &pane.context
    {
        sections.push(context_lines(context, width, show.context_parts));
    }
    if show.workspace
        && let Some(workspace) = &pane.workspace
    {
        sections.push(workspace_lines(workspace, width));
    }
    if show.workers
        && let Some(workers) = &pane.workers
    {
        sections.push(vec![workers_summary_line(workers, width)]);
    }
    if show.spend
        && let Some(spend) = &pane.spend
    {
        sections.push(spend_lines(spend, width));
    }
    if show.folds && !pane.folds.is_empty() {
        sections.push(folds_lines(&pane.folds, width));
    }
    let mut out = Vec::new();
    for (n, section) in sections.into_iter().enumerate() {
        if n > 0 {
            out.push(blank_row(width));
        }
        out.extend(section);
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

fn header_row(width: usize, text: &str) -> Line<'static> {
    Band {
        bg: palette::BLOCK,
        left: vec![Seg::new(palette::DIM, text.to_string())],
        right: vec![],
        width,
        pad: 4,
    }
    .render()
}

fn value_row(
    width: usize,
    label: &str,
    value_fg: ratatui::style::Color,
    value: &str,
) -> Line<'static> {
    Band {
        bg: palette::BLOCK,
        left: vec![Seg::new(palette::DIM, label.to_string())],
        right: vec![Seg::new(value_fg, value.to_string())],
        width,
        pad: 4,
    }
    .render()
}

fn goal_lines(goal: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![header_row(width, "GOAL")];
    for part in crate::wrap::wrap(goal, ledger_grid(width)) {
        out.push(
            Band {
                bg: palette::BLOCK,
                left: vec![Seg::new(palette::INK, part)],
                right: vec![],
                width,
                pad: 4,
            }
            .render(),
        );
    }
    out
}

fn session_lines(session: &SessionView, width: usize) -> Vec<Line<'static>> {
    vec![
        header_row(width, "SESSION"),
        value_row(width, "  model", palette::INK, &session.model),
        value_row(width, "  effort", palette::INK, &session.effort),
        value_row(width, "  access", palette::INK, &session.access),
        value_row(width, "  sandbox", palette::INK, &session.sandbox),
    ]
}

fn context_lines(context: &ContextView, width: usize, show_parts: bool) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let total = match context.used {
        Some(used) => format!("{} / {}", tokens(used), tokens(context.window)),
        None => format!("{UNKNOWN} / {}", tokens(context.window)),
    };
    out.push(value_row(width, "CONTEXT", palette::INK, &total));

    let grid = ledger_grid(width);
    let n = bar_cells(grid);
    let attn = context
        .used
        .is_some_and(|used| used >= context.summarize_at);
    let filled = match context.used {
        Some(used) => ((used as f64 / context.window.max(1) as f64).clamp(0.0, 1.0) * n as f64)
            .round() as usize,
        None => 0,
    };
    let percent = match context.used {
        Some(used) => format!(
            "{}%",
            (used as f64 * 100.0 / context.window.max(1) as f64).round() as u64
        ),
        None => UNKNOWN.to_string(),
    };
    let right = if attn {
        vec![
            Seg::new(palette::ATTN, "! "),
            Seg::new(palette::INK, percent),
        ]
    } else {
        vec![Seg::new(palette::INK, percent)]
    };
    out.push(
        Band {
            bg: palette::BLOCK,
            left: vec![
                Seg::new(palette::INK, "█".repeat(filled)),
                Seg::new(palette::RULE, "█".repeat(n.saturating_sub(filled))),
            ],
            right,
            width,
            pad: 4,
        }
        .render(),
    );
    if show_parts {
        for part in &context.parts {
            out.push(match part.count {
                Some(count) => {
                    counted_line(width, &part.label, &count.to_string(), &tokens(part.tokens))
                }
                None => value_row(
                    width,
                    &format!("  {}", part.label),
                    palette::INK,
                    &tokens(part.tokens),
                ),
            });
        }
    }
    out.push(value_row(
        width,
        "  summarize at",
        palette::INK,
        &round_k(context.summarize_at),
    ));
    out
}

/// The summarize threshold's display (§9.2 mock: `96k`, never `96.0k`): a
/// configured token budget (`[context] summarize_at_tokens`) is round by
/// convention, so it reads as whole thousands — unlike `tokens()` (used for
/// MEASURED usage above), which always keeps one decimal below 100k.
fn round_k(n: u64) -> String {
    if n < 1_000 {
        n.to_string()
    } else {
        format!("{}k", n / 1_000)
    }
}

/// A three-column row (label, count at the inner stop, value at the grid
/// edge — §9.2): `Band` only has two columns, so this nests two paddings
/// the same way `grid::counted_row` does, but paints BLOCK on every cell (a
/// pure pane row must carry its own background, §render/pane.rs).
fn counted_line(width: usize, label: &str, count: &str, value: &str) -> Line<'static> {
    let pad = 4;
    let inner = width.saturating_sub(2 * pad);
    let stop = count_stop(inner).min(inner);
    let label = format!("  {label}");
    let bg = palette::BLOCK;
    let inner_pad = stop.saturating_sub(cell_width(&label) + cell_width(count));
    let outer_pad = inner.saturating_sub(stop + cell_width(value));
    let space = |n: usize| Span::styled(" ".repeat(n), Style::new().bg(bg));
    Line::from(vec![
        space(pad),
        Span::styled(label, Style::new().fg(palette::DIM).bg(bg)),
        space(inner_pad),
        Span::styled(count.to_string(), Style::new().fg(palette::INK).bg(bg)),
        space(outer_pad),
        Span::styled(value.to_string(), Style::new().fg(palette::INK).bg(bg)),
        space(pad),
    ])
}

fn workspace_lines(workspace: &WorkspaceView, width: usize) -> Vec<Line<'static>> {
    let files = workspace
        .files
        .map(|f| f.to_string())
        .unwrap_or_else(|| UNKNOWN.into());
    let diff = workspace
        .diff
        .map(|(added, removed)| format!("+{added} \u{2212}{removed}"))
        .unwrap_or_else(|| UNKNOWN.into());
    let journal = workspace.journal.clone().unwrap_or_else(|| UNKNOWN.into());
    vec![
        header_row(width, "WORKSPACE"),
        value_row(width, "  files", palette::INK, &files),
        value_row(width, "  diff", palette::INK, &diff),
        value_row(width, "  journal", palette::INK, &journal),
    ]
}

fn spend_lines(spend: &LedgerSpend, width: usize) -> Vec<Line<'static>> {
    let or_unknown =
        |value: Option<u64>, format: fn(u64) -> String| value.map(format).unwrap_or(UNKNOWN.into());
    vec![
        header_row(width, "SPEND"),
        value_row(
            width,
            "  in",
            palette::INK,
            &or_unknown(spend.input, tokens),
        ),
        value_row(
            width,
            "  out",
            palette::INK,
            &or_unknown(spend.output, tokens),
        ),
        value_row(
            width,
            "  cache hit",
            palette::INK,
            &or_unknown(spend.cache_hit_percent, |p| format!("{p}%")),
        ),
        value_row(
            width,
            "  cost",
            palette::INK,
            &or_unknown(spend.cost_micro_usd, cost_string),
        ),
    ]
}

fn workers_summary_line(workers: &WorkersSummary, width: usize) -> Line<'static> {
    Band {
        bg: palette::BLOCK,
        left: vec![Seg::new(palette::DIM, "WORKERS".to_string())],
        right: vec![
            Seg::new(palette::INK, workers.live.to_string()),
            Seg::new(palette::DIM, " live · ".to_string()),
            Seg::new(palette::INK, workers.done.to_string()),
            Seg::new(palette::DIM, " done   ".to_string()),
            Seg::new(palette::FAINT, "^Tab".to_string()),
        ],
        width,
        pad: 4,
    }
    .render()
}

fn folds_lines(folds: &[FoldRef], width: usize) -> Vec<Line<'static>> {
    let mut out = vec![header_row(width, "FOLDS")];
    for fold in folds.iter().take(3) {
        out.push(
            Band {
                bg: palette::BLOCK,
                left: vec![Seg::new(palette::REF, format!("  {}", fold.handle))],
                right: vec![Seg::new(
                    palette::DIM,
                    format!("{} · {} lines", fold.kind, fold.lines),
                )],
                width,
                pad: 4,
            }
            .render(),
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain2(line: &Line<'static>) -> String {
        ratatui::text::Text::from(line.clone()).to_string()
    }

    #[test]
    fn unknown_context_usage_is_a_dash_never_a_fake_zero() {
        let pane = LedgerPane {
            context: Some(ContextView {
                used: None,
                window: 120_000,
                summarize_at: 96_000,
                parts: vec![],
            }),
            ..Default::default()
        };
        let text: Vec<String> = render(&pane, 38, None).iter().map(plain2).collect();
        assert_eq!(text[0], "    CONTEXT               — / 120k    ");
        // The bar is fully unfilled (empty), the percent is —, never 0%.
        assert_eq!(text[1], format!("    {}      —    ", "█".repeat(23)));
    }

    #[test]
    fn the_attn_mark_appears_only_at_the_summarize_threshold() {
        let below = ContextView {
            used: Some(95_999),
            window: 120_000,
            summarize_at: 96_000,
            parts: vec![],
        };
        let at = ContextView {
            used: Some(96_000),
            ..below.clone()
        };
        let text_below: Vec<String> = render(
            &LedgerPane {
                context: Some(below),
                ..Default::default()
            },
            38,
            None,
        )
        .iter()
        .map(plain2)
        .collect();
        let text_at: Vec<String> = render(
            &LedgerPane {
                context: Some(at),
                ..Default::default()
            },
            38,
            None,
        )
        .iter()
        .map(plain2)
        .collect();
        assert!(!text_below[1].contains('!'));
        assert!(text_at[1].contains("! "));
    }
}
