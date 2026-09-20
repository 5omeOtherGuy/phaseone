//! The transcript renderer (SPEC §3, §4). Column grid: glyph, a 10-column name
//! field, the argument, then the right-aligned result. Indentation is 2 cells
//! per delegation depth — once and never more. No box-drawing, no role labels;
//! tool output earns chrome (a BLOCK background), conversation does not.

use p1_contracts::ToolStatus;
use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::fold::Fold;
use crate::glyphs;
use crate::palette;
use crate::transcript::{Block, RowStatus, ToolRow, Transcript};
use crate::wrap::{wrap, wrap_len};

use super::{elapsed, fill};

/// The name field of the §3 column grid, after the glyph prefix.
const NAME_FIELD: usize = 10;

/// Render the transcript tail to lines on a `width`-column grid, building at
/// most `max_rows` rows from the END. The visible window is always at the
/// tail: blocks are append-only and only the LAST block can be streaming-open,
/// so older blocks are immutable and off-screen rows need never be built.
///
/// `working` is the live working-indicator label (e.g. "running tests") while
/// the agent is mid-turn; `now_ms` drives the LED chase (fake time in tests).
/// The working line is appended after windowing, so it is always present.
pub fn lines(
    transcript: &Transcript,
    width: usize,
    max_rows: usize,
    working: Option<&str>,
    now_ms: u64,
    reduced_motion: bool,
) -> Vec<Line<'static>> {
    let mut chunks: Vec<Vec<Line<'static>>> = Vec::new();
    let mut collected = 0usize;
    for block in transcript.blocks.iter().rev() {
        if collected >= max_rows {
            break;
        }
        let mut block_out = Vec::new();
        block_lines(block, width, &mut block_out);
        collected += block_out.len();
        chunks.push(block_out);
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(collected);
    for chunk in chunks.into_iter().rev() {
        out.extend(chunk);
    }
    // A block that crossed the budget can push rows past it; the oldest rows
    // are the off-screen ones, so keep exactly the tail.
    if out.len() > max_rows {
        out.drain(..out.len() - max_rows);
    }
    if let Some(label) = working {
        out.push(working_line(label, width, now_ms, reduced_motion));
    }
    out
}

/// The transcript's full height on a `width`-column grid, without building a
/// single `Line`. The tail pass needs the total so the scroll math keeps its
/// absolute frame and a scrolled window can be located from the end.
pub fn count_rows(transcript: &Transcript, width: usize) -> usize {
    transcript
        .blocks
        .iter()
        .map(|block| block_rows(block, width))
        .sum()
}

fn block_rows(block: &Block, width: usize) -> usize {
    match block {
        Block::Operator { text } => wrap_len(text, width.saturating_sub(2)),
        Block::Prose { lines } => lines.iter().map(|line| wrap_len(line, width)).sum(),
        Block::Reasoning {
            lines, expanded, ..
        } => {
            let body = if *expanded {
                lines
                    .iter()
                    .map(|line| wrap_len(line, width.saturating_sub(2)))
                    .sum()
            } else {
                0
            };
            1 + body
        }
        Block::Call(row) => 1 + fold_rows(row),
        Block::Info { lines } => lines.len(),
        Block::Meta { .. } => 1,
        Block::Notice { lines } => lines.iter().map(|line| wrap_len(line, width)).sum(),
    }
}

fn fold_rows(row: &ToolRow) -> usize {
    match row.fold() {
        None => 0,
        Some(Fold::Full { lines }) => lines.len(),
        Some(Fold::Folded { head, .. }) => head.len() + 1,
    }
}

