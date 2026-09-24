use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use crate::text::sanitize_segments;
use crate::wrap::{cell_width, fit_cells};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Seg {
    pub fg: Color,
    pub bg: Option<Color>,
    pub text: String,
}
impl Seg {
    pub fn new(fg: Color, text: impl Into<String>) -> Self {
        Self {
            fg,
            bg: None,
            text: text.into(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Band {
    pub bg: Color,
    pub left: Vec<Seg>,
    pub right: Vec<Seg>,
    pub width: usize,
    pub pad: usize,
}

impl Band {
    pub fn render(&self) -> Line<'static> {
        let inner = self.width.saturating_sub(2 * self.pad);
        let mut left = sanitize_segments(&self.left);
        let right = sanitize_segments(&self.right);
        let right_len: usize = right.iter().map(|s| cell_width(&s.text)).sum();
        let left_len: usize = left.iter().map(|s| cell_width(&s.text)).sum();
        // A side can keep segments whose text sanitized away (e.g. a lone incomplete CSI) or
        // survives only as zero-cell marks, and those draw nothing: reserving the separator for
        // them would truncate the left for free. So the separator is keyed on cells, not text.
        let mut over =
            (left_len + right_len + if right_len > 0 { 2 } else { 0 }) as isize - inner as isize;
        for seg in left.iter_mut().rev() {
            if over <= 0 {
                break;
            }
            let len = cell_width(&seg.text);
            if len as isize > over + 1 {
                seg.text = format!("{}…", fit_cells(&seg.text, len - over as usize - 1));
                over = 0;
            } else {
                over -= len as isize;
                seg.text.clear();
            }
        }
        let l: usize = left.iter().map(|s| cell_width(&s.text)).sum();
        let gap = inner.saturating_sub(l + right_len);
        let mut spans = Vec::new();
        let space = |n: usize, spans: &mut Vec<Span<'static>>| {
            if n > 0 {
                spans.push(Span::styled(" ".repeat(n), Style::new().bg(self.bg)));
            }
        };
        space(self.pad, &mut spans);
        for seg in left {
            if !seg.text.is_empty() {
                spans.push(Span::styled(
                    seg.text,
                    Style::new().fg(seg.fg).bg(seg.bg.unwrap_or(self.bg)),
                ));
            }
        }
        space(gap, &mut spans);
        for seg in &right {
            if !seg.text.is_empty() {
                spans.push(Span::styled(
                    seg.text.clone(),
                    Style::new().fg(seg.fg).bg(seg.bg.unwrap_or(self.bg)),
                ));
            }
        }
        space(self.pad, &mut spans);
        clip(Line::from(spans), self.width)
    }
}

/// The §5 rule ends with "clipped to width": a right side wider than the row (which never
/// truncates) must still not spill past the band.
fn clip(line: Line<'static>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let w = cell_width(&span.content);
        if used + w <= width {
            used += w;
            spans.push(span);
        } else {
            let room = width - used;
            if room > 0 {
                spans.push(Span::styled(fit_cells(&span.content, room), span.style));
            }
            break;
        }
    }
    Line::from(spans)
}

/// Keep a path's basename whole and replace a removed prefix at a slash with an ellipsis.
pub fn truncate_path_middle(path: &str, cells: usize) -> String {
    if cell_width(path) <= cells {
        return path.to_string();
    }
    let name = path.rsplit('/').next().unwrap_or(path);
    if cell_width(name) + 1 > cells {
        return truncate(path, cells);
    }
    let room = cells.saturating_sub(cell_width(name) + 2);
    let suffix_start = path.len().saturating_sub(name.len());
    let prefix = &path[..suffix_start.saturating_sub(1)];
    let keep = fit_cells(prefix, room);
    format!("{keep}…/{name}")
}

fn truncate(text: &str, cells: usize) -> String {
    if cells == 0 {
        return String::new();
    }
    if cell_width(text) <= cells {
        return text.into();
    }
    format!("{}…", fit_cells(text, cells - 1))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::palette;
    #[test]
    fn right_survives_and_left_last_segment_is_cut() {
        let line = Band {
            bg: palette::BLOCK,
            left: vec![
                Seg::new(palette::INK, "hello"),
                Seg::new(palette::DIM, " world"),
            ],
            right: vec![Seg::new(palette::OK, " DONE")],
            width: 18,
            pad: 1,
        }
        .render();
        assert_eq!(line.to_string(), " hello wo…   DONE ");
    }
    #[test]
    fn a_right_side_wider_than_the_row_is_clipped_and_keeps_its_fill() {
        let chip = Seg {
            fg: palette::GROUND,
            bg: Some(palette::INK),
            text: " chip ".into(),
        };
        let line = Band {
            bg: palette::BLOCK,
            left: vec![],
            right: vec![chip, Seg::new(palette::DIM, " far too long")],
            width: 10,
            pad: 1,
        }
        .render();
        assert_eq!(crate::wrap::cell_width(&line.to_string()), 10);
        assert_eq!(line.spans[1].style.bg, Some(palette::INK));
    }
    #[test]
    fn a_right_side_that_sanitizes_to_nothing_reserves_no_separator() {
        let band = |right: &str, width| Band {
            bg: palette::BLOCK,
            left: vec![Seg::new(palette::INK, "abcdef")],
            right: vec![Seg::new(palette::OK, right)],
            width,
            pad: 0,
        };
        for right in ["\u{1b}[31", "\u{301}", "\u{1b}[31m\u{301}"] {
            assert!(
                band(right, 2).render().to_string().starts_with("a\u{2026}"),
                "{right:?}"
            );
            assert!(
                band(right, 6).render().to_string().starts_with("abcdef"),
                "{right:?}"
            );
        }
        // Visible text after a malformed chip still earns its separator.
        let line = Band {
            bg: palette::BLOCK,
            left: vec![Seg::new(palette::INK, "abcdef")],
            right: vec![
                Seg::new(palette::DIM, "\u{1b}[31m"),
                Seg::new(palette::OK, "OK"),
            ],
            width: 8,
            pad: 0,
        }
        .render();
        assert_eq!(line.to_string(), "abc\u{2026}  OK");
    }
    #[test]
    fn path_cut_keeps_basename_and_wide_cut_is_safe() {
        assert_eq!(
            truncate_path_middle("crates/p1-contracts/src/edge.rs", 20),
            "crates/p1-c…/edge.rs"
        );
        assert_eq!(truncate("ab界cd", 4), "ab…");
    }
}
