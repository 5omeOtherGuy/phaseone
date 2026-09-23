//! The composer (handoff §8.1): row 1 BLOCK+ with the `›` prompt and the operator's text (or a
//! faint placeholder), row 2 BLOCK with faint key hints whose verbs follow the state. Long lines
//! soft-wrap and `⌥⏎` lines break, growing the input upward. The cursor is the terminal's
//! hardware cursor: this module says where it goes and never draws a cursor cell.

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::palette;
use crate::state::Composer;
use crate::wrap::cell_width;

/// Band padding on both sides of every composer row.
const PAD: usize = 2;
/// `› ` and the hang indent of continuation rows.
const PROMPT: usize = 2;

/// What the composer is for right now (the §8.1 table rows that are not derived from the text).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode<'a> {
    Idle,
    /// A turn is live: `⏎` queues steering.
    Working,
    /// An approval is on screen: one amber event per view, so the prompt goes dim.
    Decision,
    /// Attached to a worker's read-only transcript.
    Attached(&'a str),
}

/// The composer's rows and where the hardware cursor goes, relative to the first row's
/// top-left cell. `None` only while the composer is disabled (a decision on screen, attached):
/// an editable composer always shows its cursor.
#[derive(Debug, Clone, PartialEq)]
pub struct Frame {
    pub lines: Vec<Line<'static>>,
    pub cursor: Option<(u16, u16)>,
}

/// Render the composer `width` cells wide in at most `max_rows` rows: `1` is the short-screen
/// form (input only, H ≤ 12); otherwise the hint row is kept and the input takes the rest.
pub fn render(composer: &Composer, mode: Mode<'_>, width: usize, max_rows: usize) -> Frame {
    let row = |bg, left: Vec<Seg>, right: Vec<Seg>| {
        Band {
            bg,
            left,
            right,
            width,
            pad: PAD,
        }
        .render()
    };
    let compact = max_rows <= 1;
    let (hints, secondary) = hints(composer, mode);
    let hint_row = row(
        palette::BLOCK,
        vec![Seg::new(palette::FAINT, hints)],
        vec![Seg::new(palette::FAINT, secondary)],
    );
    let disabled = match mode {
        Mode::Decision => Some("decide above".to_string()),
        Mode::Attached(id) => Some(format!("attached to {id} — read only")),
        Mode::Idle | Mode::Working => None,
    };
    let placeholder = match (&disabled, mode) {
        (Some(text), _) => Some(text.clone()),
        (None, _) if !composer.text.is_empty() => None,
        (None, Mode::Working) => Some("steer the running turn".to_string()),
        (None, _) => Some("message, / for commands".to_string()),
    };
    let prompt_fg = if disabled.is_some() {
        palette::DIM
    } else {
        palette::ATTN
    };

    let mut lines = Vec::new();
    let cursor;
    if let Some(placeholder) = placeholder {
        lines.push(row(
            palette::BLOCK_PLUS,
            vec![
                Seg::new(prompt_fg, "› "),
                Seg::new(palette::FAINT, placeholder),
            ],
            vec![],
        ));
        // On a placeholder the block cursor sits on its first cell, where typing starts.
        cursor = disabled.is_none().then_some(((PAD + PROMPT) as u16, 0));
    } else {
        let measure = width.saturating_sub(2 * PAD + PROMPT).max(1);
        let rows = soft_rows(&composer.text, measure);
        // The row holding the cursor: the one whose range contains it, or, at the end of a hard
        // line, that line's last row.
        let cursor_row = rows
            .iter()
            .rposition(|r| r.start <= composer.cursor && composer.cursor <= r.end)
            .unwrap_or(0);
        let room = if compact { 1 } else { max_rows - 1 };
        // Keep the cursor's row on screen; above it, as many earlier rows as fit.
        let first = (cursor_row + 1).saturating_sub(room);
        let chars: Vec<char> = composer.text.chars().collect();
        for (n, r) in rows.iter().enumerate().skip(first).take(room) {
            let prefix = if n == 0 {
                Seg::new(prompt_fg, "› ")
            } else {
                Seg::new(palette::INK, "  ")
            };
            let text: String = chars[r.start..r.end].iter().collect();
            lines.push(row(
                palette::BLOCK_PLUS,
                vec![prefix, Seg::new(palette::INK, text)],
                vec![],
            ));
        }
        let r = &rows[cursor_row];
        let cursor_cells = cell_width(&chars[r.start..composer.cursor].iter().collect::<String>());
        cursor = Some((
            (PAD + PROMPT + cursor_cells) as u16,
            (cursor_row - first) as u16,
        ));
    }
    if !compact {
        lines.push(hint_row);
    }
    Frame { lines, cursor }
}