fn block_lines(block: &Block, width: usize, out: &mut Vec<Line<'static>>) {
    match block {
        Block::Operator { text } => {
            let ink = Style::new().fg(palette::INK);
            let mut first = true;
            for part in wrap(text, width.saturating_sub(2)) {
                if first {
                    out.push(Line::from(vec![
                        Span::styled(format!("{} ", glyphs::OPERATOR), ink),
                        Span::styled(part.clone(), ink),
                    ]));
                    first = false;
                } else {
                    out.push(Line::styled(format!("  {part}"), ink));
                }
            }
        }
        Block::Prose { lines, .. } => {
            for line in lines {
                for part in wrap(line, width) {
                    out.push(Line::styled(part, Style::new().fg(palette::INK)));
                }
            }
        }
        Block::Reasoning {
            lines: body,
            expanded,
            elapsed_ms,
            ..
        } => {
            let label = match elapsed_ms {
                Some(ms) => format!("{} reasoning {}", glyphs::PENDING, elapsed(*ms)),
                None => format!("{} reasoning", glyphs::PENDING),
            };
            let hint = "^R expand";
            let pad = width.saturating_sub(label.chars().count() + hint.len());
            out.push(Line::from(vec![
                Span::styled(label, Style::new().fg(palette::FAINT)),
                Span::raw(" ".repeat(pad)),
                Span::styled(hint, Style::new().fg(palette::FAINT)),
            ]));
            if *expanded {
                for line in body {
                    for part in wrap(line, width.saturating_sub(2)) {
                        out.push(Line::styled(
                            format!("  {part}"),
                            Style::new().fg(palette::DIM),
                        ));
                    }
                }
            }
        }
        Block::Call(row) => call_lines(row, width, out),
        Block::Info { lines } => {
            for line in lines {
                let shown: String = line.chars().take(width).collect();
                out.push(Line::styled(shown, Style::new().fg(palette::INK)));
            }
        }
        Block::Meta { text } => {
            out.push(Line::styled(text.clone(), Style::new().fg(palette::DIM)));
        }
        Block::Notice { lines } => {
            // §4.9: state what broke, no banner. The first line is INK — it is
            // the fact; the rest are DIM detail.
            let mut first = true;
            for line in lines {
                let style = if first {
                    Style::new().fg(palette::INK)
                } else {
                    Style::new().fg(palette::DIM)
                };
                for part in wrap(line, width) {
                    out.push(Line::styled(part, style));
                }
                first = false;
            }
        }
    }
}

/// One call row plus its fold block, when the row earned one.
fn call_lines(row: &ToolRow, width: usize, out: &mut Vec<Line<'static>>) {
    out.push(call_row(row, width));
    if let Some(fold) = row.fold() {
        fold_lines(&fold, width, out);
    }
}

/// The §3 row: `▸ read      src/lib.rs              ✓ 412 lines`.
fn call_row(row: &ToolRow, width: usize) -> Line<'static> {
    let indent = "";
    let (glyph, glyph_fg) = match row.status {
        RowStatus::Running => (glyphs::TOOL, palette::DIM),
        RowStatus::Settled(ToolStatus::Ok) => (glyphs::DONE, palette::DIM),
        RowStatus::Settled(ToolStatus::Error) => (glyphs::FAILED, palette::INK),
        RowStatus::Settled(ToolStatus::Denied) => (glyphs::FAILED, palette::INK),
        RowStatus::Settled(_) => (glyphs::PENDING, palette::FAINT),
    };
    // The name field is exactly NAME_FIELD cells; a name that would fill it
    // completely still gets one blank separator before the argument.
    let name = {
        let cut: String = row.name.chars().take(NAME_FIELD - 1).collect();
        format!("{cut:<NAME_FIELD$}")
    };
    let mut spans = vec![
        Span::styled(indent, Style::new().fg(palette::DIM)),
        Span::styled(format!("{glyph} "), Style::new().fg(glyph_fg)),
        Span::styled(name, Style::new().fg(palette::DIM)),
        Span::styled(row.summary.clone(), Style::new().fg(palette::DIM)),
    ];
    // The right-aligned result: status word + evidence, never FAINT. A failed
    // call's first output line is part of the evidence (SPEC §3's
    // `✗ 11.4s · 12 passed, 1 failed · 94 lines`).
    if let RowStatus::Settled(status) = row.status {
        let mut parts: Vec<String> = Vec::new();
        if let Some(ms) = row.elapsed_ms {
            parts.push(elapsed(ms));
        }
        if status != ToolStatus::Ok
            && let Some(first) = row
                .output
                .as_deref()
                .map(str::lines)
                .and_then(|mut lines| lines.next())
                .filter(|line| !line.is_empty())
        {
            let mut first: String = first.chars().take(40).collect();
            if first.chars().count() == 40 {
                first.push('\u{2026}');
            }
            parts.push(first);
        }
        if row.line_count > 0 {
            let n = row.line_count;
            parts.push(if n == 1 {
                "1 line".into()
            } else {
                format!("{n} lines")
            });
        }
        let mut result = parts.join(" · ");
        if result.is_empty() {
            result.push_str(status_word(status));
        }
        if status == ToolStatus::Denied && result != status_word(status) {
            result = format!("denied · {result}");
        }
        // The result wins the row's right edge; the summary truncates with `…`
        // until the row fits (the grid rule: nothing overflows).
        let fixed: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let budget = width.saturating_sub(fixed + result.chars().count() + 1);
        if row.summary.chars().count() > budget {
            let keep = budget.saturating_sub(1);
            let mut cut: String = row.summary.chars().take(keep).collect();
            cut.push('…');
            let span = spans.last_mut().expect("the summary span exists");
            *span = Span::styled(cut, span.style);
        }
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = width.saturating_sub(used + result.chars().count() + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(result, Style::new().fg(palette::DIM)));
        // An output big enough to fold is addressable: `^O open` (FAINT hint).
        if row
            .output
            .as_deref()
            .is_some_and(|o| o.lines().count() > crate::fold::FULL_BLOCK_MAX_LINES)
        {
            spans.push(Span::styled("   ^O open", Style::new().fg(palette::FAINT)));
        }
    }
    Line::from(spans)
}

