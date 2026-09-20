//! The SPEC §5 grid rule, as functions.
//!
//! Every label/value row is ONE label at the left edge of the grid and ONE
//! value right-aligned to the grid width. No value is positioned by eye or by
//! a counted run of spaces; each row is `label + pad + value` computed against
//! the grid width. This module is the only place that arithmetic lives, so a
//! string whose length changes cannot drift the pane out of alignment.

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::palette;

/// One label/value row: `DIM label` left, `INK value` right-aligned to `width`
/// (the SPEC §1 hierarchy rule applied at the grid rule's single call site).
/// A row that cannot fit keeps the value whole and truncates the label with
/// `…` — the value is what the operator needs to read.
pub fn row(width: usize, label: &str, value: &str) -> Line<'static> {
    styled_row(width, label, palette::DIM, value, palette::INK)
}

/// The same row with explicit colours, for states the hierarchy rule varies
/// (an unavailable row is FAINT across the WHOLE row, label included).
pub fn styled_row(width: usize, label: &str, label_fg: Color, value: &str, value_fg: Color) -> Line<'static> {
    let label = fit_label(width, label, value);
    let pad = width.saturating_sub(label.chars().count() + value.chars().count());
    Line::from(vec![
        Span::styled(label, Style::new().fg(label_fg)),
        // The pad shares the label's colour so a "whole row FAINT" rule holds
        // for every span, not just the visible ones.
        Span::styled(" ".repeat(pad), Style::new().fg(label_fg)),
        Span::styled(value.to_string(), Style::new().fg(value_fg)),
    ])
}

/// A three-column row: label left, a count right-aligned to the fixed inner
/// `stop`, the value right-aligned to the grid width (SPEC §5: 20 in the
/// ledger). Rows with and without a count therefore share one value column.
pub fn counted_row(width: usize, stop: usize, label: &str, count: &str, value: &str) -> Line<'static> {
    let stop = stop.min(width);
    // Lay out as two nested grid rows: [label, count] on the inner stop, then
    // the value against the full width.
    let label = fit_label(stop, label, count);
    let inner_pad = stop.saturating_sub(label.chars().count() + count.chars().count());
    let outer_pad = width.saturating_sub(stop + value.chars().count());
    Line::from(vec![
        Span::styled(label, Style::new().fg(palette::DIM)),
        Span::raw(" ".repeat(inner_pad)),
        Span::styled(count.to_string(), Style::new().fg(palette::INK)),
        Span::raw(" ".repeat(outer_pad)),
        Span::styled(value.to_string(), Style::new().fg(palette::INK)),
    ])
}

/// The SPEC §5 bar: `█` at INK over `█` at RULE, never another glyph. The
/// filled cell count is `round(fraction × cells)`; the fraction is clamped so
/// a bad input can never overflow the row.
pub fn bar(cells: usize, fraction: f64) -> Vec<Span<'static>> {
    let filled = (fraction.clamp(0.0, 1.0) * cells as f64).round() as usize;
    vec![
        Span::styled("█".repeat(filled), Style::new().fg(palette::INK)),
        Span::styled("█".repeat(cells - filled), Style::new().fg(palette::RULE)),
    ]
}

/// Truncate `label` with `…` until `label + 1 space + value` fits `width`.
fn fit_label(width: usize, label: &str, value: &str) -> String {
    let room = width.saturating_sub(value.chars().count() + 1);
    if label.chars().count() <= room {
        return label.to_string();
    }
    let keep = room.saturating_sub(1);
    let mut out: String = label.chars().take(keep).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    fn text(line: Line<'static>) -> String {
        Text::from(line).to_string()
    }

    #[test]
    fn value_right_aligns_to_the_grid_width() {
        assert_eq!(text(row(8, "in", "38.1k")), "in 38.1k");
        assert_eq!(text(row(13, "cache hit", "71%")), "cache hit 71%");
    }

    #[test]
    fn the_label_truncates_never_the_value() {
        assert_eq!(text(row(8, "averylonglabel", "1.2k")), "av… 1.2k");
    }

    #[test]
    fn the_count_sits_on_the_inner_stop() {
        // The count ends exactly at the stop, the value exactly at the width.
        let line = text(counted_row(32, 20, "files", "4", "6.8k"));
        assert_eq!(line.chars().count(), 32);
        assert_eq!(&line[19..20], "4");
        assert!(line.ends_with("6.8k"));
        let line = text(counted_row(32, 20, "tools", "11", "3.1k"));
        assert_eq!(&line[18..20], "11");
        assert!(line.ends_with("3.1k"));
    }

    #[test]
    fn bars_fill_by_rounding_and_clamp() {
        let spans = bar(4, 0.5);
        assert_eq!(spans[0].content.as_ref(), "██");
        assert_eq!(spans[1].content.as_ref(), "██");
        let spans = bar(4, 2.0);
        assert_eq!(spans[0].content.as_ref(), "████");
        assert_eq!(spans[1].content.as_ref(), "");
    }
}
