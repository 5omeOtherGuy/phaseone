//! The home screen (handoff §6.11): the prelude — version line, one sentence of state, the
//! affordances — on the Band rule at the text column, and the monogram: the `p1` node layout
//! drawn as BLOCK+ cells in the unused transcript rows, with the name and the motto below.
use crate::band::{Band, Seg};
use crate::palette;
use ratatui::{buffer::Buffer, layout::Rect, style::Color, text::Line};

/// The command field of an affordance row (`/resume` + 5 spaces).
const COMMAND_FIELD: usize = 12;

/// What the idle prelude says. The host fills it; nothing here knows a command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct HomePrelude {
    pub version: String,
    /// The workspace as the operator knows it (`~/dev/phaseone`).
    pub path: String,
    /// Omitted outside git.
    pub branch: Option<String>,
    /// One sentence of state per row, pre-styled (`✗` a missing login, ink text by default).
    pub state: Vec<Vec<Seg>>,
    /// Affordances: command, what it does.
    pub items: Vec<(String, String)>,
}

impl HomePrelude {
    /// The prelude rows at the transcript width `width`: the version line, a blank row, the
    /// state, a blank row, the affordances. Every row is a GROUND band, so an over-long row
    /// ends in `…` instead of wrapping.
    pub fn lines(&self, width: usize) -> Vec<Line<'static>> {
        let row = |left: Vec<Seg>| {
            Band {
                bg: palette::GROUND,
                left,
                right: Vec::new(),
                width,
                pad: 2,
            }
            .render()
        };
        let mut head = vec![
            Seg::new(palette::INK, format!("p1 {}", self.version)),
            Seg::new(palette::DIM, "   "),
            Seg::new(palette::REF, self.path.clone()),
        ];
        if let Some(branch) = &self.branch {
            head.push(Seg::new(palette::DIM, format!("   {branch}")));
        }
        let mut out = vec![row(head)];
        if !self.state.is_empty() {
            out.push(row(Vec::new()));
            out.extend(self.state.iter().map(|segs| row(segs.clone())));
        }
        if !self.items.is_empty() {
            out.push(row(Vec::new()));
            for (command, description) in &self.items {
                let pad = COMMAND_FIELD.saturating_sub(crate::wrap::cell_width(command));
                out.push(row(vec![
                    Seg::new(palette::INK, format!("{command}{}", " ".repeat(pad.max(1)))),
                    Seg::new(palette::DIM, description.clone()),
                ]));
            }
        }
        out
    }
}

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
