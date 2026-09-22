//! The snapshot oracle for the SLAB Harness redesign (ADR-0051).
//!
//! `docs/design/tui/slab/grids.json` holds every mock of the handoff as TEXT (one string per
//! row) and RUNS (run-length `<bg><fg>×<n>` codes per row). This module encodes a rendered
//! `Buffer` region the same way and compares the two, so a renderer is done when its cells
//! equal the designer's cells.
//!
//! Colours are decoded from the handoff's §2 token table, NOT from `p1_tui::palette`: the
//! oracle must not agree with the code it checks by construction. A colour outside the table
//! encodes as `?` and never matches a mock. The animated `▪▪▪` cells are the one exception the
//! mocks cannot pin (their fg is scaled per frame): render with reduced motion, or at a time
//! where every cell is at full opacity, before comparing.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::sync::OnceLock;

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;

const GRIDS: &str = include_str!("../../../../docs/design/tui/slab/grids.json");

/// One mock: exact characters and attribute runs, one entry per terminal row.
#[derive(Debug, Clone)]
pub struct Mock {
    pub id: String,
    pub title: String,
    pub text: Vec<String>,
    pub runs: Vec<String>,
}

impl Mock {
    pub fn width(&self) -> usize {
        self.text.first().map_or(0, |row| row.chars().count())
    }
    pub fn height(&self) -> usize {
        self.text.len()
    }
}

fn all() -> &'static BTreeMap<String, Mock> {
    static MOCKS: OnceLock<BTreeMap<String, Mock>> = OnceLock::new();
    MOCKS.get_or_init(|| {
        let root: serde_json::Value = serde_json::from_str(GRIDS).expect("grids.json parses");
        let mut out = BTreeMap::new();
        for group in ["elements", "screens"] {
            let entries = root[group].as_object().expect("grids.json group is an object");
            for (id, mock) in entries {
                let strings = |key: &str| -> Vec<String> {
                    mock[key]
                        .as_array()
                        .unwrap_or_else(|| panic!("{id}: `{key}` is an array"))
                        .iter()
                        .map(|v| v.as_str().expect("row is a string").to_string())
                        .collect()
                };
                out.insert(
                    id.clone(),
                    Mock {
                        id: id.clone(),
                        title: mock["title"].as_str().unwrap_or_default().to_string(),
                        text: strings("text"),
                        runs: strings("runs"),
                    },
                );
            }
        }
        out
    })
}

/// Every mock id (`el-…@<width>` element mocks, then the screen ids `S01`, `H01`, …).
pub fn ids() -> Vec<String> {
    all().keys().cloned().collect()
}

/// A mock by id; panics with the known ids when it does not exist.
pub fn mock(id: &str) -> Mock {
    all()
        .get(id)
        .cloned()
        .unwrap_or_else(|| panic!("no mock `{id}`; known: {:?}", ids()))
}

/// The handoff §2 table, 24-bit column: background code for a surface colour.
fn bg_code(color: Color) -> char {
    match color {
        // GROUND is the terminal's default background (SPEC §1, handoff §2).
        Color::Reset => 'G',
        Color::Rgb(0x0a, 0x0a, 0x0a) => 'G',
        Color::Rgb(0x12, 0x12, 0x12) => 'B',
        Color::Rgb(0x1c, 0x1c, 0x1c) => 'P',
        Color::Rgb(0x3a, 0x4a, 0x3a) => '+',
        Color::Rgb(0x4a, 0x35, 0x35) => '-',
        Color::Rgb(0xe2, 0xa0, 0x3f) => 'A',
        Color::Rgb(0xe8, 0xe8, 0xe8) => 'N',
        _ => '?',
    }
}

/// The handoff §2 table, 24-bit column: foreground code for an ink or signal colour.
fn fg_code(color: Color) -> char {
    match color {
        Color::Rgb(0xe8, 0xe8, 0xe8) => 'i',
        Color::Rgb(0x9a, 0x9a, 0x9a) => 'd',
        Color::Rgb(0x6a, 0x6a, 0x6a) => 'f',
        Color::Rgb(0xe2, 0xa0, 0x3f) => 'a',
        Color::Rgb(0xe0, 0x70, 0x5f) => 'x',
        Color::Rgb(0x8f, 0xb5, 0x73) => 'o',
        Color::Rgb(0x72, 0xb8, 0xb0) => 'l',
        Color::Rgb(0x7f, 0xa7, 0xd6) => 'r',
        Color::Rgb(0xc0, 0x8f, 0xc8) => 's',
        Color::Rgb(0x0a, 0x0a, 0x0a) => 'g',
        Color::Rgb(0x2a, 0x2a, 0x2a) => 'u',
        Color::Rgb(0xd8, 0xe8, 0xd0) => '+',
        Color::Rgb(0xe8, 0xd0, 0xd0) => '-',
        _ => '?',
    }
}