/// One composer row: a char range of the text, without the newline that ends a hard line.
struct SoftRow {
    start: usize,
    end: usize,
}

/// Soft-wrap every `⌥⏎` line at `measure` cells, keeping every char (spaces included) so the
/// cursor maps onto a cell: a row breaks after its last space that fits, or hard at the cell
/// edge when a word alone is too long. `wrap::wrap` collapses spaces, so it cannot place a
/// cursor.
fn soft_rows(text: &str, measure: usize) -> Vec<SoftRow> {
    let mut rows = Vec::new();
    let mut offset = 0;
    for line in text.split('\n') {
        let chars: Vec<char> = line.chars().collect();
        let mut start = 0;
        loop {
            let mut end = start;
            let mut used = 0;
            while end < chars.len() {
                let w = unicode_width::UnicodeWidthChar::width(chars[end]).unwrap_or(0);
                if used + w > measure {
                    break;
                }
                used += w;
                end += 1;
            }
            if end < chars.len()
                && let Some(space) = chars[start..end].iter().rposition(|c| *c == ' ')
            {
                end = start + space + 1;
            }
            // A char wider than the whole measure still takes a row of its own.
            if end == start && start < chars.len() {
                end += 1;
            }
            rows.push(SoftRow {
                start: offset + start,
                end: offset + end,
            });
            if end >= chars.len() {
                break;
            }
            start = end;
        }
        offset += chars.len() + 1;
    }
    rows
}

