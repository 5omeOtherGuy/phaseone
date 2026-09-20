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
    let body_width = width.saturating_sub(2);
    for part in crate::wrap::wrap(&composer.text, body_width) {
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
    fn the_floor_line_right_aligns_its_keys() {
        let line = floor_line("ask", "claude", "12.4k", 80);
        let text = Text::from(line).to_string();
        assert!(text.starts_with("ask · claude · 12.4k"));
        assert!(text.ends_with("^L ledger   ^C cancel"));
        assert_eq!(text.chars().count(), 80);
    }
}
