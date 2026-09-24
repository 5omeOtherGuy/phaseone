//! Independent #93 acceptance authored by wf1 verifier (DeepSeek V4.1 Flash).
//! Lead reviewed before implementation: strengthened cross-segment policy to
//! consume a side as one stream, so split OSC payloads cannot become visible.
//! Each side is independent; surviving text keeps its original segment style.
//! Tests reach the private sanitizer only through the real Band/Buffer consumer.

use p1_tui::band::{Band, Seg};
use p1_tui::palette::{BLOCK, INK, OK, REF};
use p1_tui::wrap::cell_width;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

fn left_only(raw: &str, sanitized: &str) -> Line<'static> {
    Band {
        bg: BLOCK,
        left: vec![Seg::new(INK, raw)],
        right: Vec::new(),
        width: cell_width(sanitized),
        pad: 0,
    }
    .render()
}

fn assert_row(raw: &str, sanitized: &str) {
    assert_eq!(
        left_only(raw, sanitized).to_string(),
        sanitized,
        "raw {raw:?}"
    );
}

#[test]
fn normal_bytes_pass_through_byte_for_byte() {
    for text in [
        "hello, world!",
        "  leading and trailing  ",
        "punctuation: a/b.rs:12 - ok",
        "cafe with an accent: caf\u{e9}",
        "\u{4f60}\u{597d}",
        "e\u{301}clair",
        "\u{1f642} emoji",
        "",
    ] {
        assert_row(text, text);
    }
}

#[test]
fn csi_is_stripped_in_7bit_and_c1_forms() {
    assert_row("\u{1b}[31mred\u{1b}[0m", "red");
    assert_row("a\u{1b}[1;38;5;208mb", "ab");
    assert_row("\u{1b}[?25lkeep", "keep");
    assert_row("\u{1b}[mempty-sgr", "empty-sgr");
    assert_row("\u{1b}[200~pasted\u{1b}[201~", "pasted");
    assert_row("\u{9b}31mc1", "c1");
    assert_row("x\u{9b}0my", "xy");
}

#[test]
fn osc_strings_end_at_bel_st_and_c1_st() {
    assert_row("\u{1b}]8;;https://a\u{7}txt\u{1b}]8;;\u{7}", "txt");
    assert_row("\u{1b}]0;title\u{1b}\\body", "body");
    assert_row("\u{1b}]0;title\u{9c}body", "body");
    assert_row("\u{9d}0;title\u{7}body", "body");
    assert_row("ding\u{7}", "ding");
    assert_row("\u{1b}[31ma\tb\u{1b}[0m", "a b");
}

#[test]
fn dcs_sos_pm_apc_are_stripped_in_both_forms() {
    assert_row("\u{1b}Passq\u{1b}\\after_dcs", "after_dcs");
    assert_row("\u{1b}Xstart\u{1b}\\after_sos", "after_sos");
    assert_row("\u{1b}^secret\u{1b}\\after_pm", "after_pm");
    assert_row("\u{1b}_apc\u{7}after_apc", "after_apc");
    assert_row("\u{90}dcs\u{9c}after_c1_dcs", "after_c1_dcs");
    assert_row("\u{98}sos\u{9c}after_c1_sos", "after_c1_sos");
    assert_row("\u{9e}pm\u{9c}after_c1_pm", "after_c1_pm");
    assert_row("\u{9f}apc\u{9c}after_c1_apc", "after_c1_apc");
}

#[test]
fn incomplete_controls_are_consumed_to_end_of_input() {
    assert_row("prefix\u{1b}[31", "prefix");
    assert_row("prefix\u{1b}]8;;http://x", "prefix");
    assert_row("prefix\u{1b}", "prefix");
    assert_row("prefix\u{1b}P", "prefix");
    assert_row("prefix\u{9b}31", "prefix");
    assert_row("\u{1b}[31m", "");
}

#[test]
fn a_tab_becomes_exactly_one_space() {
    assert_row("a\tb", "a b");
    assert_row("12345678\tb", "12345678 b");
    assert_row("\tleading", " leading");
    assert_row("trailing\t", "trailing ");
    assert_row("a\t\tb", "a  b");
}

#[test]
fn other_control_characters_are_removed() {
    for control in ['\0', '\r', '\n', '\u{8}', '\u{b}', '\u{c}', '\u{7f}'] {
        assert_row(&format!("a{control}b"), "ab");
    }
}

#[test]
fn every_non_control_byte_is_preserved_in_order() {
    let raw = "a\u{1b}[1mb\u{9b}0mc\u{7}d\te\u{1b}]0;t\u{7}f";
    assert_row(raw, "abcd ef");
}

