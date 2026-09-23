//! The pane frame pieces shared by every mode (handoff §9.1): the mode strip
//! (the pane's last row) and the peek banner (2 rows over the pane's top).
//! Content rows for each mode live in `ledger.rs` / `output.rs` /
//! `workers.rs`; this module only draws the frame around them. Mode
//! *availability* and the peek's lifetime are state, not rendering — they
//! stay in `state.rs` (§9.1's promotion rules); this module only turns
//! already-decided data into cells.

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::palette;
use crate::state::PaneMode;

/// The pane's last row (§9.1): available modes left (the current one dim,
/// the others faint), right `^Tab` — or `pinned ^P` while `^P` holds the
/// mode (verified against the S01/W01 screen mocks: `tests/slab_pane.rs`).
pub fn mode_strip(
    width: usize,
    available: &[PaneMode],
    current: PaneMode,
    pinned: bool,
) -> Line<'static> {
    let mut left = Vec::with_capacity(available.len());
    for (n, mode) in available.iter().enumerate() {
        let fg = if *mode == current {
            palette::DIM
        } else {
            palette::FAINT
        };
        let text = if n == 0 {
            mode_label(*mode).to_string()
        } else {
            format!("  {}", mode_label(*mode))
        };
        left.push(Seg::new(fg, text));
    }
    let right_text = if pinned { "pinned ^P" } else { "^Tab" };
    Band {
        bg: palette::BLOCK,
        left,
        right: vec![Seg::new(palette::FAINT, right_text.to_string())],
        width,
        pad: 4,
    }
    .render()
}

fn mode_label(mode: PaneMode) -> &'static str {
    match mode {
        PaneMode::Ledger => "ledger",
        PaneMode::Output => "output",
        PaneMode::Diff => "diff",
        PaneMode::Workers => "workers",
    }
}

/// The peek banner (§9.1): 2 BLOCK+ rows over the pane's top, for a failed
/// tool or context at the summarize threshold. `state.rs` decides whether
/// one is showing at all (never over a pinned or blocked pane) and how long
/// it lives (3 s); this only draws the two given lines.
pub fn peek(
    width: usize,
    title: &str,
    detail: &str,
    remaining_s: Option<u64>,
) -> Vec<Line<'static>> {
    let right = match remaining_s {
        Some(s) => vec![Seg::new(palette::FAINT, format!("{s}s"))],
        None => vec![],
    };
    vec![
        Band {
            bg: palette::BLOCK_PLUS,
            left: vec![
                Seg::new(palette::FAIL, "✗".to_string()),
                Seg::new(palette::INK, format!(" {title}")),
            ],
            right,
            width,
            pad: 4,
        }
        .render(),
        Band {
            bg: palette::BLOCK_PLUS,
            left: vec![Seg::new(palette::DIM, detail.to_string())],
            right: vec![],
            width,
            pad: 4,
        }
        .render(),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_current_mode_is_dim_the_rest_are_faint() {
        let line = mode_strip(
            38,
            &[PaneMode::Ledger, PaneMode::Output, PaneMode::Workers],
            PaneMode::Ledger,
            false,
        );
        assert_eq!(line.to_string(), "    ledger  output  workers   ^Tab    ");
    }

    #[test]
    fn pinned_replaces_the_tab_hint() {
        let line = mode_strip(
            56,
            &[PaneMode::Ledger, PaneMode::Output, PaneMode::Workers],
            PaneMode::Workers,
            true,
        );
        assert_eq!(
            line.to_string(),
            "    ledger  output  workers                pinned ^P    "
        );
    }

    #[test]
    fn the_peek_is_two_rows_glyph_and_countdown() {
        let lines = peek(
            38,
            "shell failed",
            "exit 101 · hard_pressure_waits",
            Some(3),
        );
        assert_eq!(lines.len(), 2);
        assert_eq!(
            lines[0].to_string(),
            "    ✗ shell failed              3s    "
        );
        assert_eq!(
            lines[1].to_string(),
            "    exit 101 · hard_pressure_waits    "
        );
    }
}
