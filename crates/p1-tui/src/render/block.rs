//! The three-band BLOCK treatment for one tool event (BLOCK-SPEC §2–§6).
//!
//! Band A is a single header row on BLOCK+; band B is the tool's own body on
//! BLOCK; band C is at most one meta row (a fold handle, or facts that did not
//! fit the header). Every band is filled edge to edge across the transcript
//! width — a ragged right edge is the one thing that makes a block look like a
//! box that failed. No box-drawing codepoints are ever emitted; the background
//! step is the only edge.
//!
//! Geometry (character cells): `W` = width, horizontal padding 2 left and 2
//! right, so usable width `U = W - 4`. Vertical padding is 0.

use p1_contracts::ToolStatus;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

use super::{elapsed, fill};
use crate::fold::{FOLD_KEEP_LINES, Fold, FoldKeep};
use crate::glyphs;
use crate::palette;
use crate::transcript::{RowStatus, ToolRow, json_string_field, json_unescape};

/// Horizontal padding inside every band.
const PAD: usize = 2;
/// The name field after the glyph and its space (BLOCK-SPEC §2).
const NAME_FIELD: usize = 10;
/// Minimum columns between the argument and the right-aligned outcome.
const MIN_GAP: usize = 2;
/// The fold handle's right-aligned key hint.
const OPEN_HINT: &str = "^O open in pane";

/// Render one tool event as its three bands.
pub fn lines(row: &ToolRow, width: usize, now_ms: u64, reduced_motion: bool) -> Vec<Line<'static>> {
    let mut out = Vec::with_capacity(4);
    out.push(header(row, width, now_ms, reduced_motion));
    out.extend(body(row, width));
    if let Some(meta) = meta(row, width) {
        out.push(meta);
    }
    out
}

/// Band A: `<glyph> <name(10)> <argument> … <outcome>`, exactly one row.
fn header(row: &ToolRow, width: usize, now_ms: u64, reduced_motion: bool) -> Line<'static> {
    let (glyph, glyph_fg) = glyph(row.status);
    let name = name_field(&row.name);
    let argument = argument(row);
    let outcomes = outcome(row, now_ms, reduced_motion);
    let outcome_w: usize = outcomes.iter().map(|s| s.content.chars().count()).sum();
    let budget = width.saturating_sub(2 * PAD);
    let prefix_w = 2 + NAME_FIELD;
    // The argument yields to the outcome; the outcome is never truncated.
    let arg_budget = budget.saturating_sub(prefix_w + MIN_GAP + outcome_w);
    let argument = truncate_argument(&argument, arg_budget);
    let used = prefix_w + argument.chars().count();
    let gap = budget.saturating_sub(used + outcome_w);
    let mut spans = vec![
        Span::styled(format!("{glyph} "), Style::new().fg(glyph_fg)),
        Span::styled(name, Style::new().fg(palette::DIM)),
        Span::styled(argument, Style::new().fg(palette::INK)),
        Span::styled(" ".repeat(gap), Style::new()),
    ];
    spans.extend(outcomes);
    band(spans, width, palette::BLOCK_PLUS)
}

/// Band B: the tool's own bytes, unmodified except for truncation (and the
/// per-tool line-number / diff framing), then folded to the §5 window.
fn body(row: &ToolRow, width: usize) -> Vec<Line<'static>> {
    let Some(output) = row.output.as_deref() else {
        return Vec::new();
    };
    let full = match row.name.as_str() {
        "read" => read_body(output, width),
        "edit" | "patch" => edit_body(row, width),
        "write" => write_body(row, width),
        "shell" => shell_body(output, width),
        _ => plain_rows(output, width),
    };
    match row.fold() {
        Some(Fold::Folded { keep, .. }) => match keep {
            FoldKeep::Head => full.into_iter().take(FOLD_KEEP_LINES).collect(),
            FoldKeep::Tail => {
                let skip = full.len().saturating_sub(FOLD_KEEP_LINES);
                full.into_iter().skip(skip).collect()
            }
        },
        _ => full,
    }
}