#[test]
fn unicode_is_preserved_and_measured_in_cells_not_bytes() {
    let line = left_only("\u{1b}[31m你好\u{1b}[0m", "你好");
    assert_eq!(line.to_string(), "你好");
    assert_eq!(cell_width(&line.to_string()), 4);
    assert_row("\u{1b}[1m\u{1f642}\u{1b}[0m", "\u{1f642}");
    assert_row("e\u{301}\u{1b}[0mx", "e\u{301}x");
}

#[test]
fn supplied_styles_survive_sanitization() {
    let chip = Seg {
        fg: REF,
        bg: Some(INK),
        text: "x\u{1b}[31my".to_string(),
    };
    let line = Band {
        bg: BLOCK,
        left: vec![chip],
        right: Vec::new(),
        width: 2,
        pad: 0,
    }
    .render();
    assert_eq!(line.to_string(), "xy");
    assert_eq!(line.spans.len(), 1);
    assert_eq!(line.spans[0].style.fg, Some(REF));
    assert_eq!(line.spans[0].style.bg, Some(INK));
    let line = left_only("ok\u{1b}[0m", "ok");
    assert_eq!(line.spans[0].style.fg, Some(INK));
    assert_eq!(line.spans[0].style.bg, Some(BLOCK));
}

#[test]
fn rendering_never_mutates_the_source_band() {
    let left = vec![Seg::new(INK, "a\u{1b}[31mb")];
    let right = vec![Seg::new(OK, "c\u{7}d")];
    let band = Band {
        bg: BLOCK,
        left: left.clone(),
        right: right.clone(),
        width: 12,
        pad: 1,
    };
    let _ = band.render();
    assert_eq!(band.left, left);
    assert_eq!(band.right, right);
}

#[test]
fn both_sides_are_sanitized_before_measuring() {
    let line = Band {
        bg: BLOCK,
        left: vec![Seg::new(INK, "left\u{1b}[31m")],
        right: vec![Seg::new(OK, "right\u{1b}]0;t\u{7}")],
        width: 11,
        pad: 0,
    }
    .render();
    assert_eq!(line.to_string(), "left  right");
}

#[test]
fn tiny_and_zero_widths_stay_within_the_band_and_emit_no_controls() {
    for width in 0..=4 {
        let line = Band {
            bg: BLOCK,
            left: vec![Seg::new(INK, "ab\u{1b}[31mcd\u{1b}[0m")],
            right: vec![Seg::new(OK, "ef\u{1b}]0;t\u{7}")],
            width,
            pad: 0,
        }
        .render();
        let row = line.to_string();
        assert!(cell_width(&row) <= width, "{width}: {row:?}");
        assert!(!row.chars().any(char::is_control), "{width}: {row:?}");
    }
}

#[test]
fn escape_sequences_cross_segments_without_losing_styles() {
    for (first, second, expected) in [
        ("\u{1b}[31", "mrest", "rest"),
        ("ab\u{1b}", "[31mcd", "abcd"),
        ("\u{1b}]8;;http", "://x\u{7}link", "link"),
        ("\u{1b}]title\u{1b}", "\\body", "body"),
    ] {
        let line = Band {
            bg: BLOCK,
            left: vec![Seg::new(INK, first), Seg::new(REF, second)],
            right: Vec::new(),
            width: cell_width(expected),
            pad: 0,
        }
        .render();
        assert_eq!(line.to_string(), expected);
        assert_eq!(line.spans.last().unwrap().style.fg, Some(REF));
    }
    // A malformed left field must not suppress an unrelated right outcome.
    let line = Band {
        bg: BLOCK,
        left: vec![Seg::new(INK, "\u{1b}]unterminated")],
        right: vec![Seg::new(OK, "OK")],
        width: 4,
        pad: 0,
    }
    .render();
    assert_eq!(line.to_string(), "  OK");
}

#[test]
fn a_rendered_band_reaches_the_buffer_without_control_bytes() {
    let line = left_only("ok\u{1b}[31m", "ok");
    let mut buffer = Buffer::empty(Rect::new(0, 0, 2, 1));
    buffer.set_line(0, 0, &line, 2);
    assert_eq!(buffer[(0, 0)].symbol(), "o");
    assert_eq!(buffer[(1, 0)].symbol(), "k");
    assert_eq!(buffer[(0, 0)].fg, INK);
    assert_eq!(buffer[(0, 0)].bg, BLOCK);
}
