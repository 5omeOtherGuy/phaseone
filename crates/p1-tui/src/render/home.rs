//! Static synapse mark for the unused welcome area.
use crate::palette;
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

const GLYPH_P: [&str; 9] = [
    "00000", "00000", "11110", "10001", "10001", "10001", "11110", "10000", "10000",
];
const GLYPH_ONE: [&str; 9] = [
    "00100", "01100", "00100", "00100", "00100", "00100", "01110", "00000", "00000",
];

fn centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: Color) {
    let width = text.chars().count() as u16;
    if width <= area.width && y < area.bottom() {
        buf.set_string(
            area.x + (area.width - width) / 2,
            y,
            text,
            ratatui::style::Style::new().fg(color).bg(palette::GROUND),
        );
    }
}

pub(super) fn draw(area: Rect, buf: &mut Buffer) {
    if area.height < 5 || area.width < 11 {
        return;
    }
    let show_mark = area.width >= 19 && area.height >= 13;
    let step_y = if area.width >= 37 && area.height >= 21 {
        2
    } else {
        1
    };
    let step_x = step_y * 2;
    let mark_height = if show_mark { 8 * step_y + 1 } else { 1 };
    let top = area.y + (area.height - (mark_height + 4)) / 2;
    if show_mark {
        let left = area.x + (area.width - (9 * step_x + 1)) / 2;
        // Same node coordinates and cardinal connections as the approved SVG.
        for (letter, rows) in [GLYPH_P, GLYPH_ONE].iter().enumerate() {
            for (y, row) in rows.iter().enumerate() {
                for (x, byte) in row.bytes().enumerate() {
                    if byte != b'1' {
                        continue;
                    }
                    let px = left + (x + letter * 6) as u16 * step_x;
                    let py = top + y as u16 * step_y;
                    buf[(px, py)].set_char('●').set_fg(palette::INK);
                    if row.as_bytes().get(x + 1) == Some(&b'1') {
                        for dx in 1..step_x {
                            buf[(px + dx, py)].set_char('·').set_fg(palette::DIM);
                        }
                    }
                    if rows.get(y + 1).is_some_and(|r| r.as_bytes()[x] == b'1') {
                        for dy in 1..step_y {
                            buf[(px, py + dy)].set_char('·').set_fg(palette::DIM);
                        }
                    }
                }
            }
        }
    } else {
        centered(buf, area, top, "p1", palette::INK);
    }
    centered(buf, area, top + mark_height + 1, "phaseone", palette::INK);
    centered(
        buf,
        area,
        top + mark_height + 3,
        "We love pie",
        palette::DIM,
    );
}