/// Band C: at most one row. Fold metadata wins over leftover facts.
fn meta(row: &ToolRow, width: usize) -> Option<Line<'static>> {
    if let Some(Fold::Folded { folded, id, .. }) = row.fold() {
        let left = format!("{} {folded} more lines folded → [{id}]", glyphs::PENDING);
        return Some(meta_row(&left, OPEN_HINT, width, palette::BLOCK));
    }
    if row.name == "shell" && matches!(row.status, RowStatus::Settled(_)) {
        let lines = row
            .output
            .as_deref()
            .map(|o| o.lines().count())
            .unwrap_or(0);
        let exit = shell_exit(row).unwrap_or(0);
        let left = format!("exit {exit} · {lines} lines");
        return Some(band(
            vec![Span::styled(left, Style::new().fg(palette::DIM))],
            width,
            palette::BLOCK,
        ));
    }
    None
}

// ---------------------------------------------------------------------------
// Header facts
// ---------------------------------------------------------------------------

/// The glyph and its colour for a row's lifecycle (SPEC §2). The band A glyph
/// marks the event kind (`▸` tool call); the outcome carries `✓`/`✗`.
fn glyph(status: RowStatus) -> (char, Color) {
    match status {
        RowStatus::Running | RowStatus::Settled(_) => (glyphs::TOOL, palette::DIM),
    }
}

/// The 10-column name field: lowercase, space-padded, `…` when longer.
fn name_field(name: &str) -> String {
    let count = name.chars().count();
    if count <= NAME_FIELD {
        let mut padded = name.to_string();
        padded.extend(std::iter::repeat_n(' ', NAME_FIELD - count));
        padded
    } else {
        let mut cut: String = name.chars().take(NAME_FIELD - 1).collect();
        cut.push('…');
        cut
    }
}

/// The argument column. Shell commands get the single-line treatment (§4.2).
fn argument(row: &ToolRow) -> String {
    match row.name.as_str() {
        "shell" => {
            let command = json_string_field(&row.input, "command").unwrap_or(&row.summary);
            if command.contains('\n') {
                if let Some(description) = json_string_field(&row.input, "description") {
                    return description.to_string();
                }
                let first = command.lines().next().unwrap_or("");
                return format!("{first}…");
            }
            command.to_string()
        }
        _ => row.summary.clone(),
    }
}

/// The outcome spans. A `✗` marker is INK; every other fact is DIM.
fn outcome(row: &ToolRow, now_ms: u64, reduced_motion: bool) -> Vec<Span<'static>> {
    match row.status {
        RowStatus::Running => working_spans(now_ms, reduced_motion),
        RowStatus::Settled(status) => {
            let text = outcome_text(row, status);
            if status == ToolStatus::Error || status == ToolStatus::Denied {
                let mut chars = text.chars();
                let marker = chars.next().unwrap_or(' ');
                vec![
                    Span::styled(marker.to_string(), Style::new().fg(palette::INK)),
                    Span::styled(chars.collect::<String>(), Style::new().fg(palette::DIM)),
                ]
            } else {
                vec![Span::styled(text, Style::new().fg(palette::DIM))]
            }
        }
    }
}

fn outcome_text(row: &ToolRow, status: ToolStatus) -> String {
    match status {
        ToolStatus::Ok => success_outcome(row),
        ToolStatus::Denied => format!("{} denied", glyphs::FAILED),
        ToolStatus::Cancelled => format!("{} cancelled", glyphs::FAILED),
        ToolStatus::Unavailable => format!("{} unavailable", glyphs::FAILED),
        ToolStatus::Unknown => format!("{} unknown", glyphs::FAILED),
        ToolStatus::Error => failure_outcome(row),
    }
}

fn success_outcome(row: &ToolRow) -> String {
    let output = row.output.as_deref().unwrap_or("");
    let secs = row.elapsed_ms.map(elapsed);
    match row.name.as_str() {
        "read" => format!(
            "{} {} lines · {}",
            glyphs::DONE,
            output.lines().count(),
            kilobytes(output.len())
        ),
        "shell" => format!("{} {}", glyphs::DONE, secs.unwrap_or_default()),
        "write" => format!(
            "{} {} lines",
            glyphs::DONE,
            write_content(row).map(|c| c.lines().count()).unwrap_or(0)
        ),
        "edit" | "patch" => {
            let (added, removed) = edit_counts(row);
            format!("{} +{added} −{removed} · 1 of 1 files", glyphs::DONE)
        }
        "search" => format!(
            "{} {} hits",
            glyphs::DONE,
            output.lines().filter(|l| !l.trim().is_empty()).count()
        ),
        "finish" => format!("{} done", glyphs::DONE),
        _ => {
            let lines = output.lines().count();
            if lines > 0 {
                format!("{} {lines} lines", glyphs::DONE)
            } else {
                format!("{} ok", glyphs::DONE)
            }
        }
    }
}

