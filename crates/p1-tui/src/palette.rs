//! The SPEC §1 palette, verbatim. Monochrome: two hues exist and are reserved
//! for diff bodies only. Selection and focus INVERT (ink on ground swapped);
//! that is the only highlight mechanism in the interface.

use ratatui::style::Color;

/// Transcript background, terminal default bg.
pub const GROUND: Color = Color::Rgb(0x0a, 0x0a, 0x0a);
/// Right pane, folded-output blocks.
pub const BLOCK: Color = Color::Rgb(0x12, 0x12, 0x12);
/// Peek banner, inline command echo.
pub const BLOCK_PLUS: Color = Color::Rgb(0x1c, 0x1c, 0x1c);
/// Unfilled bar segments.
pub const RULE: Color = Color::Rgb(0x2a, 0x2a, 0x2a);
/// Primary text, values, active symbols.
pub const INK: Color = Color::Rgb(0xe8, 0xe8, 0xe8);
/// Labels, secondary text, settled tool results.
pub const DIM: Color = Color::Rgb(0x9a, 0x9a, 0x9a);
/// Key hints, line numbers, fold metadata, unavailable rows, pending items,
/// timestamps. 3.4:1 — legal ONLY for those six categories (SPEC §1).
pub const FAINT: Color = Color::Rgb(0x6a, 0x6a, 0x6a);
/// Added diff lines: background / foreground.
pub const DIFF_ADD_BG: Color = Color::Rgb(0x3a, 0x4a, 0x3a);
pub const DIFF_ADD_FG: Color = Color::Rgb(0xd8, 0xe8, 0xd0);
/// Removed diff lines: background / foreground.
pub const DIFF_DEL_BG: Color = Color::Rgb(0x4a, 0x35, 0x35);
pub const DIFF_DEL_FG: Color = Color::Rgb(0xe8, 0xd0, 0xd0);

/// SLAB signal and reference tokens.
pub const ATTN: Color = Color::Rgb(0xe2, 0xa0, 0x3f);
pub const FAIL: Color = Color::Rgb(0xe0, 0x70, 0x5f);
pub const OK: Color = Color::Rgb(0x8f, 0xb5, 0x73);
pub const LIVE: Color = Color::Rgb(0x72, 0xb8, 0xb0);
pub const REF: Color = Color::Rgb(0x7f, 0xa7, 0xd6);
pub const SYNTAX: Color = Color::Rgb(0xc0, 0x8f, 0xc8);
pub const AMBER_FILL: Color = ATTN;
pub const INK_FILL: Color = INK;
pub const ON_FILL: Color = GROUND;

/// Compatibility aliases used by renderers not yet migrated from SPEC.
pub const SELECTION_BG: Color = INK;
pub const SELECTION_FG: Color = GROUND;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColorMode {
    TrueColor,
    Ansi256,
    NoColor,
}

impl ColorMode {
    pub fn detect(env: &std::collections::HashMap<String, String>) -> Self {
        if env.contains_key("NO_COLOR") {
            return Self::NoColor;
        }
        if matches!(
            env.get("COLORTERM").map(String::as_str),
            Some("truecolor" | "24bit")
        ) {
            return Self::TrueColor;
        }
        Self::Ansi256
    }
}

/// Reduce terminal attributes only after a complete frame has been rendered.
pub fn degrade(buffer: &mut ratatui::buffer::Buffer, mode: ColorMode) {
    use ratatui::style::Modifier;
    for cell in buffer.content.iter_mut() {
        if mode == ColorMode::NoColor {
            if cell.fg == FAINT {
                cell.modifier |= Modifier::DIM;
            }
            if cell.bg == ATTN || cell.bg == INK {
                cell.modifier |= Modifier::REVERSED;
            }
            if cell.symbol() == "█" && cell.fg == RULE {
                cell.set_symbol(" ");
            }
            cell.fg = Color::Reset;
            cell.bg = Color::Reset;
            continue;
        }
        if let Color::Rgb(r, g, b) = cell.fg {
            cell.fg = Color::Indexed(index(r, g, b));
        }
        if let Color::Rgb(r, g, b) = cell.bg {
            cell.bg = Color::Indexed(index(r, g, b));
        }
    }
}

fn index(r: u8, g: u8, b: u8) -> u8 {
    match (r, g, b) {
        (10, 10, 10) => 232,
        (18, 18, 18) => 233,
        (28, 28, 28) => 234,
        (42, 42, 42) => 235,
        (0x9a, 0x9a, 0x9a) => 247,
        (0x6a, 0x6a, 0x6a) => 242,
        (0xe8, 0xe8, 0xe8) => 254,
        (0xe2, 0xa0, 0x3f) => 179,
        (0xe0, 0x70, 0x5f) => 167,
        (0x8f, 0xb5, 0x73) => 107,
        (0x72, 0xb8, 0xb0) => 73,
        (0x7f, 0xa7, 0xd6) => 110,
        (0xc0, 0x8f, 0xc8) => 176,
        (0x3a, 0x4a, 0x3a) => 22,
        (0xd8, 0xe8, 0xd0) => 194,
        (0x4a, 0x35, 0x35) => 52,
        (0xe8, 0xd0, 0xd0) => 224,
        _ => {
            let cv = |v: u8| {
                if v < 48 {
                    0
                } else if v < 115 {
                    1
                } else {
                    ((v as usize - 35) / 40).min(5)
                }
            };
            let (r, g, b) = (cv(r), cv(g), cv(b));
            (16 + 36 * r + 6 * g + b) as u8
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
    use std::collections::HashMap;

    #[test]
    fn color_detection_precedence() {
        let env = HashMap::from([
            ("NO_COLOR".into(), String::new()),
            ("COLORTERM".into(), "truecolor".into()),
        ]);
        assert_eq!(ColorMode::detect(&env), ColorMode::NoColor);
        assert_eq!(
            ColorMode::detect(&HashMap::from([("COLORTERM".into(), "24bit".into())])),
            ColorMode::TrueColor
        );
        assert_eq!(
            ColorMode::detect(&HashMap::from([("TERM".into(), "xterm-256color".into())])),
            ColorMode::Ansi256
        );
        assert_eq!(ColorMode::detect(&HashMap::new()), ColorMode::Ansi256);
    }

    #[test]
    fn degrade_maps_tokens_and_strips_color_with_modifiers() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 4, 1));
        buffer[(0, 0)].set_bg(GROUND);
        buffer[(1, 0)].set_bg(ATTN);
        buffer[(2, 0)].set_fg(FAINT);
        buffer[(2, 0)].set_char('x');
        buffer[(3, 0)].set_fg(RULE);
        buffer[(3, 0)].set_char('█');
        degrade(&mut buffer, ColorMode::Ansi256);
        assert_eq!(buffer[(0, 0)].bg, Color::Indexed(232));
        assert_eq!(buffer[(1, 0)].bg, Color::Indexed(179));
        buffer[(1, 0)].set_bg(ATTN);
        buffer[(2, 0)].set_fg(FAINT);
        buffer[(3, 0)].set_fg(RULE);
        degrade(&mut buffer, ColorMode::NoColor);
        assert_eq!(
            buffer[(1, 0)].modifier & Modifier::REVERSED,
            Modifier::REVERSED
        );
        assert_eq!(buffer[(2, 0)].modifier & Modifier::DIM, Modifier::DIM);
        assert_eq!(buffer[(3, 0)].symbol(), " ");
    }
}
