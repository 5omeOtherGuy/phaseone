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
use crate::wrap::{cell_width, fit_cells};

/// One label/value row: `DIM label` left, `INK value` right-aligned to `width`
/// (the SPEC §1 hierarchy rule applied at the grid rule's single call site).
/// A row that cannot fit keeps the value whole and truncates the label with
/// `…` — the value is what the operator needs to read.
pub fn row(width: usize, label: &str, value: &str) -> Line<'static> {
    styled_row(width, label, palette::DIM, value, palette::INK)
}

/// The same row with explicit colours, for states the hierarchy rule varies
/// (an unavailable row is FAINT across the WHOLE row, label included).
pub fn styled_row(
    width: usize,
    label: &str,
    label_fg: Color,
    value: &str,
    value_fg: Color,
) -> Line<'static> {
    let label = fit_label(width, label, value);
    let pad = width.saturating_sub(cell_width(&label) + cell_width(value));
    Line::from(vec![
        Span::styled(label, Style::new().fg(label_fg)),
        // The pad shares the label's colour so a "whole row FAINT" rule holds
        // for every span, not just the visible ones.
        Span::styled(" ".repeat(pad), Style::new().fg(label_fg)),
        Span::styled(value.to_string(), Style::new().fg(value_fg)),
    ])
}

/// A label/description row (pickers, the palette, /help, /status). When
/// both fit it is the grid row; when they do not, the label — the key or the
/// command the operator types — stays whole (up to half the grid) and the
/// description is cut with `…`, two spaces apart so they never read as one.
pub fn described_row(
    width: usize,
    label: &str,
    label_fg: Color,
    value: &str,
    value_fg: Color,
) -> Line<'static> {
    if cell_width(label) + 2 + cell_width(value) <= width {
        return styled_row(width, label, label_fg, value, value_fg);
    }
    // A value up to two thirds of the grid (an output's `h-2ede · 60 lines`)
    // is kept whole beside a long label; a longer one (a description) yields
    // to the label past half the grid.
    let keep = if cell_width(value) * 3 <= width * 2 {
        cell_width(label).min(width.saturating_sub(cell_width(value) + 2))
    } else {
        cell_width(label).min(width.saturating_sub(cell_width(value) + 2).max(width / 2))
    };
    let label = if cell_width(label) <= keep {
        label.to_string()
    } else {
        let mut cut = fit_cells(label, keep.saturating_sub(1));
        cut.push('…');
        cut
    };
    let room = width.saturating_sub(cell_width(&label) + 2);
    let value = if cell_width(value) <= room {
        value.to_string()
    } else if room >= 2 {
        let mut cut = fit_cells(value, room - 1);
        cut.push('…');
        cut
    } else {
        String::new()
    };
    let pad = width.saturating_sub(cell_width(&label) + cell_width(&value));
    Line::from(vec![
        Span::styled(label, Style::new().fg(label_fg)),
        Span::styled(" ".repeat(pad), Style::new().fg(label_fg)),
        Span::styled(value, Style::new().fg(value_fg)),
    ])
}

/// A three-column row: label left, a count right-aligned to the fixed inner
/// `stop`, the value right-aligned to the grid width (SPEC §5: 20 in the
/// ledger). Rows with and without a count therefore share one value column.
pub fn counted_row(
    width: usize,
    stop: usize,
    label: &str,
    count: &str,
    value: &str,
) -> Line<'static> {
    let stop = stop.min(width);
    // Lay out as two nested grid rows: [label, count] on the inner stop, then
    // the value against the full width.
    let label = fit_label(stop, label, count);
    let inner_pad = stop.saturating_sub(cell_width(&label) + cell_width(count));
    let outer_pad = width.saturating_sub(stop + cell_width(value));
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

/// Truncate `label` with `…` until `label + 1 space + value` fits `width`
/// CELLS (display width — a wide char occupies two).
fn fit_label(width: usize, label: &str, value: &str) -> String {
    let room = width.saturating_sub(cell_width(value) + 1);
    if cell_width(label) <= room {
        return label.to_string();
    }
    let mut out = fit_cells(label, room.saturating_sub(1));
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
    fn a_description_row_keeps_the_key_and_cuts_the_description() {
        let row = |w, l, v| text(described_row(w, l, palette::DIM, v, palette::INK));
        // Fits: the ordinary grid row.
        assert_eq!(row(20, "  /help", "keys"), "  /help         keys");
        // Too narrow: the command stays, the description is cut.
        assert_eq!(
            row(30, "  /outputs", "pick a retained output to open"),
            "  /outputs  pick a retained o…"
        );
        // A label longer than half the grid is cut too, never to nothing.
        let narrow = row(20, "  ^W · ^Tab or F6 · ^P", "pane width · pane mode");
        assert_eq!(narrow, "  ^W · ^T…  pane wi…");
        // A short value is kept whole beside a long label.
        let output = row(
            40,
            "  shell  python3 -c 'for i in range(60): print(i)'",
            "h-2ede · 60 lines",
        );
        assert!(output.ends_with("h-2ede · 60 lines"), "{output}");
        assert!(narrow.contains("  "));
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
        // The fill is carried by the STYLE (INK over RULE), not the glyph.
        assert_eq!(spans[0].style.fg, Some(palette::INK));
        assert_eq!(spans[1].style.fg, Some(palette::RULE));
        let spans = bar(4, 2.0);
        assert_eq!(spans[0].content.as_ref(), "████");
        assert_eq!(spans[1].content.as_ref(), "");
    }
}