/// The hint row's (left, right) for the current state (§8.1 table).
fn hints(composer: &Composer, mode: Mode<'_>) -> (&'static str, &'static str) {
    match mode {
        Mode::Decision => ("", "^C cancel turn"),
        Mode::Attached(_) => ("esc detach   x stop", "PgUp scroll"),
        _ if composer.editing_goal() => ("⏎ set goal   empty ⏎ clears", "esc keep"),
        _ if composer.text.starts_with('/') => ("tab complete   ⏎ run", "esc dismiss"),
        Mode::Working => ("⏎ queue steering   ⌥⏎ queue follow-up", "^C cancel"),
        Mode::Idle => ("⏎ send   ⌥⏎ newline", "^C quit"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn composer(text: &str) -> Composer {
        Composer {
            text: text.into(),
            cursor: text.chars().count(),
            ..Composer::default()
        }
    }

    fn texts(frame: &Frame) -> Vec<String> {
        frame
            .lines
            .iter()
            .map(|l| l.to_string().trim_end().to_string())
            .collect()
    }

    #[test]
    fn every_state_row_of_the_table() {
        let empty = Composer::default();
        let cases = [
            (
                Mode::Idle,
                "  › message, / for commands",
                "  ⏎ send   ⌥⏎ newline",
                "^C quit",
            ),
            (
                Mode::Working,
                "  › steer the running turn",
                "  ⏎ queue steering   ⌥⏎ queue follow-up",
                "^C cancel",
            ),
            (Mode::Decision, "  › decide above", "", "^C cancel turn"),
            (
                Mode::Attached("w2"),
                "  › attached to w2 — read only",
                "  esc detach   x stop",
                "PgUp scroll",
            ),
        ];
        for (mode, input, hints, secondary) in cases {
            let frame = render(&empty, mode, 76, 2);
            let text = texts(&frame);
            assert_eq!(text[0], input, "{mode:?}");
            assert!(text[1].starts_with(hints), "{mode:?}: {}", text[1]);
            assert!(text[1].ends_with(secondary), "{mode:?}: {}", text[1]);
        }
        let typed = texts(&render(&composer("/mo"), Mode::Idle, 76, 2));
        assert!(typed[1].starts_with("  tab complete   ⏎ run"));
        assert!(typed[1].ends_with("esc dismiss"));
        let mut goal = Composer::default();
        goal.begin_goal_edit(Some("ship it"));
        let text = texts(&render(&goal, Mode::Working, 76, 2));
        assert_eq!(text[0], "  › /goal ship it");
        assert!(text[1].starts_with("  ⏎ set goal   empty ⏎ clears"));
        assert!(text[1].ends_with("esc keep"));
    }

    #[test]
    fn the_prompt_dims_and_the_cursor_goes_away_while_a_decision_is_on_screen() {
        let frame = render(&composer("typed"), Mode::Decision, 76, 2);
        assert_eq!(frame.cursor, None);
        assert_eq!(frame.lines[0].spans[1].style.fg, Some(palette::DIM));
        let frame = render(&composer("typed"), Mode::Idle, 76, 2);
        assert_eq!(frame.lines[0].spans[1].style.fg, Some(palette::ATTN));
        assert_eq!(frame.cursor, Some((9, 0)));
    }

    #[test]
    fn newlines_grow_upward_keep_the_hint_row_and_follow_the_cursor() {
        let mut c = composer("one\ntwo\nthree");
        let frame = render(&c, Mode::Idle, 40, 8);
        assert_eq!(texts(&frame)[..3], ["  › one", "    two", "    three"]);
        assert_eq!(frame.cursor, Some((9, 2)));
        // Capped: two input rows plus the hints, the cursor's line kept.
        let frame = render(&c, Mode::Idle, 40, 3);
        assert_eq!(texts(&frame)[..2], ["    two", "    three"]);
        assert_eq!(frame.lines.len(), 3);
        c.cursor = 1;
        let frame = render(&c, Mode::Idle, 40, 3);
        assert_eq!(texts(&frame)[..2], ["  › one", "    two"]);
        assert_eq!(frame.cursor, Some((5, 0)));
    }

    #[test]
    fn the_short_screen_form_is_one_row() {
        let frame = render(&composer("abc"), Mode::Working, 76, 1);
        assert_eq!(frame.lines.len(), 1);
        assert_eq!(texts(&frame)[0], "  › abc");
    }

    #[test]
    fn long_lines_soft_wrap_upward_and_the_cursor_is_always_on_screen() {
        // Width 20: a 14-cell measure after the pads and the prompt.
        let mut c = composer("alpha beta gamma delta epsilon");
        let frame = render(&c, Mode::Idle, 20, 8);
        assert_eq!(
            texts(&frame)[..3],
            ["  › alpha beta", "    gamma delta", "    epsilon"]
        );
        assert_eq!(frame.cursor, Some((11, 2)), "after `epsilon`");
        // The break keeps the space on the upper row, so every char has a cell: a cursor on
        // the first char of a wrapped row sits at that row's start.
        c.cursor = "alpha beta ".len();
        assert_eq!(render(&c, Mode::Idle, 20, 8).cursor, Some((4, 1)));
        // Capped rows keep the cursor's row; the rows above it scroll away.
        let frame = render(
            &composer("alpha beta gamma delta epsilon"),
            Mode::Idle,
            20,
            3,
        );
        assert_eq!(texts(&frame)[..2], ["    gamma delta", "    epsilon"]);
        assert_eq!(frame.cursor, Some((11, 1)));
        // A word longer than the measure breaks hard at the cell edge.
        let long = "x".repeat(30);
        let frame = render(&composer(&long), Mode::Idle, 20, 8);
        assert_eq!(frame.lines.len(), 4, "14 + 14 + 2, then the hints");
        assert_eq!(frame.cursor, Some((6, 2)));
        for line in &frame.lines {
            assert_eq!(cell_width(&line.to_string()), 20);
        }
    }

    #[test]
    fn wide_chars_wrap_by_cells() {
        let frame = render(&composer("界界界界界界界界"), Mode::Idle, 20, 8);
        assert_eq!(texts(&frame)[..2], ["  › 界界界界界界界", "    界"]);
        assert_eq!(frame.cursor, Some((6, 1)));
    }
}
