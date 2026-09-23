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

pub fn layout(w: u16, h: u16, pane: PaneWidth, focus: bool, composer_rows: u16) -> Layout {
    let active = !focus && w >= 100 && pane != PaneWidth::Off;
    let mut p = if active {
        match pane {
            PaneWidth::Ch40 => 38,
            PaneWidth::Ch56 => 56,
            PaneWidth::Split => (w.saturating_sub(6) / 2) as usize,
            PaneWidth::Off => 0,
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
    let (top, gap_c, gap_s, bottom) = if h >= 30 {
        (1, 1, 1, 1)
    } else if h >= 20 {
        (0, 1, 0, 0)
    } else {
        (0, 0, 0, 0)
    };
    let cr = composer_rows.min(if h <= 12 { 1 } else { 2 });
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
        composer: Rect::new(
            x,
            comp_y,
            w.saturating_sub(4 + if p > 0 { p as u16 + 2 } else { 0 }),
            cr,
        ),
        statusline: Rect::new(x, status_y, w.saturating_sub(4), 1),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn column_table() {
        for (w, pane, t, p) in [
            (80, PaneWidth::Ch40, 76, 0),
            (100, PaneWidth::Ch40, 56, 38),
            (120, PaneWidth::Ch40, 76, 38),
            (120, PaneWidth::Ch56, 58, 56),
            (160, PaneWidth::Ch56, 98, 56),
            (200, PaneWidth::Ch56, 120, 74),
            (240, PaneWidth::Ch56, 120, 114),
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
}