fn failure_outcome(row: &ToolRow) -> String {
    let secs = row.elapsed_ms.map(elapsed);
    match row.name.as_str() {
        "shell" => match (secs, shell_exit(row)) {
            (Some(secs), Some(exit)) => format!("{} {secs} · exit {exit}", glyphs::FAILED),
            (Some(secs), None) => format!("{} {secs}", glyphs::FAILED),
            (None, Some(exit)) => format!("{} exit {exit}", glyphs::FAILED),
            (None, None) => format!("{} failed", glyphs::FAILED),
        },
        _ => match secs {
            Some(secs) => format!("{} {secs}", glyphs::FAILED),
            None => format!("{} failed", glyphs::FAILED),
        },
    }
}

fn kilobytes(bytes: usize) -> String {
    if bytes < 10_000 {
        format!("{:.1} kB", bytes as f64 / 1000.0)
    } else {
        format!("{} kB", bytes / 1000)
    }
}

/// The shell exit code, parsed from the tool's own footer.
fn shell_exit(row: &ToolRow) -> Option<i32> {
    let output = row.output.as_deref()?;
    output.lines().rev().find_map(|line| {
        line.trim()
            .strip_prefix("[exit code: ")?
            .strip_suffix(']')?
            .trim()
            .parse()
            .ok()
    })
}

// ---------------------------------------------------------------------------
// Body
// ---------------------------------------------------------------------------

/// A read body keeps the tool's own line numbers, at FAINT (§4.1).
fn read_body(output: &str, width: usize) -> Vec<Line<'static>> {
    let parsed: Vec<(Option<u32>, String)> = output
        .lines()
        .map(|line| match parse_read_line(line) {
            Some((number, text)) => (Some(number), text),
            None => (None, line.to_string()),
        })
        .collect();
    let number_width = parsed
        .iter()
        .filter_map(|(n, _)| *n)
        .map(digits)
        .max()
        .unwrap_or(3)
        .max(3);
    parsed
        .into_iter()
        .map(|(number, text)| match number {
            Some(number) => numbered_row(number, &text, number_width, width),
            None => plain_row(&text, width),
        })
        .collect()
}

/// `{:>6}\t` is the read tool's line prefix; anything else is verbatim.
fn parse_read_line(line: &str) -> Option<(u32, String)> {
    let (prefix, text) = line.split_once('\t')?;
    let number: u32 = prefix.trim().parse().ok()?;
    Some((number, text.to_string()))
}

/// A shell body is stdout then stderr; the tool's footer becomes band C.
fn shell_body(output: &str, width: usize) -> Vec<Line<'static>> {
    output
        .lines()
        .filter(|line| !is_shell_footer(line))
        .map(|line| plain_row(line, width))
        .collect()
}

fn is_shell_footer(line: &str) -> bool {
    let line = line.trim();
    (line.starts_with("[exit code: ") || line.starts_with("[terminated by signal "))
        && line.ends_with(']')
}

/// A write body is the first six written lines as an add-diff (§4.3).
fn write_body(row: &ToolRow, width: usize) -> Vec<Line<'static>> {
    let Some(content) = write_content(row) else {
        return Vec::new();
    };
    content
        .lines()
        .take(6)
        .enumerate()
        .map(|(index, text)| {
            diff_row(
                Some(index as u32 + 1),
                '+',
                text,
                3,
                width,
                palette::DIFF_ADD_BG,
                palette::DIFF_ADD_FG,
            )
        })
        .collect()
}

fn write_content(row: &ToolRow) -> Option<String> {
    json_string_field(&row.input, "content").map(json_unescape)
}

/// `(added, removed)` line counts from the edit's own old/new strings.
fn edit_counts(row: &ToolRow) -> (usize, usize) {
    let removed = json_string_field(&row.input, "old_string")
        .map(json_unescape)
        .unwrap_or_default();
    let added = json_string_field(&row.input, "new_string")
        .map(json_unescape)
        .unwrap_or_default();
    (added.lines().count(), removed.lines().count())
}

