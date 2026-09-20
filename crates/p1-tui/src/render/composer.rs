//! The composer renderer (SPEC §4.1/§4.2): the `›` prompt, the operator's
//! text, and one hint line whose verbs change with the run state — `send` when
//! idle, `queue steering` while working. Focus mode hides the composer while
//! it is empty; this module only renders what state says is visible.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::glyphs;
use crate::palette;
use crate::state::{Composer, Queued};

/// The composer's rows: queued-input lines (FAINT, so the operator sees what
/// will land), the input line, then the hint line.
pub fn lines(
    composer: &Composer,
    queued: &std::collections::VecDeque<Queued>,
    working: bool,
    width: usize,
) -> Vec<Line<'static>> {
    let ink = Style::new().fg(palette::INK);
    let mut out = Vec::new();
    for q in queued {
        let kind = if q.follow_up { "follow-up" } else { "steering" };
        let text: String = q.text.chars().take(width.saturating_sub(14)).collect();
        out.push(Line::styled(
            format!("{} {kind}: {text}", glyphs::PENDING),
            Style::new().fg(palette::FAINT),
        ));
    }
    let mut first = true;
    // The marker owns the gutter; continuation lines hang under the text.
    // ⌥⏎ newlines are real line breaks; each segment wraps on its own.
    let body_width = width.saturating_sub(2);
    for part in composer
        .text
        .split('\n')
        .flat_map(|segment| crate::wrap::wrap(segment, body_width))
    {
        if first {
            out.push(Line::from(vec![
                Span::styled(format!("{} ", glyphs::OPERATOR), ink),
                Span::styled(part.clone(), ink),
            ]));
            first = false;
        } else {
            out.push(Line::styled(format!("  {part}"), ink));
        }
    }
    let hints = if working {
        "⏎ queue steering   ⌥⏎ queue follow-up   ^C cancel"
    } else {
        "⏎ send   ⌥⏎ newline   ^C quit"
    };
    out.push(Line::styled(hints, Style::new().fg(palette::FAINT)));
    out
}

/// The cursor's cell: (column, row within the composer's lines), accounting
/// for the gutter, wrapping and newline segments.
pub fn cursor_cell(composer: &Composer, queued_rows: usize, width: usize) -> (usize, usize) {
    let body_width = width.saturating_sub(2).max(1);
    let before: String = composer.text.chars().take(composer.cursor).collect();
    let segments: Vec<&str> = before.split('\n').collect();
    let last = segments.len() - 1;
    let mut row = queued_rows;
    let mut col = 2;
    for (n, segment) in segments.iter().enumerate() {
        let w = crate::wrap::cell_width(segment);
        col = 2 + w % body_width;
        row += w / body_width;
        if n < last {
            row += 1; // the newline starts a new row
            col = 2;
        }
    }
    (col, row)
}

/// The one dim line under the composer at the 80-column floor (SPEC §6): the
/// only bottom-of-screen state in the design, and only because the ledger
/// that held these values is gone.
pub fn floor_line(env: &str, route: &str, context: &str, width: usize) -> Line<'static> {
    let left = format!("{env} · {route} · {context}");
    let right = "^L ledger   ^C cancel";
    let pad = width.saturating_sub(left.chars().count() + right.chars().count());
    Line::from(vec![
        Span::styled(left, Style::new().fg(palette::DIM)),
        Span::raw(" ".repeat(pad)),
        Span::styled(right, Style::new().fg(palette::FAINT)),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    #[test]
    fn hints_follow_the_run_state() {
        let composer = Composer::default();
        let idle = lines(&composer, &Default::default(), false, 80);
        assert_eq!(
            Text::from(idle[1].clone()).to_string(),
            "⏎ send   ⌥⏎ newline   ^C quit"
        );
        let working = lines(&composer, &Default::default(), true, 80);
        assert_eq!(
            Text::from(working[1].clone()).to_string(),
            "⏎ queue steering   ⌥⏎ queue follow-up   ^C cancel"
        );
    }

    #[test]
    fn the_cursor_tracks_edits_across_wraps_and_newlines() {
        let mut c = Composer {
            text: "abcd".into(),
            cursor: 2,
            revealed: false,
        };
        assert_eq!(cursor_cell(&c, 0, 10), (4, 0));
        c = Composer {
            text: "ab\ncd".into(),
            cursor: 4, // before the 'd'
            revealed: false,
        };
        assert_eq!(cursor_cell(&c, 0, 10), (3, 1));
        // Wrapping: width 6 -> body 4; "abcdef" wraps after 4.
        c = Composer {
            text: "abcdef".into(),
            cursor: 6,
            revealed: false,
        };
        assert_eq!(cursor_cell(&c, 0, 6), (4, 1));
    }

    #[test]
    fn the_floor_line_right_aligns_its_keys() {
        let line = floor_line("ask", "claude", "12.4k", 80);
        let text = Text::from(line).to_string();
        assert!(text.starts_with("ask · claude · 12.4k"));
        assert!(text.ends_with("^L ledger   ^C cancel"));
        assert_eq!(text.chars().count(), 80);
    }
}
