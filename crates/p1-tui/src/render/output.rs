//! The OUTPUT pane mode (handoff §9.3): the expanded form of a fold handle — header, source,
//! range, then the numbered lines in view. Yank (clipboard) is deliberately out of scope for
//! now — p1 has no clipboard seam — and filtering lands with the pane's own input focus.

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::fold::FoldId;
use crate::palette;
use crate::wrap::{cell_width, fit_cells};

/// The pane's view of one fold: the full output and a scroll offset in rows
/// from the top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputView {
    pub id: FoldId,
    pub lines: Vec<String>,
    pub scroll: usize,
}

impl OutputView {
    /// The OUTPUT pane content for `body_rows` visible lines from `scroll`, numbered from 1.
    /// Scrolling past the end keeps the last line in view, never a blank pane.
    pub fn pane(&self, source: Vec<Seg>, body_rows: usize) -> OutputPane {
        let total = self.lines.len();
        let first = self.scroll.min(total.saturating_sub(1));
        let lines: Vec<(u64, String)> = self
            .lines
            .iter()
            .enumerate()
            .skip(first)
            .take(body_rows)
            .map(|(n, text)| (n as u64 + 1, text.clone()))
            .collect();
        let range = match (lines.first(), lines.last()) {
            (Some((from, _)), Some((to, _))) => format!("{from}–{to} of {total}"),
            _ => format!("0 of {total}"),
        };
        OutputPane {
            handle: self.id.to_string(),
            source,
            range,
            lines,
        }
    }
}

/// The OUTPUT pane's content: a fold handle's header facts and the window of
/// body lines currently in view (the caller scrolls by changing the slice
/// and `range`; this renderer never re-paginates).
#[derive(Debug, Clone, PartialEq)]
pub struct OutputPane {
    /// The bare handle, e.g. `h-0275b8a9` (the header brackets it itself).
    pub handle: String,
    /// `tool · target · <outcome>` pre-styled by the caller (host `Face`
    /// territory — this module never names a tool, §7.1).
    pub source: Vec<Seg>,
    /// Right-aligned range fact, e.g. `12–41 of 94`.
    pub range: String,
    /// `(line number, text)` for the visible window only.
    pub lines: Vec<(u64, String)>,
}

/// Render the OUTPUT pane: header, source, range, one blank row, then the
/// numbered body (§9.3). `width` is the pane width (pad 4); body lines over
/// `width − 14` (linenum field 4 + 2-cell gap + the 8-cell outer pad) cut
/// with `…` — output is evidence, never wrapped (§7.1).
pub fn render(pane: &OutputPane, width: usize) -> Vec<Line<'static>> {
    let mut out = vec![
        Band {
            bg: palette::BLOCK,
            left: vec![Seg::new(palette::DIM, "OUTPUT".to_string())],
            right: vec![Seg::new(palette::FAINT, format!("[{}]", pane.handle))],
            width,
            pad: 4,
        }
        .render(),
        Band {
            bg: palette::BLOCK,
            left: pane.source.clone(),
            right: vec![],
            width,
            pad: 4,
        }
        .render(),
        Band {
            bg: palette::BLOCK,
            left: vec![],
            right: vec![Seg::new(palette::DIM, pane.range.clone())],
            width,
            pad: 4,
        }
        .render(),
        blank_row(width),
    ];
    let grid = width.saturating_sub(8);
    let text_field = grid.saturating_sub(6);
    for (n, text) in &pane.lines {
        let cut = if cell_width(text) > text_field {
            format!("{}…", fit_cells(text, text_field.saturating_sub(1)))
        } else {
            text.clone()
        };
        out.push(
            Band {
                bg: palette::BLOCK,
                left: vec![
                    Seg::new(palette::FAINT, format!("{n:>4}")),
                    Seg::new(palette::DIM, format!("  {cut}")),
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_window_numbers_from_one_and_keeps_the_last_line_in_view() {
        let view = OutputView {
            id: FoldId::of("x"),
            lines: (1..=20).map(|n| format!("line {n}")).collect(),
            scroll: 5,
        };
        let pane = view.pane(Vec::new(), 4);
        assert_eq!(pane.range, "6–9 of 20");
        assert_eq!(pane.lines[0], (6, "line 6".to_string()));
        let past = OutputView { scroll: 99, ..view };
        assert_eq!(past.pane(Vec::new(), 4).range, "20–20 of 20");
    }
}
