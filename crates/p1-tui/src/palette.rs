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
    // Renderers draw 24-bit already; a truecolor terminal gets the frame untouched.
    if mode == ColorMode::TrueColor {
        return;
    }
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

// Adapted from iris-donor/src/ui/palette.rs at pin
// 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa (MIT).
fn nearest_cube_component(value: u8) -> (u8, u8) {
    const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
    LEVELS
        .iter()
        .copied()
        .enumerate()
        .min_by_key(|(_, level)| value.abs_diff(*level))
        .map(|(index, level)| (index as u8, level))
        .unwrap_or((0, 0))
}

fn distance_sq(a: (u8, u8, u8), b: (u8, u8, u8)) -> u32 {
    let dr = i32::from(a.0) - i32::from(b.0);
    let dg = i32::from(a.1) - i32::from(b.1);
    let db = i32::from(a.2) - i32::from(b.2);
    (dr * dr + dg * dg + db * db) as u32
}

fn rgb_to_xterm(r: u8, g: u8, b: u8) -> u8 {
    let (ri, rv) = nearest_cube_component(r);
    let (gi, gv) = nearest_cube_component(g);
    let (bi, bv) = nearest_cube_component(b);
    let cube_index = 16 + 36 * ri + 6 * gi + bi;
    let cube_distance = distance_sq((r, g, b), (rv, gv, bv));

    let mean = (u16::from(r) + u16::from(g) + u16::from(b)) / 3;
    let gray_slot = mean.saturating_sub(8).saturating_add(5) / 10;
    let gray_slot = gray_slot.min(23) as u8;
    let gray = 8 + 10 * gray_slot;
    let gray_distance = distance_sq((r, g, b), (gray, gray, gray));
    if gray_distance < cube_distance {
        232 + gray_slot
    } else {
        cube_index
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
        _ => rgb_to_xterm(r, g, b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::{buffer::Buffer, layout::Rect, style::Modifier};
    use std::collections::HashMap;

    const SLAB_TOKEN_INDICES: [(Color, u8); 17] = [
        (GROUND, 232),
        (BLOCK, 233),
        (BLOCK_PLUS, 234),
        (RULE, 235),
        (DIM, 247),
        (FAINT, 242),
        (INK, 254),
        (ATTN, 179),
        (FAIL, 167),
        (OK, 107),
        (LIVE, 73),
        (REF, 110),
        (SYNTAX, 176),
        (DIFF_ADD_BG, 22),
        (DIFF_ADD_FG, 194),
        (DIFF_DEL_BG, 52),
        (DIFF_DEL_FG, 224),
    ];

    fn main_cube_index(r: u8, g: u8, b: u8) -> u8 {
        let cv = |value: u8| {
            if value < 48 {
                0
            } else if value < 115 {
                1
            } else {
                ((value as usize - 35) / 40).min(5)
            }
        };
        (16 + 36 * cv(r) + 6 * cv(g) + cv(b)) as u8
    }

    fn indexed_rgb(index: u8) -> (u8, u8, u8) {
        const LEVELS: [u8; 6] = [0, 95, 135, 175, 215, 255];
        if (16..232).contains(&index) {
            let offset = usize::from(index - 16);
            (
                LEVELS[offset / 36],
                LEVELS[offset / 6 % 6],
                LEVELS[offset % 6],
            )
        } else {
            let grey = 8 + 10 * (index - 232);
            (grey, grey, grey)
        }
    }

    fn squared_distance(rgb: (u8, u8, u8), other: (u8, u8, u8)) -> u32 {
        let channel_distance = |a: u8, b: u8| {
            let difference = i32::from(a) - i32::from(b);
            (difference * difference) as u32
        };
        channel_distance(rgb.0, other.0)
            + channel_distance(rgb.1, other.1)
            + channel_distance(rgb.2, other.2)
    }

    #[test]
    fn slab_token_indices_are_unchanged() {
        for (color, expected) in SLAB_TOKEN_INDICES {
            let Color::Rgb(r, g, b) = color else {
                unreachable!()
            };
            assert_eq!(index(r, g, b), expected, "{color:?}");
        }
    }

    // Adapted from iris-donor/src/ui/palette.rs at pin
    // 5b04a1ad3412ad0bb663b6355f77a024aec0ddfa (MIT).
    #[test]
    fn xterm_quantizer_keeps_primary_and_grayscale_anchors() {
        assert_eq!(index(255, 0, 0), 196);
        assert_eq!(index(128, 128, 128), 244);
        assert_eq!(index(0x30, 0x30, 0x30), 236);
        assert_eq!(index(0x00, 0x87, 0xff), 33);
    }

    #[test]
    fn quantizer_is_never_farther_than_the_cube() {
        let values = || (0..=250).step_by(5).chain(std::iter::once(255));
        for r in values() {
            for g in values() {
                for b in values() {
                    let rgb = (r, g, b);
                    if SLAB_TOKEN_INDICES
                        .iter()
                        .any(|(color, _)| *color == Color::Rgb(r, g, b))
                    {
                        continue;
                    }
                    let cube_distance =
                        squared_distance(rgb, indexed_rgb(main_cube_index(r, g, b)));
                    let quantized_distance = squared_distance(rgb, indexed_rgb(index(r, g, b)));
                    assert!(
                        quantized_distance <= cube_distance,
                        "{rgb:?}: {quantized_distance} > {cube_distance}"
                    );
                }
            }
        }
        let grey = (128, 128, 128);
        assert!(
            squared_distance(grey, indexed_rgb(index(128, 128, 128)))
                < squared_distance(grey, indexed_rgb(main_cube_index(128, 128, 128)))
        );
    }

    #[test]
    fn ansi256_uses_the_grey_ramp_for_slab_background() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 1, 1));
        buffer[(0, 0)].set_bg(Color::Rgb(0x30, 0x30, 0x30));
        degrade(&mut buffer, ColorMode::Ansi256);
        assert_eq!(buffer[(0, 0)].bg, Color::Indexed(236));
    }

    #[test]
    fn truecolor_leaves_the_frame_untouched() {
        let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
        buffer[(0, 0)].set_fg(ATTN).set_bg(BLOCK_PLUS);
        let before = buffer.clone();
        degrade(&mut buffer, ColorMode::TrueColor);
        assert_eq!(buffer, before);
    }

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
