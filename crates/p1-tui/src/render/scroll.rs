//! Queue rows (handoff §8.2) and the scroll mark (§8.3): the two transcript-area rows that sit
//! directly above the composer gap. Queued input is FAINT on GROUND — it has not happened yet;
//! the scroll mark is a BLOCK row that replaces the last transcript row while scrolled back.

use std::collections::VecDeque;

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::palette;
use crate::state::Queued;
use crate::wrap::cell_width;

/// `steering` and `follow-up` share one label column so the texts line up.
const KIND_FIELD: usize = 11;

/// One row per queued input, oldest first. The text is cut with `…` at the row (Band rule);
/// the right side says when it lands and never truncates.
pub fn queue_rows(queued: &VecDeque<Queued>, width: usize) -> Vec<Line<'static>> {
    queued
        .iter()
        .map(|q| {
            let (kind, lands) = if q.follow_up {
                ("follow-up", "after this turn")
            } else {
                ("steering", "next boundary")
            };
            let kind = format!(
                "{kind}{}",
                " ".repeat(KIND_FIELD.saturating_sub(cell_width(kind)))
            );
            // A queued ⌥⏎ newline must not break the one-row shape.
            let text = q.text.split('\n').collect::<Vec<_>>().join(" ");
            Band {
                bg: palette::GROUND,
                left: vec![
                    Seg::new(palette::FAINT, "· "),
                    Seg::new(palette::FAINT, kind),
                    Seg::new(palette::FAINT, text),
                ],
                right: vec![Seg::new(palette::FAINT, lands)],
                width,
                pad: 2,
            }
            .render()
        })
        .collect()
}

/// What the scroll mark reports while the view is pinned above the live tail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScrollMark {
    /// Transcript rows below the view.
    pub below: usize,
    /// The running tool (`shell running`) when a turn is live.
    pub running: Option<String>,
    /// 1-based absolute row at the top of the view, and the transcript's total rows.
    pub row: usize,
    pub total: usize,
}

impl ScrollMark {
    pub fn line(&self, width: usize) -> Line<'static> {
        let noun = if self.below == 1 { "row" } else { "rows" };
        let mut left = vec![
            Seg::new(palette::FAINT, "· "),
            Seg::new(palette::INK, self.below.to_string()),
            Seg::new(palette::DIM, format!(" new {noun} below")),
        ];
        if let Some(running) = &self.running {
            left.push(Seg::new(palette::DIM, " · "));
            left.push(Seg::new(palette::LIVE, "▸"));
            left.push(Seg::new(palette::DIM, format!(" {running}")));
        }
        Band {
            bg: palette::BLOCK,
            left,
            right: vec![Seg::new(
                palette::FAINT,
                format!("row {} of {}   esc live tail", self.row, self.total),
            )],
            width,
            pad: 2,
        }
        .render()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_long_queued_text_is_cut_and_its_landing_survives() {
        let queued = VecDeque::from([Queued {
            follow_up: false,
            text: "x".repeat(200),
        }]);
        let row = queue_rows(&queued, 40)[0].to_string();
        assert_eq!(cell_width(&row), 40);
        assert!(row.contains("…  next boundary"), "{row}");
    }

    #[test]
    fn the_mark_omits_the_running_tool_when_idle() {
        let mark = ScrollMark {
            below: 1,
            running: None,
            row: 3,
            total: 9,
        };
        let text = mark.line(60).to_string();
        assert!(text.starts_with("  · 1 new row below "), "{text}");
        assert!(text.trim_end().ends_with("row 3 of 9   esc live tail"));
    }
}
