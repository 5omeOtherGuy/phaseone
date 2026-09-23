use crate::state::PaneWidth;
use ratatui::layout::Rect;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub transcript: Rect,
    pub gutter: Rect,
    pub pane: Rect,
    pub composer: Rect,
    pub statusline: Rect,
}

/// The §4 screen geometry. `composer_rows` is what the composer wants (0 while hidden): one row
/// at H ≤ 12, otherwise all of them — a wrapped or multiline composer grows upward, and the
/// caller caps it (a third of the screen).
pub fn layout(w: u16, h: u16, pane: PaneWidth, focus: bool, composer_rows: u16) -> Layout {
    let active = !focus && w >= 100 && pane != PaneWidth::Off;
    let mut p = if active {
        match pane.resolve(w) {
            PaneWidth::Narrow => 38,
            PaneWidth::Wide => 56,
            PaneWidth::Split => (w.saturating_sub(6) / 2) as usize,
            PaneWidth::Off | PaneWidth::Auto => 0,
        }
    } else {
        0
    };
    if p > 0 && (w as usize).saturating_sub(6 + p) < 56 {
        p = if w >= 100 { 38 } else { 0 };
    }
    if p > 0 && (w as usize).saturating_sub(6 + p) < 56 {
        p = 0;
    }
    let mut t = if p > 0 {
        w as usize - 6 - p
    } else {
        (w as usize).saturating_sub(4).min(120)
    };
    if p > 0 && t > 120 {
        p += t - 120;
        t = 120;
    }
    let (top, mut gap_c, gap_s, bottom) = if h >= 30 {
        (1, 1, 1, 1)
    } else if h >= 20 {
        (0, 1, 0, 0)
    } else {
        (0, 0, 0, 0)
    };
    let cr = if h <= 12 {
        composer_rows.min(1)
    } else {
        composer_rows
    };
    // A hidden composer takes its gap with it: the transcript runs down to the statusline gap
    // (F01: focus mode at 120×40 shows transcript rows 1–36).
    if cr == 0 {
        gap_c = 0;
    }
    let status_y = h.saturating_sub(1 + bottom);
    let comp_y = status_y.saturating_sub(gap_s + cr);
    let transcript_y = top;
    let transcript_h = comp_y.saturating_sub(gap_c + transcript_y);
    let x = 2u16;
    let tx = t.min(u16::MAX as usize) as u16;
    let px = if p > 0 {
        x.saturating_add(tx).saturating_add(2)
    } else {
        0
    };
    Layout {
        transcript: Rect::new(x, transcript_y, tx, transcript_h),
        gutter: Rect::new(
            if p > 0 { x + tx } else { 0 },
            transcript_y,
            if p > 0 { 2 } else { 0 },
            transcript_h,
        ),
        pane: Rect::new(
            px,
            top,
            p as u16,
            if p > 0 {
                comp_y.saturating_add(cr).saturating_sub(top)
            } else {
                0
            },
        ),
        // The composer is exactly as wide as the transcript column, also past the 120 cap.
        composer: Rect::new(x, comp_y, tx, cr),
        statusline: Rect::new(x, status_y, w.saturating_sub(4), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn column_table() {
        for (w, pane, t, p) in [
            (80, PaneWidth::Narrow, 76, 0),
            (100, PaneWidth::Narrow, 56, 38),
            (120, PaneWidth::Narrow, 76, 38),
            (120, PaneWidth::Wide, 58, 56),
            (160, PaneWidth::Wide, 98, 56),
            (200, PaneWidth::Wide, 120, 74),
            (240, PaneWidth::Wide, 120, 114),
            // The start state (§4.1): narrow below 160 columns, wide from 160.
            (120, PaneWidth::Auto, 76, 38),
            (159, PaneWidth::Auto, 115, 38),
            (160, PaneWidth::Auto, 98, 56),
            // Wide and split fall back to narrow where the transcript would drop under 56.
            (110, PaneWidth::Wide, 66, 38),
            (110, PaneWidth::Split, 66, 38),
        ] {
            let l = layout(w, 40, pane, false, 2);
            assert_eq!((l.transcript.width, l.pane.width), (t, p), "{w} {pane:?}");
        }
    }
    #[test]
    fn height_bands() {
        for (h, ty, th, cy, sy) in [
            (40, 1, 33, 35, 38),
            (30, 1, 23, 25, 28),
            (24, 0, 20, 21, 23),
            (20, 0, 16, 17, 19),
            (19, 0, 16, 16, 18),
            (13, 0, 10, 10, 12),
            (12, 0, 10, 10, 11),
            (8, 0, 6, 6, 7),
        ] {
            let l = layout(120, h, PaneWidth::Off, false, 2);
            assert_eq!(
                (
                    l.transcript.y,
                    l.transcript.height,
                    l.composer.y,
                    l.statusline.y
                ),
                (ty, th, cy, sy),
                "height {h}"
            );
        }
    }
    #[test]
    fn a_hidden_composer_gives_its_rows_and_gap_to_the_transcript() {
        let l = layout(120, 40, PaneWidth::Off, true, 0);
        assert_eq!((l.transcript.y, l.transcript.height), (1, 36));
        assert_eq!(l.statusline.y, 38);
        let l = layout(120, 12, PaneWidth::Off, true, 0);
        assert_eq!((l.transcript.y, l.transcript.height), (0, 11));
    }
    #[test]
    fn a_taller_composer_grows_upward_and_the_pane_keeps_its_bottom() {
        let two = layout(120, 40, PaneWidth::Narrow, false, 2);
        let three = layout(120, 40, PaneWidth::Narrow, false, 3);
        assert_eq!((three.composer.y, three.composer.height), (34, 3));
        assert_eq!(three.transcript.height, two.transcript.height - 1);
        assert_eq!(three.pane.bottom(), two.pane.bottom());
        // At H ≤ 12 the composer is one row whatever it holds.
        assert_eq!(layout(120, 12, PaneWidth::Off, false, 3).composer.height, 1);
    }
}
