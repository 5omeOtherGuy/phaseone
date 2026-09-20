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
