//! The LEDGER pane (SPEC §5): a 32-column grid inside the pane width. Every
//! row is `label + pad + value` through the grid rule — nothing is positioned
//! by eye. Unknown values render `—`, never 0. Sections with no data at all
//! are omitted rather than shown empty.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::grid;
use crate::palette;

use super::{UNKNOWN, tokens};

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

/// The spend section. `None` renders `—`; `responses == 0` renders the whole
/// section as `—` rows rather than a fake zero (SPEC §5 product contract).
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
    out.push(header("SPEND"));
    let known = spend.responses > 0;
    let or_unknown = |value: Option<u64>, format: fn(u64) -> String| {
        if known {
            value.map(format).unwrap_or(UNKNOWN.into())
        } else {
            UNKNOWN.into()
        }
    };
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
        &or_unknown(spend.cost_micro_usd, |micro| {
            format!("${}.{:04}", micro / 1_000_000, (micro % 1_000_000) / 100)
        }),
    ));
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
}
