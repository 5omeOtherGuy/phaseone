//! SLAB composer, queue, scroll mark, menus and full diff review against the handoff mocks.
//!
//! Each element renders alone into a buffer the size of the cells it occupies in a full-screen
//! mock; the region's position follows the screen's §4 geometry (120×40: transcript rows 1–33,
//! composer 35–36; 80×24: transcript 0–19, composer 21–22; text column 2, T = 76). The composer
//! cursor is the terminal's hardware cursor, which the mocks draw as one amber cell: the tests
//! put that cell where `composer::render` says the cursor goes.
mod common;

use std::collections::VecDeque;

use common::slab;
use p1_tui::palette;
use p1_tui::render::composer::{self, Mode};
use p1_tui::render::diff::{DiffRow, DiffView};
use p1_tui::render::picker::{self, Picker, PickerGroup, PickerRow};
use p1_tui::render::review::{self, Decision, DecisionKey, ReviewFile};
use p1_tui::render::scroll::{self, ScrollMark};
use p1_tui::state::{Composer, FullReview, Queued, Screen};
use p1_tui::wrap::cell_width;
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::text::Line;

/// The transcript column at both 80 and 120 columns.
const T: u16 = 76;
const EDGE: &str = "crates/p1-context/src/edge.rs";

fn buffer(lines: &[Line<'static>], width: u16) -> Buffer {
    let mut buf = Buffer::empty(Rect::new(0, 0, width, lines.len() as u16));
    for (y, line) in lines.iter().enumerate() {
        buf.set_line(0, y as u16, line, width);
    }
    buf
}

/// Render the composer and place the block cursor where the terminal would draw it.
fn composer_buffer(composer: &Composer, mode: Mode<'_>) -> Buffer {
    let frame = composer::render(composer, mode, T as usize, 2);
    let mut buf = buffer(&frame.lines, T);
    if let Some((x, y)) = frame.cursor {
        let cell = &mut buf[(x, y)];
        cell.bg = palette::AMBER_FILL;
        cell.fg = palette::ON_FILL;
    }
    buf
}

fn typed(text: &str) -> Composer {
    Composer {
        text: text.into(),
        cursor: text.chars().count(),
        ..Composer::default()
    }
}

fn check_composer(id: &str, top: usize, composer: &Composer, mode: Mode<'_>) {
    let buf = composer_buffer(composer, mode);
    slab::assert_mock_region(&buf, buf.area, id, top, 2);
}

fn check_rows(id: &str, top: usize, lines: &[Line<'static>], width: u16) {
    let buf = buffer(lines, width);
    slab::assert_mock_region(&buf, buf.area, id, top, 2);
}

fn queue() -> VecDeque<Queued> {
    VecDeque::from([
        Queued {
            follow_up: false,
            text: "use a VecDeque for the pending queue".into(),
        },
        Queued {
            follow_up: true,
            text: "then run clippy on p1-tui".into(),
        },
    ])
}

#[test]
fn queue_rows_and_scroll_mark_match_the_element_mock() {
    let mut buf = Buffer::empty(Rect::new(0, 0, T, 4));
    for (y, line) in scroll::queue_rows(&queue(), T as usize).iter().enumerate() {
        buf.set_line(0, y as u16, line, T);
    }
    let mark = ScrollMark {
        below: 14,
        running: Some("shell running".into()),
        row: 212,
        total: 480,
    };
    buf.set_line(0, 3, &mark.line(T as usize), T);
    slab::assert_mock(&buf, buf.area, "el-queue-scroll@76");
}

#[test]
fn q01_queue_rows_sit_above_the_working_composer() {
    check_rows("Q01", 32, &scroll::queue_rows(&queue(), T as usize), T);
    check_composer("Q01", 35, &Composer::default(), Mode::Working);
}

#[test]
fn r01_the_scroll_mark_takes_the_last_transcript_row() {
    let mark = ScrollMark {
        below: 14,
        running: Some("shell running".into()),
        row: 1,
        total: 47,
    };
    check_rows("R01", 33, &[mark.line(T as usize)], T);
    check_composer("R01", 35, &Composer::default(), Mode::Working);
}

#[test]
fn composer_states_match_their_screens() {
    check_composer("H01", 35, &Composer::default(), Mode::Idle);
    check_composer("A01", 35, &Composer::default(), Mode::Decision);
    check_composer("A02", 21, &Composer::default(), Mode::Decision);
    check_composer("W02", 35, &Composer::default(), Mode::Attached("w2"));
}

#[test]
fn c01_command_completion_docks_above_the_composer() {
    let mut screen = Screen::new(true);
    screen.statusbar.model = Some("claude/opus-5.5".into());
    screen.statusbar.effort = Some("high".into());
    screen.open_completion();
    let menu = screen.picker.as_mut().expect("`/` opens completion");
    // Access is fixed per process and lives in the host; the menu shows what it is given.
    menu.set_value("/access", "full");
    check_rows("C01", 24, &picker::lines(menu, T as usize), T);
    check_composer("C01", 35, &screen.composer, Mode::Working);
}

fn row(label: &str, description: &str, value: &str) -> PickerRow {
    PickerRow {
        label: label.into(),
        description: description.into(),
        value: value.into(),
        efforts: description
            .split(' ')
            .filter(|w| ["low", "medium", "high", "max"].contains(w))
            .map(str::to_string)
            .collect(),
        ..PickerRow::default()
    }
}

fn group(header: &str, right: &str, rows: Vec<PickerRow>) -> PickerGroup {
    PickerGroup {
        header: header.into(),
        right: right.into(),
        rows,
    }
}

fn model_menu() -> Picker {
    let borrowed = "oauth · borrowed";
    let mut sol = row("gpt/gpt-5.6-sol", "low medium high", borrowed);
    sol.effort = 1;
    Picker {
        title: Some("/model".into()),
        count: "11 models · 5 environments".into(),
        groups: vec![
            group(
                "CLAUDE",
                "anthropic-subscription",
                vec![
                    row("claude/opus-5.5", "low medium high max", "current"),
                    row("claude/sonnet-5", "low medium high max", borrowed),
                    row("claude/opus-5", "low medium high", borrowed),
                ],
            ),
            group(
                "DEEPSEEK",
                "opencode-go-subscription",
                vec![row("deepseek/v4.1-flash", "default", "api key")],
            ),
            group(
                "DEEPSEEK2",
                "opencode-go-2-subscription",
                vec![row("deepseek2/v4.1-flash", "default", "api key")],
            ),
            group(
                "GLM",
                "glm-subscription",
                vec![PickerRow {
                    available: false,
                    ..row("glm/5.3", "default", "account exhausted")
                }],
            ),
            group(
                "GPT",
                "openai-codex-subscription",
                vec![
                    row("gpt/gpt-6-astra", "low medium high", borrowed),
                    sol,
                    row("gpt/gpt-5.6-terra", "low medium high", borrowed),
                    row("gpt/gpt-5.6-luna", "low medium", borrowed),
                ],
            ),
        ],
        selected: 7,
        label_width: 22,
        note: "switches at the next turn".into(),
        keys: "↑↓ move   ←→ effort   ⏎ switch   esc".into(),
        ..Picker::default()
    }
}

#[test]
fn c02_model_menu_at_120x40() {
    check_rows("C02", 18, &picker::lines(&model_menu(), T as usize), T);
    check_composer("C02", 35, &typed("/model"), Mode::Working);
}

#[test]
fn c03_model_menu_at_80x24() {
    check_rows("C03", 4, &picker::lines(&model_menu(), T as usize), T);
    check_composer("C03", 21, &typed("/model"), Mode::Working);
}

#[test]
fn c02_arrows_step_the_focused_rows_effort() {
    let mut menu = model_menu();
    menu.step_effort(1);
    let text = picker::lines(&menu, T as usize)[13].to_string();
    assert!(
        text.contains("gpt/gpt-5.6-sol       effort ← high →"),
        "{text}"
    );
}

#[test]
fn h03_resume_menu() {
    let session = |label: &str, description: &str, value: &str| PickerRow {
        label: label.into(),
        description: description.into(),
        value: value.into(),
        ..PickerRow::default()
    };
    let menu = Picker {
        title: Some("/resume".into()),
        count: "4 sessions · this directory".into(),
        groups: vec![PickerGroup {
            rows: vec![
                session(
                    "today 21:10",
                    "fix compaction boundary stall",
                    "claude/opus-5.5 · 214 items",
                ),
                session(
                    "today 17:42",
                    "split provider-http helpers (#47)",
                    "deepseek2/v4.1-flash · 96",
                ),
                session(
                    "yesterday 23:05",
                    "worker grants: add_tools",
                    "gpt/gpt-5.6-sol · 311",
                ),
                PickerRow {
                    available: false,
                    ..session(
                        "2026-09-20 14:02",
                        "websocket fallback notice",
                        "in use · pid 41210",
                    )
                },
            ],
            ..PickerGroup::default()
        }],
        label_width: 18,
        note: "resumes on its recorded model unless /model is set".into(),
        keys: "↑↓ move   ⏎ resume   esc".into(),
        ..Picker::default()
    };
    check_rows("H03", 28, &picker::lines(&menu, T as usize), T);
    check_composer("H03", 35, &typed("/resume"), Mode::Idle);
}

// Lead decision on the ADR-0056 handoff (deviation: C05 text row): §8.1's prose wins over the
// C05 mock. The mock cuts the long goal with `…` only because the designer's generator ran every
// row through the Band rule; the composer soft-wraps instead, so what the operator types stays
// visible and the cursor always has a cell. C05's hint row is checked as drawn; its text row is
// checked for the cell layout of a goal that fits, and a long goal must wrap.
const C05_GOAL: &str = "fix compaction boundary stall without changing the summary format";

fn goal_editor(goal: &str) -> Composer {
    let mut screen = Screen::new(true);
    screen.goal = Some(goal.into());
    screen.edit_goal();
    screen.composer
}

#[test]
fn c05_goal_editor_hint_row() {
    let frame = composer::render(&goal_editor(C05_GOAL), Mode::Working, T as usize, 3);
    let hints = frame.lines.last().expect("hint row").clone();
    check_rows("C05", 36, &[hints], T);
}

#[test]
fn c05_a_goal_that_fits_has_the_mock_row_layout() {
    // The mock's own text up to a word boundary: `/goal ` + 58 cells, well inside the row.
    let goal = "fix compaction boundary stall without changing the summary";
    let frame = composer::render(&goal_editor(goal), Mode::Working, T as usize, 3);
    assert_eq!(frame.lines.len(), 2, "one input row and the hints");
    let buf = buffer(&frame.lines[..1], T);
    // Pad, amber `›`, the ink text: the same cells as C05 row 35 up to the text's end.
    let shown = 4 + cell_width("/goal ") + cell_width(goal);
    slab::assert_mock_region(&buf, Rect::new(0, 0, shown as u16, 1), "C05", 35, 2);
    // Past the text only BLOCK+ (the mock's `…` cell aside): its right padding matches too.
    slab::assert_mock_region(
        &buf,
        Rect::new(T - 2, 0, 2, 1),
        "C05",
        35,
        2 + T as usize - 2,
    );
    for x in shown as u16..T {
        let cell = &buf[(x, 0)];
        assert_eq!(
            (cell.symbol(), cell.bg),
            (" ", palette::BLOCK_PLUS),
            "col {x}"
        );
    }
    // The hardware cursor sits on the cell after the goal.
    assert_eq!(frame.cursor, Some((shown as u16, 0)));
}

#[test]
fn c05_a_long_goal_wraps_upward_with_the_cursor_on_its_last_cell() {
    let frame = composer::render(&goal_editor(C05_GOAL), Mode::Working, T as usize, 3);
    let text: Vec<String> = frame.lines.iter().map(|l| l.to_string()).collect();
    assert_eq!(text.len(), 3, "two input rows and the hints");
    assert_eq!(
        text[0].trim_end(),
        "  › /goal fix compaction boundary stall without changing the summary"
    );
    assert_eq!(text[1].trim_end(), "    format");
    assert!(text.iter().all(|row| !row.contains('…')), "nothing is cut");
    // Row 1 keeps C05 row 35's prompt cells; the continuation hangs under the text.
    let buf = buffer(&frame.lines[..2], T);
    slab::assert_mock_region(&buf, Rect::new(0, 0, 10, 1), "C05", 35, 2);
    assert_eq!(buf[(4, 1)].symbol(), "f");
    assert_eq!(buf[(4, 1)].fg, palette::INK);
    assert_eq!(buf[(4, 1)].bg, palette::BLOCK_PLUS);
    assert_eq!(frame.cursor, Some((4 + cell_width("format") as u16, 1)));
}

fn edit_diff(shift: u32) -> Vec<DiffRow> {
    let context = |line: u32, text: &str| DiffRow::Context {
        line: line + shift,
        text: text.into(),
    };
    let del = |line: u32, text: &str| DiffRow::Del {
        line: line + shift,
        text: text.into(),
    };
    let add = |line: u32, text: &str| DiffRow::Add {
        line: line + shift,
        text: text.into(),
    };
    vec![
        context(411, "let pressure = self.pressure_at_edge();"),
        del(412, "if pressure == Pressure::Hard {"),
        del(413, "    block_until_ready(&worker);"),
        del(414, "}"),
        add(412, "if let Some(summary) = ready {"),
        add(413, "    return self.apply_at_boundary(summary);"),
        add(414, "}"),
        context(415, "self.commit_boundary()"),
    ]
}

fn review_file(
    tool: &str,
    summary: &str,
    rows: Vec<DiffRow>,
    counts: (usize, usize),
) -> ReviewFile {
    ReviewFile {
        view: DiffView {
            tool: tool.into(),
            file: EDGE.into(),
            summary: summary.into(),
            position: (1, 1),
            rows,
            grantable: true,
        },
        added: counts.0,
        removed: counts.1,
    }
}

#[test]
fn a04_full_review_of_a_three_file_patch() {
    let first = review_file(
        "apply_patch",
        "update file · hunk 1 of 1",
        [edit_diff(0), edit_diff(40)].concat(),
        (12, 3),
    );
    let files = [first.clone(), first.clone(), first];
    let review = FullReview {
        open: true,
        ..FullReview::default()
    };
    let decision = Decision::for_call(true, files.len());
    // 120×40: the review owns rows 1–36 (top blank row, then up to the statusline gap).
    let lines = review::lines(&files, &review, &decision, 116, 36);
    check_rows("A04", 1, &lines, 116);
}

#[test]
fn a05_full_review_at_80x24_keeps_the_decision_pinned() {
    let files = [review_file(
        "edit",
        "replace exact string · once",
        [edit_diff(0), edit_diff(0)].concat(),
        (3, 3),
    )];
    let key = |key, label: &str, available| DecisionKey {
        key,
        label: label.into(),
        available,
        reason: None,
    };
    let decision = Decision {
        keys: vec![
            key('y', "allow once", true),
            key('a', "session", true),
            key('p', "project", false),
            key('n', "deny", true),
        ],
        secondary: vec![],
    };
    let review = FullReview {
        open: true,
        ..FullReview::default()
    };
    // 80×24: no blank rows; the review owns rows 0–22.
    let lines = review::lines(&files, &review, &decision, 76, 23);
    check_rows("A05", 0, &lines, 76);
}

#[test]
fn focus_mode_hides_the_empty_composer_and_short_screens_keep_one_row() {
    // F01 / S05: hidden while empty, revealed by the first key, hidden again when cleared.
    let mut composer = Composer::default();
    assert!(!composer.visible(true));
    composer.insert('x');
    assert!(composer.visible(true));
    composer.backspace();
    assert!(!composer.visible(true));
    // H ≤ 12: row 1 only.
    let frame = composer::render(&typed("abc"), Mode::Working, T as usize, 1);
    assert_eq!(frame.lines.len(), 1);
}
