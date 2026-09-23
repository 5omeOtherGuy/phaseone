//! The LEDGER pane (SPEC §5): a 32-column grid inside the pane width. Every
//! row is `label + pad + value` through the grid rule — nothing is positioned
//! by eye. Unknown values render `—`, never 0. Sections with no data at all
//! are omitted rather than shown empty.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::band::{Band, Seg};
use crate::grid;
use crate::palette;
use crate::wrap::cell_width;

use super::{UNKNOWN, cost_string, tokens};

/// The LEDGER content grid: 32 columns inside the 40-ch pane (the difference
/// is the pane's padding, SPEC §5).
pub const LEDGER_GRID: usize = 32;
/// The inner stop a row's count right-aligns to (SPEC §5).
const COUNT_STOP: usize = 20;
/// Bar cells on the context row; the percentage right-aligns after it.
const BAR_CELLS: usize = 26;

/// Everything the ledger shows, as plain data. The driver builds this; the
/// renderer never reaches into agent or host state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Ledger {
    /// The session goal, shown as a quotation (SPEC §4.7).
    pub goal: Option<String>,
    pub context: Option<Context>,
    pub task: Option<Task>,
    pub spend: SpendView,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Context {
    pub used: u64,
    pub window: u64,
    /// The warn threshold (SPEC example: 60%).
    pub warn_at: u64,
    /// Budget breakdown rows: label, an optional count (files: 4), tokens.
    /// Empty until the host's context-stats seam lands — the section then
    /// shows the total row only.
    pub parts: Vec<ContextPart>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ContextPart {
    pub label: String,
    pub count: Option<u64>,
    pub tokens: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Task {
    /// The task id when the session has one; `None` shows the files count on
    /// the TASK row instead (p1 has no task ids yet).
    pub id: Option<String>,
    pub files: Option<u64>,
    /// Added / removed lines.
    pub diff: Option<(u64, u64)>,
    /// Preformatted recency, e.g. `2m ago` (the driver owns time formatting).
    pub journal: Option<String>,
}

/// The spend section. `None` renders `—`, never 0 (SPEC §5 product contract);
/// before the first response the whole section is omitted instead.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct SpendView {
    pub responses: u64,
    pub input: Option<u64>,
    pub output: Option<u64>,
    pub cache_hit_percent: Option<u64>,
    pub cost_micro_usd: Option<u64>,
}

/// Render the ledger on its 32-column grid. Sections are separated by one
/// blank line; absent sections leave no gap.
pub fn lines(ledger: &Ledger) -> Vec<Line<'static>> {
    let mut sections: Vec<Vec<Line<'static>>> = Vec::new();
    if let Some(goal) = &ledger.goal {
        let mut section = vec![header("GOAL")];
        for part in crate::wrap::wrap(goal, LEDGER_GRID) {
            section.push(Line::styled(part, Style::new().fg(palette::INK)));
        }
        sections.push(section);
    }
    if let Some(context) = &ledger.context {
        let mut section = Vec::new();
        context_lines(context, &mut section);
        sections.push(section);
    }
    if let Some(task) = &ledger.task {
        let mut section = Vec::new();
        task_lines(task, &mut section);
        sections.push(section);
    }
    // Before the first response there is no spend to report: the section is
    // absent, not a column of `—` (absence is not unknownness).
    if ledger.spend.responses > 0 {
        let mut spend = Vec::new();
        spend_lines(&ledger.spend, &mut spend);
        sections.push(spend);
    }
    let mut out = Vec::new();
    for (n, section) in sections.into_iter().enumerate() {
        if n > 0 {
            out.push(Line::default());
        }
        out.extend(section);
    }
    out
}

/// A section header is a DIM structural label (SPEC §1 hierarchy).
fn header(text: &str) -> Line<'static> {
    Line::styled(text.to_string(), Style::new().fg(palette::DIM))
}

fn context_lines(context: &Context, out: &mut Vec<Line<'static>>) {
    let total = format!("{} / {}", tokens(context.used), tokens(context.window));
    out.push(grid::row(LEDGER_GRID, "CONTEXT", &total));
    let fraction = context.used as f64 / context.window.max(1) as f64;
    let percent = format!("{}%", (fraction * 100.0).round() as u64);
    let mut bar = grid::bar(BAR_CELLS, fraction);
    let pad = LEDGER_GRID.saturating_sub(BAR_CELLS + percent.len());
    bar.push(Span::raw(" ".repeat(pad)));
    bar.push(Span::styled(percent, Style::new().fg(palette::INK)));
    out.push(Line::from(bar));
    for part in &context.parts {
        let label = format!("  {}", part.label);
        let row = match &part.count {
            Some(count) => grid::counted_row(
                LEDGER_GRID,
                COUNT_STOP,
                &label,
                &count.to_string(),
                &tokens(part.tokens),
            ),
            None => grid::row(LEDGER_GRID, &label, &tokens(part.tokens)),
        };
        out.push(row);
    }
    let warn_pct = context.warn_at * 100 / context.window.max(1);
    out.push(grid::row(
        LEDGER_GRID,
        &format!("  warn at {warn_pct}%"),
        &tokens(context.warn_at),
    ));
}

fn task_lines(task: &Task, out: &mut Vec<Line<'static>>) {
    match (&task.id, task.files) {
        (Some(id), _) => out.push(grid::row(LEDGER_GRID, "TASK", id)),
        (None, Some(files)) => out.push(grid::row(LEDGER_GRID, "TASK", &files.to_string())),
        (None, None) => out.push(header("TASK")),
    }
    if task.id.is_some()
        && let Some(files) = task.files
    {
        out.push(grid::row(LEDGER_GRID, "  files", &files.to_string()));
    }
    if let Some((added, removed)) = task.diff {
        out.push(grid::row(
            LEDGER_GRID,
            "  diff",
            &format!("+{added} −{removed}"),
        ));
    }
    if let Some(journal) = &task.journal {
        out.push(grid::row(LEDGER_GRID, "  journal", journal));
    }
}

