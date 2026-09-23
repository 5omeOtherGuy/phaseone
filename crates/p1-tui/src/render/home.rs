//! The home monogram (handoff §6.11): the `p1` node layout drawn as BLOCK+
//! cells in the unused transcript rows, with the name and the motto below.
use crate::palette;
use ratatui::{buffer::Buffer, layout::Rect, style::Color};

const GLYPH_P: [&str; 9] = [
    "00000", "00000", "11110", "10001", "10001", "10001", "11110", "10000", "10000",
];
const GLYPH_ONE: [&str; 9] = [
    "00100", "01100", "00100", "00100", "00100", "00100", "01110", "00000", "00000",
];

/// Each node is two cells wide, so it reads square in a terminal cell grid.
const NODE_CELLS: u16 = 2;
/// Two 5-node letters, one node apart.
const MARK_WIDTH: u16 = (5 + 1 + 5) * NODE_CELLS;
const MARK_HEIGHT: u16 = GLYPH_P.len() as u16;
/// The mark, a blank row, `phaseone`, a blank row, the motto.
const HOME_HEIGHT: u16 = MARK_HEIGHT + 4;
/// Below this free area there is no mark and no words (§6.11).
const MIN_WIDTH: u16 = 30;
const MIN_HEIGHT: u16 = 14;

fn centered(buf: &mut Buffer, area: Rect, y: u16, text: &str, color: Color) {
    let width = crate::wrap::cell_width(text) as u16;
    if width <= area.width && y < area.bottom() {
        buf.set_string(
            area.x + (area.width - width) / 2,
            y,
            text,
            ratatui::style::Style::new().fg(color).bg(palette::GROUND),
        );
    }
}

/// Draw the monogram centred in `area`, the free transcript rows. The mark is
/// background only — no glyph — so NO_COLOR, which drops backgrounds, shows no mark.
pub fn draw(area: Rect, buf: &mut Buffer) {
    if area.width < MIN_WIDTH || area.height < MIN_HEIGHT {
        return;
    }
    let top = area.y + (area.height - HOME_HEIGHT) / 2;
    let left = area.x + (area.width - MARK_WIDTH) / 2;
    for (letter, rows) in [GLYPH_P, GLYPH_ONE].iter().enumerate() {
        for (y, row) in rows.iter().enumerate() {
            for (x, byte) in row.bytes().enumerate() {
                if byte != b'1' {
                    continue;
                }
                let px = left + (x + letter * 6) as u16 * NODE_CELLS;
                for dx in 0..NODE_CELLS {
                    buf[(px + dx, top + y as u16)]
                        .set_char(' ')
                        .set_bg(palette::BLOCK_PLUS);
                }
            }
        }
    }
    centered(buf, area, top + MARK_HEIGHT + 1, "phaseone", palette::INK);
    centered(
        buf,
        area,
        top + MARK_HEIGHT + 3,
        "We love pie",
        palette::DIM,
    );
}
