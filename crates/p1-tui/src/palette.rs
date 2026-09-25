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

/// The one highlight: selection and focus invert ink and ground.
pub const SELECTION_BG: Color = INK;
pub const SELECTION_FG: Color = GROUND;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum ColorMode {
    #[default]
    TrueColor,
    Indexed,
    Plain,
}

impl ColorMode {
    pub fn from_env() -> Self {
        // The NO_COLOR convention: set to a non-empty value.
        if std::env::var_os("NO_COLOR").is_some_and(|v| !v.is_empty()) {
            return Self::Plain;
        }
        let color = std::env::var("COLORTERM").unwrap_or_default();
        let term = std::env::var("TERM").unwrap_or_default();
        if color.contains("truecolor") || color.contains("24bit") {
            Self::TrueColor
        } else if term.contains("256color") {
            Self::Indexed
        } else {
            Self::Plain
        }
    }
    pub fn apply(self, buffer: &mut ratatui::buffer::Buffer) {
        fn index(color: Color) -> Color {
            // Keep both reserved diff hues distinct on the small xterm cube.
            if color == DIFF_ADD_BG {
                return Color::Indexed(22);
            }
            if color == DIFF_DEL_BG {
                return Color::Indexed(52);
            }
            let Color::Rgb(r, g, b) = color else {
                return color;
            };
            if r == g && g == b {
                return Color::Indexed(if r < 8 {
                    16
                } else if r > 248 {
                    231
                } else {
                    232 + (r - 8) / 10
                });
            }
            let cube = |v: u8| ((u16::from(v) * 5 + 127) / 255) as u8;
            Color::Indexed(16 + 36 * cube(r) + 6 * cube(g) + cube(b))
        }
        for cell in &mut buffer.content {
            match self {
                Self::TrueColor => {}
                Self::Indexed => {
                    cell.fg = index(cell.fg);
                    cell.bg = index(cell.bg);
                }
                Self::Plain => {
                    // Without colour, an inverted cell (decision keys, the
                    // selection, the route chip) stays inverted as reverse
                    // video, and an underline stays: every state reads (§10.1).
                    use ratatui::style::Modifier;
                    let inverted = cell.bg == INK && cell.fg == GROUND;
                    let underlined = cell.modifier.contains(Modifier::UNDERLINED);
                    cell.fg = Color::Reset;
                    cell.bg = Color::Reset;
                    cell.modifier = Modifier::empty();
                    if inverted {
                        cell.modifier.insert(Modifier::REVERSED);
                    }
                    if underlined {
                        cell.modifier.insert(Modifier::UNDERLINED);
                    }
                }
            }
        }
    }
}
