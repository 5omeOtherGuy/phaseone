//! The OUTPUT pane mode (SPEC §5): the expanded form of a fold handle.
//! Scrollable; the header names the object. Yank (clipboard) is deliberately
//! out of scope for now — p1 has no clipboard seam — and filtering lands with
//! the pane's own input focus (both recorded in SPEC §9).

use ratatui::style::Style;
use ratatui::text::{Line, Span};

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

/// Render the view on the pane's content grid: a FAINT header naming the
/// handle, then the output, wrapped to the grid and scrolled.
pub fn lines(view: &OutputView, grid: usize) -> Vec<Line<'static>> {
    let mut out = vec![Line::from(vec![
        Span::styled("OUTPUT ".to_string(), Style::new().fg(palette::DIM)),
        Span::styled(format!("[{}]", view.id), Style::new().fg(palette::FAINT)),
    ])];
    let mut body: Vec<Line<'static>> = Vec::new();
    for line in &view.lines {
        for part in crate::wrap::wrap(line, grid) {
            body.push(Line::styled(part, Style::new().fg(palette::DIM)));
        }
    }
    // Clamp: scrolling past the end shows the last row, never a blank pane.
    let scroll = view.scroll.min(body.len().saturating_sub(1));
    out.extend(body.into_iter().skip(scroll));
    out
}

// ---------------------------------------------------------------------------
// SLAB Harness OUTPUT (handoff §9.3): header / source / range / body, on
// `band::Band` at the pane width. Kept beside `OutputView`/`lines` above
// (still what `screen.rs` draws — the composition stage wires this in later).

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
    use ratatui::text::Text;

    #[test]
    fn the_header_names_the_handle_and_scroll_skips_rows() {
        let view = OutputView {
            id: FoldId::of("x"),
            lines: (0..20).map(|n| format!("line {n}")).collect(),
            scroll: 5,
        };
        let lines = lines(&view, 48);
        let text: Vec<String> = lines
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        assert!(text[0].starts_with("OUTPUT [h-"));
        assert_eq!(text[1], "line 5");
    }
}