/// An edit body: all removals, then all additions (§4.4). The tool result does
/// not carry file line numbers, so the number field stays blank rather than
/// inventing them; the mandatory `+`/`−` column carries the reading.
fn edit_body(row: &ToolRow, width: usize) -> Vec<Line<'static>> {
    let removed = json_string_field(&row.input, "old_string")
        .map(json_unescape)
        .unwrap_or_default();
    let added = json_string_field(&row.input, "new_string")
        .map(json_unescape)
        .unwrap_or_default();
    let mut rows = Vec::new();
    for text in removed.lines() {
        rows.push(diff_row(
            None,
            '−',
            text,
            3,
            width,
            palette::DIFF_DEL_BG,
            palette::DIFF_DEL_FG,
        ));
    }
    for text in added.lines() {
        rows.push(diff_row(
            None,
            '+',
            text,
            3,
            width,
            palette::DIFF_ADD_BG,
            palette::DIFF_ADD_FG,
        ));
    }
    rows
}

/// Every other tool: verbatim, DIM, truncated to the usable width.
fn plain_rows(output: &str, width: usize) -> Vec<Line<'static>> {
    output.lines().map(|line| plain_row(line, width)).collect()
}

// ---------------------------------------------------------------------------
// Row construction
// ---------------------------------------------------------------------------

fn plain_row(text: &str, width: usize) -> Line<'static> {
    let budget = width.saturating_sub(2 * PAD);
    let (shown, cut) = truncate(text, budget);
    let mut spans = vec![Span::styled(shown, Style::new().fg(palette::DIM))];
    if cut {
        spans.push(Span::styled(
            glyphs::OPERATOR.to_string(),
            Style::new().fg(palette::FAINT),
        ));
    }
    band(spans, width, palette::BLOCK)
}

fn numbered_row(number: u32, text: &str, number_width: usize, width: usize) -> Line<'static> {
    let budget = width.saturating_sub(2 * PAD);
    let prefix = format!("{:>number_width$}  ", number);
    let content_budget = budget.saturating_sub(prefix.chars().count());
    let (shown, cut) = truncate(text, content_budget);
    let mut spans = vec![
        Span::styled(prefix, Style::new().fg(palette::FAINT)),
        Span::styled(shown, Style::new().fg(palette::DIM)),
    ];
    if cut {
        spans.push(Span::styled(
            glyphs::OPERATOR.to_string(),
            Style::new().fg(palette::FAINT),
        ));
    }
    band(spans, width, palette::BLOCK)
}

fn diff_row(
    number: Option<u32>,
    marker: char,
    text: &str,
    number_width: usize,
    width: usize,
    bg: Color,
    fg: Color,
) -> Line<'static> {
    let budget = width.saturating_sub(2 * PAD);
    let number_field = match number {
        Some(number) => format!("{number:>number_width$}"),
        None => " ".repeat(number_width),
    };
    let prefix = format!("{number_field}  {marker} ");
    let content_budget = budget.saturating_sub(prefix.chars().count());
    let (shown, cut) = truncate(text, content_budget);
    let mut spans = vec![
        Span::styled(number_field, Style::new().fg(palette::FAINT)),
        Span::styled("  ".to_string(), Style::new()),
        Span::styled(format!("{marker} "), Style::new().fg(fg)),
        Span::styled(shown, Style::new().fg(fg)),
    ];
    if cut {
        spans.push(Span::styled(
            glyphs::OPERATOR.to_string(),
            Style::new().fg(palette::FAINT),
        ));
    }
    band(spans, width, bg)
}

/// Band C's left/right pair: both FAINT, the right edge aligned to `U`.
fn meta_row(left: &str, right: &str, width: usize, bg: Color) -> Line<'static> {
    let budget = width.saturating_sub(2 * PAD);
    let gap = budget
        .saturating_sub(left.chars().count() + right.chars().count())
        .max(1);
    let spans = vec![
        Span::styled(left.to_string(), Style::new().fg(palette::FAINT)),
        Span::styled(" ".repeat(gap), Style::new()),
        Span::styled(right.to_string(), Style::new().fg(palette::FAINT)),
    ];
    band(spans, width, bg)
}

