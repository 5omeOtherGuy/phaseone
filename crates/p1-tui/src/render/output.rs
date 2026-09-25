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
    // Clamp: scrolling past the end shows the last row, never a blank pane.
    let scroll = view.scroll.min(body.len().saturating_sub(1));
    out.extend(body.into_iter().skip(scroll));
    out
}

/// Preserve the output's row and column structure. Horizontal navigation reaches
/// clipped text without reflowing shell tables, stack traces or diffs.
pub fn view_lines(
    view: &OutputView,
    grid: usize,
    filter: &str,
    horizontal: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let needle = filter.to_lowercase();
    let matches: Option<Vec<usize>> = (!needle.is_empty()).then(|| {
        view.lines
            .iter()
            .enumerate()
            .filter(|(_, l)| super::block::clean(l).to_lowercase().contains(&needle))
            .map(|(i, _)| i)
            .collect()
    });
    pane_lines(&Pane {
        view,
        grid,
        filter,
        matches: matches.as_deref(),
        horizontal,
        height,
        focused: true,
        newer: None,
        searching: false,
    })
}

/// Everything the OUTPUT pane draws from.
pub struct Pane<'a> {
    pub view: &'a OutputView,
    pub grid: usize,
    pub filter: &'a str,
    /// Line indices the filter keeps (`None` = unfiltered).
    pub matches: Option<&'a [usize]>,
    pub horizontal: usize,
    pub height: usize,
    /// Whether keys go to the pane: the header says so.
    pub focused: bool,
    /// A newer retained output than the one shown.
    pub newer: Option<&'a FoldId>,
    /// A filter is being typed: its field shows from the first key (`/`).
    pub searching: bool,
}

/// Skip `cells` display cells of `text` (panning).
fn skip_cells(text: &str, cells: usize) -> String {
    let mut skipped = 0;
    text.chars()
        .skip_while(|c| {
            if skipped >= cells {
                false
            } else {
                skipped += unicode_width::UnicodeWidthChar::width(*c).unwrap_or(0);
                true
            }
        })
        .collect()
}

pub fn pane_lines(pane: &Pane<'_>) -> Vec<Line<'static>> {
    let view = pane.view;
    let grid = pane.grid;
    let filtered = pane.matches.is_some();
    let field = filtered || pane.searching;
    let count = pane.matches.map_or(view.lines.len(), <[usize]>::len);
    let body = pane.height.saturating_sub(2 + usize::from(field));
    let first = view.scroll.min(count);
    let last = (first + body).min(count);
    let range = if count == 0 {
        "0".to_owned()
    } else {
        format!("{}–{}", first + 1, last)
    };
    let mut facts = if filtered {
        format!("{range} of {count} · {} lines", view.lines.len())
    } else {
        format!("{range}/{}", view.lines.len())
    };
    if pane.horizontal > 0 {
        facts.push_str(&format!(" · col {}", pane.horizontal + 1));
    }
    let title_fg = if pane.focused {
        palette::INK
    } else {
        palette::DIM
    };
    // Fitted, never cut mid-value: the newer-output note goes first, then the
    // range shortens, then the title.
    let range_only = if filtered {
        format!("{range} of {count}")
    } else {
        format!("{range}/{}", view.lines.len())
    };
    let newer = pane.newer.map(|n| format!("  newer {n} ^O"));
    let title = "OUTPUT ";
    let id = format!("[{}]  ", view.id);
    let w = crate::wrap::cell_width;
    let fits = |parts: &[&str]| parts.iter().map(|p| w(p)).sum::<usize>() <= grid;
    let (title, facts, newer) =
        if let Some(n) = newer.as_deref().filter(|n| fits(&[title, &id, &facts, n])) {
            (title, facts, Some(n.to_owned()))
        } else if fits(&[title, &id, &facts]) {
            (title, facts, None)
        } else if fits(&[title, &id, &range_only]) {
            (title, range_only, None)
        } else if fits(&[&id, &range_only]) {
            ("", range_only, None)
        } else {
            // Narrower still: the handle and the range each whole or not at
            // all — a number is never cut.
            ("", String::new(), None)
        };
    let id = if fits(&[&id]) { id } else { String::new() };
    let mut header = vec![
        Span::styled(title.to_string(), Style::new().fg(title_fg)),
        Span::styled(id, Style::new().fg(palette::DIM)),
        Span::styled(facts, Style::new().fg(palette::DIM)),
    ];
    if let Some(newer) = newer {
        header.push(Span::styled(newer, Style::new().fg(palette::FAINT)));
    }
    let mut out = vec![super::block::band(header, grid, palette::BLOCK)];
    let needle = pane.filter.to_lowercase();
    if field {
        out.push(super::block::band(
            vec![Span::styled(
                format!("/ {}", pane.filter),
                Style::new().fg(palette::INK),
            )],
            grid,
            palette::BLOCK,
        ));
    }
    let digits = view.lines.len().to_string().len();
    let indices: Box<dyn Iterator<Item = usize>> = match pane.matches {
        Some(m) => Box::new(m.iter().copied().skip(first).take(body)),
        None => Box::new((first..last).take(body)),
    };
    for index in indices {
        let clean = super::block::clean(&view.lines[index]);
        let mut spans = vec![];
        let mut room = grid;
        if filtered {
            // Where each match sits in the output: its line number, FAINT.
            spans.push(Span::styled(
                format!("{:>digits$} ", index + 1),
                Style::new().fg(palette::FAINT),
            ));
            room = room.saturating_sub(digits + 1);
        }
        let text = skip_cells(&clean, pane.horizontal);
        if pane.horizontal > 0 && !clean.is_empty() {
            spans.push(Span::styled("‹", Style::new().fg(palette::FAINT)));
            room = room.saturating_sub(1);
        }
        let text = super::block::ellipsis(&text, room);
        // The matched text in INK over the DIM row: no new hue.
        match (!needle.is_empty())
            .then(|| text.to_lowercase().find(&needle))
            .flatten()
            .filter(|at| text.is_char_boundary(*at) && text.is_char_boundary(*at + needle.len()))
        {
            Some(at) => {
                spans.push(Span::styled(
                    text[..at].to_owned(),
                    Style::new().fg(palette::DIM),
                ));
                spans.push(Span::styled(
                    text[at..at + needle.len()].to_owned(),
                    Style::new().fg(palette::INK),
                ));
                spans.push(Span::styled(
                    text[at + needle.len()..].to_owned(),
                    Style::new().fg(palette::DIM),
                ));
            }
            None => spans.push(Span::styled(text, Style::new().fg(palette::DIM))),
        }
        out.push(super::block::band(spans, grid, palette::BLOCK));
    }
    if filtered && count == 0 {
        out.push(super::block::band(
            vec![Span::styled(
                "No matching lines",
                Style::new().fg(palette::DIM),
            )],
            grid,
            palette::BLOCK,
        ));
    }
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