/// Encode `area` of `buffer` as (TEXT rows, RUNS rows) in the grids.json format. A blank cell
/// (a space) codes its fg as `_`: its colour is invisible, so the mocks do not pin it.
pub fn capture(buffer: &Buffer, area: Rect) -> (Vec<String>, Vec<String>) {
    let mut text = Vec::new();
    let mut runs = Vec::new();
    for y in area.top()..area.bottom() {
        let mut row = String::new();
        let mut codes: Vec<(char, char)> = Vec::new();
        for x in area.left()..area.right() {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            // A wide glyph's continuation cell carries an empty symbol; the glyph itself
            // already accounts for both cells in TEXT, so only its attribute is recorded.
            if !symbol.is_empty() {
                row.push_str(symbol);
            }
            let fg = if symbol == " " || symbol.is_empty() {
                '_'
            } else {
                fg_code(cell.fg)
            };
            codes.push((bg_code(cell.bg), fg));
        }
        text.push(row);
        runs.push(encode_runs(&codes));
    }
    (text, runs)
}

fn encode_runs(codes: &[(char, char)]) -> String {
    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < codes.len() {
        let mut n = 1;
        while i + n < codes.len() && codes[i + n] == codes[i] {
            n += 1;
        }
        out.push(format!("{}{}×{n}", codes[i].0, codes[i].1));
        i += n;
    }
    out.join(" ")
}

/// Expand a RUNS row to one `(bg, fg)` pair per cell, so rows are compared cell by cell
/// whatever run boundaries the generator chose.
pub fn expand_runs(row: &str) -> Vec<(char, char)> {
    let mut cells = Vec::new();
    for run in row.split_whitespace() {
        let mut chars = run.chars();
        let bg = chars.next().expect("run has a bg code");
        let fg = chars.next().expect("run has a fg code");
        let count: String = chars.collect();
        let count = count
            .strip_prefix('×')
            .unwrap_or_else(|| panic!("run `{run}` has no ×"));
        let count: usize = count.parse().unwrap_or_else(|_| panic!("run `{run}` count"));
        cells.extend(std::iter::repeat_n((bg, fg), count));
    }
    cells
}

/// Compare `area` of `buffer` with mock `id` over all its rows. Panics with the first
/// differing rows of TEXT and of RUNS, side by side, so a failure says what to fix.
pub fn assert_mock(buffer: &Buffer, area: Rect, id: &str) {
    let mock = mock(id);
    assert_rows(buffer, area, &mock, 0..mock.height());
}

/// Compare only mock rows `rows` against `area` (whose height must equal the row count).
/// For element mocks that stack several independent examples, a test renders one example and
/// checks its rows.
pub fn assert_mock_rows(buffer: &Buffer, area: Rect, id: &str, rows: std::ops::Range<usize>) {
    let mock = mock(id);
    assert_rows(buffer, area, &mock, rows);
}

fn assert_rows(buffer: &Buffer, area: Rect, mock: &Mock, rows: std::ops::Range<usize>) {
    assert_eq!(
        area.width as usize,
        mock.width(),
        "{}: area width {} but the mock is {} wide",
        mock.id,
        area.width,
        mock.width()
    );
    assert_eq!(
        area.height as usize,
        rows.len(),
        "{}: area height {} but {} mock rows were asked for",
        mock.id,
        area.height,
        rows.len()
    );
    let (text, runs) = capture(buffer, area);
    let mut problems = Vec::new();
    for (got_index, mock_row) in rows.clone().enumerate() {
        if text[got_index] != mock.text[mock_row] {
            problems.push(format!(
                "row {mock_row} TEXT\n  want |{}|\n  got  |{}|",
                mock.text[mock_row], text[got_index]
            ));
        }
        let want = expand_runs(&mock.runs[mock_row]);
        let got = expand_runs(&runs[got_index]);
        if want != got {
            let column = want
                .iter()
                .zip(&got)
                .position(|(w, g)| w != g)
                .unwrap_or(want.len().min(got.len()));
            problems.push(format!(
                "row {mock_row} RUNS (first difference at column {column})\n  want {}\n  got  {}",
                mock.runs[mock_row], runs[got_index]
            ));
        }
    }
    assert!(
        problems.is_empty(),
        "{} ({}) differs from the mock:\n{}",
        mock.id,
        mock.title,
        problems.join("\n")
    );
}