/// The settled-status word when there is no other evidence to show.
fn status_word(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Ok => "ok",
        ToolStatus::Error => "error",
        ToolStatus::Unavailable => "unavailable",
        ToolStatus::Denied => "denied",
        ToolStatus::Cancelled => "cancelled",
        ToolStatus::Unknown => "unknown",
    }
}

/// A fold block: head lines DIM on BLOCK, then the FAINT handle line. The
/// block is padded to the full width so it reads as one surface (SPEC §3).
fn fold_lines(fold: &Fold, width: usize, out: &mut Vec<Line<'static>>) {
    let indent = "  ".to_string();
    let (head, handle) = match fold {
        Fold::Full { lines } => (lines.as_slice(), None),
        Fold::Folded { head, folded, id } => (
            head.as_slice(),
            Some(format!(
                "{} {} more lines folded → [{}]",
                glyphs::PENDING,
                folded,
                id
            )),
        ),
    };
    for line in head {
        let room = width.saturating_sub(indent.len());
        let shown = if line.chars().count() > room {
            let mut cut: String = line.chars().take(room.saturating_sub(1)).collect();
            cut.push('\u{2026}');
            cut
        } else {
            line.clone()
        };
        out.push(fill(
            Line::styled(format!("{indent}{shown}"), Style::new().fg(palette::DIM)),
            width,
            palette::BLOCK,
        ));
    }
    if let Some(handle) = handle {
        out.push(fill(
            Line::styled(format!("{indent}{handle}"), Style::new().fg(palette::FAINT)),
            width,
            palette::BLOCK,
        ));
    }
}

/// The working indicator (SPEC §2): three `▪` cells chasing on a 1.1 s cycle,
/// frozen to a static `▪▪▪` under reduced motion. Own line, short label, INK.
fn working_line(label: &str, width: usize, now_ms: u64, reduced_motion: bool) -> Line<'static> {
    let mut spans = Vec::new();
    for cell in 0..glyphs::WORKING_CELLS {
        let opacity = if reduced_motion {
            1.0
        } else {
            glyphs::working_opacity(cell, now_ms)
        };
        spans.push(Span::styled(
            glyphs::WORKING.to_string(),
            Style::new().fg(dimmed(palette::INK, opacity)),
        ));
    }
    spans.push(Span::styled(
        format!(" {label}"),
        Style::new().fg(palette::DIM),
    ));
    let _ = width;
    Line::from(spans)
}