/// Wrap band content in the 2-column left padding and fill to `width` on `bg`.
fn band(content: Vec<Span<'static>>, width: usize, bg: Color) -> Line<'static> {
    let mut spans = vec![Span::styled(" ".repeat(PAD), Style::new().bg(bg))];
    spans.extend(content);
    fill(Line::from(spans), width, bg)
}

/// Truncate to `budget` cells, reserving the last for the FAINT cut mark.
fn truncate(text: &str, budget: usize) -> (String, bool) {
    let count = text.chars().count();
    if count <= budget {
        return (text.to_string(), false);
    }
    if budget == 0 {
        return (String::new(), false);
    }
    let mut cut: String = text.chars().take(budget - 1).collect();
    cut.push(glyphs::OPERATOR);
    (cut, true)
}

fn truncate_argument(argument: &str, budget: usize) -> String {
    let count = argument.chars().count();
    if count <= budget {
        return argument.to_string();
    }
    if budget == 0 {
        return String::new();
    }
    let mut cut: String = argument.chars().take(budget - 1).collect();
    cut.push('…');
    cut
}

fn digits(number: u32) -> usize {
    number.to_string().len()
}

/// The LED chase in the header's outcome position (BLOCK-SPEC §7).
fn working_spans(now_ms: u64, reduced_motion: bool) -> Vec<Span<'static>> {
    (0..glyphs::WORKING_CELLS)
        .map(|cell| {
            let opacity = if reduced_motion {
                1.0
            } else {
                glyphs::working_opacity(cell, now_ms)
            };
            Span::styled(
                glyphs::WORKING.to_string(),
                Style::new().fg(super::dimmed(palette::INK, opacity)),
            )
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::RowStatus;

    fn row(name: &str, input: &str, status: ToolStatus, output: Option<&str>) -> ToolRow {
        ToolRow {
            name: name.into(),
            input: input.into(),
            summary: crate::transcript::summarize_call(name, input),
            status: RowStatus::Settled(status),
            output: output.map(str::to_string),
            elapsed_ms: Some(11_400),
            depth: 0,
        }
    }

    fn text(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn width_of(line: &Line<'static>) -> usize {
        line.spans.iter().map(|s| s.content.chars().count()).sum()
    }

    #[test]
    fn band_a_is_one_row_and_fills_the_width_for_every_tool() {
        let long = "x".repeat(300);
        for (name, input, output) in [
            ("read", r#"{"file_path":"src/lib.rs"}"#, "a\nb"),
            ("shell", r#"{"command":"cargo test"}"#, "ok\n[exit code: 0]"),
            (
                "write",
                r#"{"file_path":"a.rs","content":"x"}"#,
                "Wrote a.rs (1 bytes).",
            ),
            (
                "edit",
                r#"{"file_path":"a.rs","old_string":"a","new_string":"b"}"#,
                "Edited a.rs (1 replacement).",
            ),
            ("search", r#"{"pattern":"p"}"#, "a.rs:1:x"),
            ("worker_start", &long, "x"),
        ] {
            let lines = lines(
                &row(name, input, ToolStatus::Ok, Some(output)),
                100,
                0,
                true,
            );
            assert_eq!(width_of(&lines[0]), 100, "{name}: band A fills the width");
            // A 300-character argument still leaves band A one row.
            assert_eq!(
                lines
                    .iter()
                    .filter(|l| l
                        .spans
                        .iter()
                        .any(|s| s.style.bg == Some(palette::BLOCK_PLUS)))
                    .count(),
                1,
                "{name}: exactly one header row"
            );
        }
    }

    #[test]
    fn no_band_is_ragged_at_any_width() {
        for width in [80, 100, 120, 200] {
            let lines = lines(
                &row(
                    "read",
                    r#"{"file_path":"src/lib.rs"}"#,
                    ToolStatus::Ok,
                    Some("a\nb\nc"),
                ),
                width,
                0,
                true,
            );
            for line in &lines {
                assert_eq!(width_of(line), width, "width {width}");
            }
        }
    }

    #[test]
    fn a_body_row_never_soft_wraps() {
        let long = "z".repeat(500);
        let lines = lines(
            &row("read", r#"{"file_path":"a"}"#, ToolStatus::Ok, Some(&long)),
            80,
            0,
            true,
        );
        // The body row is band B; it is exactly one row and ends in the mark.
        let body = &lines[1];
        assert_eq!(width_of(body), 80);
        assert!(
            body.spans
                .iter()
                .any(|s| s.content.ends_with(glyphs::OPERATOR))
        );
    }

    #[test]
    fn read_keeps_faint_line_numbers() {
        let lines = lines(
            &row(
                "read",
                r#"{"file_path":"a.rs"}"#,
                ToolStatus::Ok,
                Some("     1\tfn main() {}\n     2\t}"),
            ),
            80,
            0,
            true,
        );
        let body = text(&lines);
        assert_eq!(body[1].trim(), "1  fn main() {}");
        // The number is FAINT; the content is DIM.
        assert_eq!(lines[1].spans[1].style.fg, Some(palette::FAINT));
    }

    #[test]
    fn shell_strips_the_footer_into_band_c() {
        let lines = lines(
            &row(
                "shell",
                r#"{"command":"cargo test"}"#,
                ToolStatus::Ok,
                Some("test result: ok\n[exit code: 0]"),
            ),
            80,
            0,
            true,
        );
        let body = text(&lines);
        assert_eq!(body[1].trim(), "test result: ok");
        assert!(body[2].trim_start().starts_with("exit 0 · 2 lines"));
    }

    #[test]
    fn shell_failure_shows_elapsed_and_exit_in_band_a() {
        let lines = lines(
            &row(
                "shell",
                r#"{"command":"cargo test"}"#,
                ToolStatus::Error,
                Some("boom\n[exit code: 101]"),
            ),
            80,
            0,
            true,
        );
        let header = text(&lines).remove(0);
        assert!(header.contains("✗ 11.4s · exit 101"), "{header}");
    }

    #[test]
    fn an_oversized_body_folds_with_a_handle_and_opens_with_the_hint() {
        let big: String = (0..100).map(|n| format!("line {n}\n")).collect();
        let lines = lines(
            &row("read", r#"{"file_path":"a"}"#, ToolStatus::Ok, Some(&big)),
            80,
            0,
            true,
        );
        let body = text(&lines);
        assert_eq!(body.len(), 1 + crate::fold::FOLD_KEEP_LINES + 1);
        let meta = body.last().unwrap();
        assert!(meta.contains("more lines folded → [h-"), "{meta}");
        assert!(meta.trim_end().ends_with("^O open in pane"), "{meta}");
    }

    #[test]
    fn the_fold_threshold_is_forty_and_below_it_nothing_folds() {
        let at_limit: String = (0..crate::fold::FOLD_THRESHOLD)
            .map(|n| format!("line {n}\n"))
            .collect();
        let lines = lines(
            &row(
                "read",
                r#"{"file_path":"a"}"#,
                ToolStatus::Ok,
                Some(&at_limit),
            ),
            80,
            0,
            true,
        );
        assert_eq!(lines.len(), 1 + crate::fold::FOLD_THRESHOLD);
    }

    #[test]
    fn a_write_body_is_an_add_diff() {
        let lines = lines(
            &row(
                "write",
                r#"{"file_path":"a.rs","content":"one\ntwo\nthree"}"#,
                ToolStatus::Ok,
                Some("Wrote a.rs (13 bytes)."),
            ),
            80,
            0,
            true,
        );
        let body = text(&lines);
        assert_eq!(body[1].trim(), "1  + one");
        assert_eq!(body[2].trim(), "2  + two");
        assert_eq!(lines[1].spans[3].style.bg, Some(palette::DIFF_ADD_BG));
    }

    #[test]
    fn an_edit_body_is_removals_then_additions() {
        let lines = lines(
            &row(
                "edit",
                r#"{"file_path":"a.rs","old_string":"old1\nold2","new_string":"new1"}"#,
                ToolStatus::Ok,
                Some("Edited a.rs (1 replacement)."),
            ),
            80,
            0,
            true,
        );
        let body = text(&lines);
        assert_eq!(body[1].trim(), "− old1");
        assert_eq!(body[2].trim(), "− old2");
        assert_eq!(body[3].trim(), "+ new1");
        // The header carries the +A −B outcome.
        assert!(body[0].contains("+1 −2 · 1 of 1 files"), "{}", body[0]);
    }

    #[test]
    fn nothing_a_reader_must_read_is_faint() {
        let lines = lines(
            &row(
                "shell",
                r#"{"command":"cargo test"}"#,
                ToolStatus::Error,
                Some("assertion failed: left == right\n[exit code: 101]"),
            ),
            100,
            0,
            true,
        );
        // The error body is DIM, never FAINT.
        let body = &lines[1];
        assert!(
            body.spans
                .iter()
                .all(|s| s.style.fg != Some(palette::FAINT))
        );
    }

    #[test]
    fn unknown_cost_like_unknown_quantities_render_as_a_dash() {
        // The block itself never prints a zero for an unknown; a tool with no
        // elapsed renders the elapsed field empty, not `0`.
        let mut r = row("read", r#"{"file_path":"a"}"#, ToolStatus::Ok, Some("x"));
        r.elapsed_ms = None;
        let lines = lines(&r, 80, 0, true);
        assert!(!text(&lines)[0].contains("0ms"));
    }

    #[test]
    fn reduced_motion_freezes_the_running_indicator() {
        let mut r = row("shell", r#"{"command":"x"}"#, ToolStatus::Ok, None);
        r.status = RowStatus::Running;
        let frozen = lines(&r, 80, 0, true);
        let cells: Vec<&Span<'static>> = frozen[0]
            .spans
            .iter()
            .filter(|s| s.content == glyphs::WORKING.to_string())
            .collect();
        assert_eq!(cells.len(), glyphs::WORKING_CELLS);
        assert!(cells.iter().all(|s| s.style.fg == Some(palette::INK)));
    }

    /// Every fixture used by the acceptance checks below: `(name, input, status,
    /// output)`.
    fn fixtures() -> Vec<(&'static str, &'static str, ToolStatus, &'static str)> {
        let long_read: String = (0..500)
            .map(|n| format!("{:>6}\tline {n}", n + 1))
            .collect::<Vec<_>>()
            .join("\n");
        let long_shell: String = (0..4000)
            .map(|n| format!("shell line {n}"))
            .chain(std::iter::once("[exit code: 101]".into()))
            .collect::<Vec<_>>()
            .join("\n");
        let many_edits: String = (0..80)
            .map(|n| format!("old {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let many_adds: String = (0..80)
            .map(|n| format!("new {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        vec![
            (
                "read",
                r#"{"file_path":"a.rs"}"#,
                ToolStatus::Ok,
                Box::leak(long_read.into_boxed_str()),
            ),
            (
                "shell",
                r#"{"command":"cargo test"}"#,
                ToolStatus::Error,
                Box::leak(long_shell.into_boxed_str()),
            ),
            (
                "edit",
                Box::leak(
                    format!(
                        r#"{{"file_path":"a.rs","old_string":{},"new_string":{}}}"#,
                        serde_escape(&many_edits),
                        serde_escape(&many_adds)
                    )
                    .into_boxed_str(),
                ),
                ToolStatus::Ok,
                "Edited a.rs (1 replacement).",
            ),
            (
                "write",
                r#"{"file_path":"a.rs","content":"x"}"#,
                ToolStatus::Ok,
                "Wrote a.rs (1 bytes).",
            ),
            (
                "search",
                r#"{"pattern":"p"}"#,
                ToolStatus::Ok,
                "a.rs:1:x\na.rs:2:y",
            ),
            ("finish", r#"{"status":"done"}"#, ToolStatus::Ok, "done"),
            (
                "delegate",
                r#"{"workers":2}"#,
                ToolStatus::Ok,
                "worker w1 started",
            ),
        ]
    }

    /// Minimal JSON string escaping for the edit fixture.
    fn serde_escape(text: &str) -> String {
        let mut out = String::from("\"");
        for c in text.chars() {
            match c {
                '"' => out.push_str("\\\""),
                '\\' => out.push_str("\\\\"),
                '\n' => out.push_str("\\n"),
                _ => out.push(c),
            }
        }
        out.push('"');
        out
    }

    #[test]
    fn the_renderer_emits_no_box_drawing_codepoint() {
        for width in [80, 100, 120, 200] {
            for (name, input, status, output) in fixtures() {
                let lines = lines(&row(name, input, status, Some(output)), width, 0, true);
                for line in &lines {
                    for span in &line.spans {
                        for c in span.content.chars() {
                            assert!(
                                !('\u{2500}'..='\u{257f}').contains(&c),
                                "{name} emitted a box-drawing codepoint {c:?}"
                            );
                        }
                    }
                }
            }
        }
    }

    #[test]
    fn a_width_sweep_keeps_every_band_full_and_band_a_one_row() {
        for width in [80, 100, 120, 200] {
            for (name, input, status, output) in fixtures() {
                let lines = lines(&row(name, input, status, Some(output)), width, 0, true);
                assert!(!lines.is_empty(), "{name} at {width}: at least a header");
                for line in &lines {
                    assert_eq!(width_of(line), width, "{name} ragged at {width}");
                }
                let headers = lines
                    .iter()
                    .filter(|l| {
                        l.spans
                            .iter()
                            .any(|s| s.style.bg == Some(palette::BLOCK_PLUS))
                    })
                    .count();
                assert_eq!(headers, 1, "{name} at {width}: band A is one row");
            }
        }
    }

    #[test]
    fn oversized_outputs_render_in_bounded_rows_with_a_handle() {
        for (name, input, status, output) in fixtures() {
            let lines = lines(&row(name, input, status, Some(output)), 100, 0, true);
            // Header + at most the fold window + one meta row. A 500-line read,
            // a 4000-line shell and a 160-row edit all stay bounded.
            assert!(
                lines.len() <= 1 + FOLD_KEEP_LINES + 1,
                "{name}: {} rows is unbounded",
                lines.len()
            );
        }
    }

    #[test]
    fn resizing_is_stable_apart_from_truncation() {
        let big: String = (0..100)
            .map(|n| format!("line {n}"))
            .collect::<Vec<_>>()
            .join("\n");
        let r = row("read", r#"{"file_path":"a"}"#, ToolStatus::Ok, Some(&big));
        let wide = text(&lines(&r, 120, 0, true));
        let narrow = text(&lines(&r, 80, 0, true));
        let wide_again = text(&lines(&r, 120, 0, true));
        assert_eq!(wide, wide_again, "120 → 80 → 120 is byte-identical");
        assert!(narrow.iter().all(|l| l.chars().count() <= 80));
    }

    #[test]
    fn the_argument_is_ink_and_the_outcome_is_never_faint() {
        for (name, input, status, output) in fixtures() {
            let lines = lines(&row(name, input, status, Some(output)), 100, 0, true);
            let header = &lines[0];
            // [PAD, glyph, name, argument, gap, outcome..]
            assert_eq!(
                header.spans[3].style.fg,
                Some(palette::INK),
                "{name}: the argument is INK"
            );
            assert!(
                header.spans[5..]
                    .iter()
                    .all(|s| s.style.fg != Some(palette::FAINT)),
                "{name}: the outcome is never FAINT"
            );
        }
    }

    #[test]
    fn golden_frames_per_tool() {
        // Fixture in, expected frame out, at the 80-column floor.
        let cases: [(&str, &str, ToolStatus, &str, &[&str]); 5] = [
            (
                "read",
                r#"{"file_path":"src/lib.rs"}"#,
                ToolStatus::Ok,
                "     1\tfn main() {}\n     2\t}",
                &[
                    "  ▸ read      src/lib.rs                                    ✓ 2 lines · 0.0 kB",
                    "    1  fn main() {}",
                    "    2  }",
                ],
            ),
            (
                "shell",
                r#"{"command":"cargo test"}"#,
                ToolStatus::Error,
                "boom\n[exit code: 101]",
                &[
                    "  ▸ shell     cargo test                                    ✗ 11.4s · exit 101",
                    "  boom",
                    "  exit 101 · 2 lines",
                ],
            ),
            (
                "write",
                r#"{"file_path":"a.rs","content":"one\ntwo"}"#,
                ToolStatus::Ok,
                "Wrote a.rs (7 bytes).",
                &[
                    "  ▸ write     a.rs                                                   ✓ 2 lines",
                    "    1  + one",
                    "    2  + two",
                ],
            ),
            (
                "edit",
                r#"{"file_path":"a.rs","old_string":"old","new_string":"new"}"#,
                ToolStatus::Ok,
                "Edited a.rs (1 replacement).",
                &[
                    "  ▸ edit      a.rs                                      ✓ +1 −1 · 1 of 1 files",
                    "       − old",
                    "       + new",
                ],
            ),
            (
                "search",
                r#"{"pattern":"block"}"#,
                ToolStatus::Ok,
                "a.rs:1:block_until_ready",
                &[
                    "  ▸ search    block                                                   ✓ 1 hits",
                    "  a.rs:1:block_until_ready",
                ],
            ),
        ];
        for (name, input, status, output, expected) in cases {
            let lines = lines(&row(name, input, status, Some(output)), 80, 0, true);
            let got = text(&lines);
            let trimmed: Vec<String> = got.iter().map(|l| l.trim_end().to_string()).collect();
            assert_eq!(trimmed, expected, "golden frame for {name}");
        }
    }
}
