//! The OUTPUT pane mode (SPEC §5): the expanded form of a fold handle.
//! Scrollable; the header names the object. Yank (clipboard) is deliberately
//! out of scope for now — p1 has no clipboard seam — and filtering lands with
//! the pane's own input focus (both recorded in SPEC §9).

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::fold::FoldId;
use crate::palette;

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
    out.extend(body.into_iter().skip(view.scroll));
    out
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