fn spend_lines(spend: &SpendView, out: &mut Vec<Line<'static>>) {
    // Called only once at least one response landed (the section is omitted
    // before); a None part is a KNOWN-unknown — render `—`.
    out.push(header("SPEND"));
    let or_unknown =
        |value: Option<u64>, format: fn(u64) -> String| value.map(format).unwrap_or(UNKNOWN.into());
    out.push(grid::row(
        LEDGER_GRID,
        "  in",
        &or_unknown(spend.input, tokens),
    ));
    out.push(grid::row(
        LEDGER_GRID,
        "  out",
        &or_unknown(spend.output, tokens),
    ));
    out.push(grid::row(
        LEDGER_GRID,
        "  cache hit",
        &or_unknown(spend.cache_hit_percent, |p| format!("{p}%")),
    ));
    out.push(grid::row(
        LEDGER_GRID,
        "  cost",
        &or_unknown(spend.cost_micro_usd, cost_string),
    ));
}

// ---------------------------------------------------------------------------
// SLAB Harness LEDGER (handoff §9.2). A new model, kept beside the SPEC one
// above (still `screen.rs`'s pane path — the composition stage wires this in
// later): GOAL/SESSION/CONTEXT/WORKSPACE/SPEND/WORKERS/FOLDS, rendered on
// `band::Band` at the PANE width (pad 4, grid = width − 8), not the old
// 32-column content grid. `render` drops sections lowest-priority-first
// (§9.2) when `height` is given and the content overflows it.

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
        sections.push(context_lines_v2(context, width, show.context_parts));
    }
    if show.workspace
        && let Some(workspace) = &pane.workspace
    {
        sections.push(workspace_lines_v2(workspace, width));
    }
    if show.workers
        && let Some(workers) = &pane.workers
    {
        sections.push(vec![workers_summary_line(workers, width)]);
    }
    if show.spend
        && let Some(spend) = &pane.spend
    {
        sections.push(spend_lines_v2(spend, width));
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

fn context_lines_v2(context: &ContextView, width: usize, show_parts: bool) -> Vec<Line<'static>> {
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

fn workspace_lines_v2(workspace: &WorkspaceView, width: usize) -> Vec<Line<'static>> {
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

fn spend_lines_v2(spend: &LedgerSpend, width: usize) -> Vec<Line<'static>> {
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
    use ratatui::text::Text;

    fn plain(line: &Line<'static>) -> String {
        Text::from(line.clone()).to_string()
    }

    fn spec_ledger() -> Ledger {
        Ledger {
            goal: Some("fix compaction boundary stall".into()),
            context: Some(Context {
                used: 12_400,
                window: 200_000,
                warn_at: 120_000,
                parts: vec![
                    ContextPart {
                        label: "system".into(),
                        count: None,
                        tokens: 1_200,
                    },
                    ContextPart {
                        label: "files".into(),
                        count: Some(4),
                        tokens: 6_800,
                    },
                    ContextPart {
                        label: "tools".into(),
                        count: Some(11),
                        tokens: 3_100,
                    },
                    ContextPart {
                        label: "recent".into(),
                        count: None,
                        tokens: 1_300,
                    },
                ],
            }),
            task: Some(Task {
                id: Some("t-3f9a".into()),
                files: Some(3),
                diff: Some((48, 12)),
                journal: Some("2m ago".into()),
            }),
            spend: SpendView {
                responses: 3,
                input: Some(38_100),
                output: Some(4_200),
                cache_hit_percent: Some(71),
                cost_micro_usd: None,
            },
        }
    }

    #[test]
    fn the_spec_example_holds() {
        let lines = lines(&spec_ledger());
        let text: Vec<String> = lines.iter().map(plain).collect();
        assert_eq!(text[0], "GOAL");
        assert_eq!(text[1], "fix compaction boundary stall");
        assert_eq!(text[3], "CONTEXT             12.4k / 200k");
        // 26 bar cells, 6% filled → 2 cells, percentage right-aligned.
        assert_eq!(
            text[4],
            format!("{}{}    6%", "█".repeat(2), "█".repeat(24))
        );
        assert_eq!(text[5], "  system                    1.2k");
        assert_eq!(text[6], "  files            4        6.8k");
        assert_eq!(text[9], "  warn at 60%               120k");
        assert_eq!(text[11], "TASK                      t-3f9a");
        assert_eq!(text[13], "  diff                   +48 −12");
        assert_eq!(text[16], "SPEND");
        // Unknown cost renders —, never 0.
        assert_eq!(text[20], "  cost                         —");
        for line in &text {
            assert!(
                line.chars().count() <= LEDGER_GRID,
                "row overflow: {line:?}"
            );
        }
    }

    #[test]
    fn zero_responses_omits_spend_and_unknown_cost_renders_dash() {
        let empty = Ledger::default();
        assert!(lines(&empty).is_empty(), "no sections have data yet");
        let mut ledger = Ledger::default();
        ledger.spend.responses = 1;
        let text: Vec<String> = lines(&ledger).iter().map(plain).collect();
        assert_eq!(text[0], "SPEND");
        assert_eq!(text[4], "  cost                         —");
    }

    // -----------------------------------------------------------------
    // SLAB LedgerPane (§9.2).

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
