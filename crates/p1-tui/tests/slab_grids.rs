//! Self-test of the SLAB snapshot oracle (`common/slab.rs`): the mocks are well formed, a
//! buffer painted from a mock captures back to the same mock, and one wrong cell fails.

mod common;

use common::slab;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

/// Inverse of the oracle's decode table: the 24-bit colour for a code (handoff §2).
fn bg(code: char) -> Color {
    match code {
        'G' => Color::Rgb(0x0a, 0x0a, 0x0a),
        'B' => Color::Rgb(0x12, 0x12, 0x12),
        'P' => Color::Rgb(0x1c, 0x1c, 0x1c),
        '+' => Color::Rgb(0x3a, 0x4a, 0x3a),
        '-' => Color::Rgb(0x4a, 0x35, 0x35),
        'A' => Color::Rgb(0xe2, 0xa0, 0x3f),
        'N' => Color::Rgb(0xe8, 0xe8, 0xe8),
        other => panic!("unknown bg code {other}"),
    }
}

fn fg(code: char) -> Color {
    match code {
        'i' | '_' => Color::Rgb(0xe8, 0xe8, 0xe8),
        'd' => Color::Rgb(0x9a, 0x9a, 0x9a),
        'f' => Color::Rgb(0x6a, 0x6a, 0x6a),
        'a' => Color::Rgb(0xe2, 0xa0, 0x3f),
        'x' => Color::Rgb(0xe0, 0x70, 0x5f),
        'o' => Color::Rgb(0x8f, 0xb5, 0x73),
        'l' => Color::Rgb(0x72, 0xb8, 0xb0),
        'r' => Color::Rgb(0x7f, 0xa7, 0xd6),
        's' => Color::Rgb(0xc0, 0x8f, 0xc8),
        'g' => Color::Rgb(0x0a, 0x0a, 0x0a),
        'u' => Color::Rgb(0x2a, 0x2a, 0x2a),
        '+' => Color::Rgb(0xd8, 0xe8, 0xd0),
        '-' => Color::Rgb(0xe8, 0xd0, 0xd0),
        other => panic!("unknown fg code {other}"),
    }
}

fn paint(mock: &slab::Mock) -> Buffer {
    let area = Rect::new(0, 0, mock.width() as u16, mock.height() as u16);
    let mut buffer = Buffer::empty(area);
    for (y, (text, runs)) in mock.text.iter().zip(&mock.runs).enumerate() {
        let cells = slab::expand_runs(runs);
        for (x, (ch, (b, f))) in text.chars().zip(cells).enumerate() {
            let cell = &mut buffer[(x as u16, y as u16)];
            cell.set_char(ch);
            cell.set_bg(bg(b));
            cell.set_fg(fg(f));
        }
    }
    buffer
}

#[test]
fn every_mock_is_well_formed() {
    let ids = slab::ids();
    assert_eq!(ids.len(), 25 + 34, "25 element mocks and 34 screens");
    for id in ids {
        let mock = slab::mock(&id);
        assert_eq!(
            mock.text.len(),
            mock.runs.len(),
            "{id}: TEXT and RUNS row counts"
        );
        for (row, (text, runs)) in mock.text.iter().zip(&mock.runs).enumerate() {
            assert_eq!(
                text.chars().count(),
                mock.width(),
                "{id} row {row}: TEXT width"
            );
            let cells = slab::expand_runs(runs);
            assert_eq!(cells.len(), mock.width(), "{id} row {row}: RUNS width");
            // A blank cell is coded `_` and a glyph never is: the two views agree.
            for (x, (ch, (_, f))) in text.chars().zip(&cells).enumerate() {
                assert_eq!(
                    ch == ' ',
                    *f == '_',
                    "{id} row {row} col {x}: `{ch}` coded fg `{f}`"
                );
            }
        }
    }
}

#[test]
fn a_painted_mock_captures_back_to_itself() {
    for id in slab::ids() {
        let mock = slab::mock(&id);
        let buffer = paint(&mock);
        slab::assert_mock(&buffer, buffer.area, &id);
    }
}

#[test]
fn a_region_of_a_screen_compares_like_a_mock() {
    // The statusline row of the 120×40 reference screen, cut out of the painted screen.
    let screen = slab::mock("S01");
    let buffer = paint(&screen);
    slab::assert_mock_region(&buffer, Rect::new(2, 38, 116, 1), "S01", 38, 2);
    let wrong = std::panic::catch_unwind(|| {
        slab::assert_mock_region(&buffer, Rect::new(2, 38, 116, 1), "S01", 37, 2)
    });
    assert!(
        wrong.is_err(),
        "a region compared against the wrong rows must fail"
    );
}

#[test]
fn one_wrong_cell_fails_the_comparison() {
    let mock = slab::mock("el-statusline@116");
    let mut buffer = paint(&mock);
    // Recolour the first glyph cell to a colour outside the token table.
    let x = mock.text[0].chars().position(|c| c != ' ').unwrap() as u16;
    buffer[(x, 0)].set_fg(Color::Rgb(1, 2, 3));
    let area = buffer.area;
    let result = std::panic::catch_unwind(|| slab::assert_mock(&buffer, area, "el-statusline@116"));
    assert!(result.is_err(), "a recoloured cell must not match");

    let mut buffer = paint(&mock);
    buffer[(x, 0)].set_char('#');
    let result = std::panic::catch_unwind(|| slab::assert_mock(&buffer, area, "el-statusline@116"));
    assert!(result.is_err(), "a changed character must not match");
}