/// Scale a palette colour toward the ground by `opacity` (0.0–1.0). The LED
/// chase dims its cells; it never introduces a hue.
fn dimmed(color: ratatui::style::Color, opacity: f32) -> ratatui::style::Color {
    let ratatui::style::Color::Rgb(r, g, b) = color else {
        return color;
    };
    let scale = |v: u8| (v as f32 * opacity) as u8;
    ratatui::style::Color::Rgb(scale(r), scale(g), scale(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem};

    fn plain(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }

    fn transcript_with_call() -> Transcript {
        let mut t = Transcript::new();
        t.operator("why does compaction stall?");
        t.apply(
            &AgentEvent::TextDelta {
                text: "Checking.".into(),
            },
            None,
        );
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "read".into(),
                    input: ToolInput::Json("src/edge.rs".into()),
                },
            },
            None,
        );
        t.apply(
            &AgentEvent::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "read".into(),
                    status: ToolStatus::Ok,
                    content: "a\nb\nc".into(),
                },
            },
            Some(412),
        );
        t
    }

    #[test]
    fn the_column_grid_holds() {
        let lines = lines(&transcript_with_call(), 60, usize::MAX, None, 0, true);
        let text = plain(&lines);
        assert_eq!(text[0], "› why does compaction stall?");
        assert_eq!(text[1], "Checking.");
        // Glyph + 10-column name field, argument, right-aligned result.
        let expected = format!(
            "{} read      src/edge.rs{}412ms · 3 lines",
            glyphs::DONE,
            " ".repeat(60 - 23 - 15 - 1)
        );
        assert_eq!(text[2], expected);
    }

    #[test]
    fn a_failed_call_shows_its_evidence_on_block() {
        let mut t = Transcript::new();
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json("cargo test".into()),
                },
            },
            None,
        );
        t.apply(
            &AgentEvent::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    status: ToolStatus::Error,
                    content: "test one ... FAILED\nassertion failed".into(),
                },
            },
            Some(11_400),
        );
        let lines = lines(&t, 60, usize::MAX, None, 0, true);
        let text = plain(&lines);
        // The row carries the failure; the evidence block follows on BLOCK.
        assert!(text[0].contains(&glyphs::FAILED.to_string()));
        assert_eq!(text[1].trim_end(), "  test one ... FAILED");
        assert_eq!(text[2].trim_end(), "  assertion failed");
        assert_eq!(
            lines[1].spans[0].style.bg,
            Some(palette::BLOCK),
            "tool output earns chrome"
        );
    }

    #[test]
    fn a_long_summary_truncates_so_the_result_never_overflows() {
        let mut t = Transcript::new();
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json(
                        r#"{"command":"cargo test -p p1-provider-http --all-features -- --nocapture"}"#
                            .into(),
                    ),
                },
            },
            None,
        );
        t.apply(
            &AgentEvent::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    status: ToolStatus::Ok,
                    content: "ok".into(),
                },
            },
            Some(120),
        );
        let width = 40;
        let lines = lines(&t, width, usize::MAX, None, 0, true);
        let text = plain(&lines);
        // The row never exceeds the grid width and ends in the truncation
        // mark before the right-aligned result.
        assert!(text[0].chars().count() < width, "never overflows the grid");
        assert!(text[0].contains('…'));
        assert!(text[0].trim_end().ends_with("120ms · 1 line"));
    }

    #[test]
    fn reduced_motion_freezes_the_chase() {
        let frozen = lines(
            &Transcript::new(),
            60,
            usize::MAX,
            Some("running tests"),
            0,
            true,
        );
        // Every cell at full INK: a static `▪▪▪`.
        for cell in 0..glyphs::WORKING_CELLS {
            assert_eq!(frozen[0].spans[cell].style.fg, Some(palette::INK));
        }
        // Live, the same instant dims cells below the floor's opposite end.
        let live = lines(
            &Transcript::new(),
            60,
            usize::MAX,
            Some("running tests"),
            0,
            false,
        );
        assert_ne!(live[0].spans[0].style.fg, Some(palette::INK));
    }

    #[test]
    fn count_rows_matches_the_full_render() {
        let mut mixed = transcript_with_call();
        mixed.apply(
            &AgentEvent::ReasoningDelta {
                text: "weigh the two designs carefully".into(),
            },
            None,
        );
        mixed.apply(
            &AgentEvent::TextDelta {
                text: "a long sentence that must wrap more than once at a narrow width".into(),
            },
            None,
        );
        for t in [transcript_with_call(), mixed, Transcript::new()] {
            for width in [1usize, 7, 40, 120] {
                let built = lines(&t, width, usize::MAX, None, 0, true);
                assert_eq!(
                    count_rows(&t, width),
                    built.len(),
                    "count_rows disagrees with a full render at width {width}"
                );
            }
        }
    }

    #[test]
    fn a_5000_block_transcript_builds_only_the_last_screenful() {
        let mut t = Transcript::new();
        for n in 0..5_000 {
            t.blocks.push(Block::Prose {
                lines: vec![format!("prose line {n}")],
            });
        }
        let width = 120;
        let max_rows = 40;
        // The pre-change reference: build everything, then window it.
        let full = lines(&t, width, usize::MAX, None, 0, true);
        assert_eq!(full.len(), 5_000);
        let tail = lines(&t, width, max_rows, Some("running tests"), 0, true);
        // The working line is appended after windowing; the transcript rows
        // never exceed the budget.
        assert!(
            tail.len() <= max_rows + 1,
            "tail is {} rows for a {max_rows}-row budget",
            tail.len()
        );
        assert!(plain(&tail[tail.len() - 1..])[0].contains("running tests"));
        assert_eq!(
            plain(&tail[..max_rows]),
            plain(&full[full.len() - max_rows..]),
            "the visible tail is identical to a full render's last screenful"
        );
    }

    #[test]
    fn a_budget_that_cuts_a_block_keeps_the_newest_rows() {
        // Three rows per block: the budget lands mid-block, so the oldest
        // rows of the crossing block must be the ones dropped.
        let mut t = Transcript::new();
        for n in 0..50 {
            t.blocks.push(Block::Prose {
                lines: vec![
                    format!("b{n} one"),
                    format!("b{n} two"),
                    format!("b{n} three"),
                ],
            });
        }
        let full = lines(&t, 40, usize::MAX, None, 0, true);
        let tail = lines(&t, 40, 20, None, 0, true);
        assert_eq!(tail.len(), 20);
        assert_eq!(plain(&tail), plain(&full[full.len() - 20..]));
    }
}
