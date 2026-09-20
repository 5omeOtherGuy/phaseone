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
use crate::wrap::wrap;

use super::{elapsed, fill};

/// The name field of the §3 column grid, after the glyph prefix.
const NAME_FIELD: usize = 10;

/// Render the whole transcript to lines on a `width`-column grid.
/// `working` is the live working-indicator label (e.g. "running tests") while
/// the agent is mid-turn; `now_ms` drives the LED chase (fake time in tests).
pub fn lines(
    transcript: &Transcript,
    width: usize,
    working: Option<&str>,
    now_ms: u64,
    reduced_motion: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for block in &transcript.blocks {
        block_lines(block, width, &mut out);
    }
    if let Some(label) = working {
        out.push(working_line(label, width, now_ms, reduced_motion));
    }
    out
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
        fold_lines(&fold, width, row.depth, out);
    }
}

/// The §3 row: `▸ read      src/lib.rs              ✓ 412 lines`.
fn call_row(row: &ToolRow, width: usize) -> Line<'static> {
    let indent = "  ".repeat(row.depth.min(1) as usize);
    let (glyph, glyph_fg) = match row.status {
        RowStatus::Running => (glyphs::TOOL, palette::DIM),
        RowStatus::Settled(ToolStatus::Ok) => (glyphs::DONE, palette::DIM),
        RowStatus::Settled(ToolStatus::Error) => (glyphs::FAILED, palette::INK),
        RowStatus::Settled(ToolStatus::Denied) => (glyphs::FAILED, palette::INK),
        RowStatus::Settled(_) => (glyphs::PENDING, palette::FAINT),
    };
    let mut name: String = row.name.chars().take(NAME_FIELD).collect();
    while name.chars().count() < NAME_FIELD {
        name.push(' ');
    }
    let mut spans = vec![
        Span::styled(indent.clone(), Style::new().fg(palette::DIM)),
        Span::styled(format!("{glyph} "), Style::new().fg(glyph_fg)),
        Span::styled(name, Style::new().fg(palette::DIM)),
        Span::styled(row.summary.clone(), Style::new().fg(palette::DIM)),
    ];
    // The right-aligned result: status word + evidence, never FAINT.
    if let RowStatus::Settled(status) = row.status {
        let mut result = String::new();
        if let Some(ms) = row.elapsed_ms {
            result.push_str(&elapsed(ms));
        }
        if let Some(output) = &row.output {
            if !result.is_empty() {
                result.push_str(" · ");
            }
            result.push_str(&format!("{} lines", output.lines().count()));
        }
        if result.is_empty() {
            result.push_str(status_word(status));
        }
        if status == ToolStatus::Denied {
            result = format!("denied · {result}");
        }
        let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
        let pad = width.saturating_sub(used + result.chars().count() + 1);
        spans.push(Span::raw(" ".repeat(pad)));
        spans.push(Span::styled(result, Style::new().fg(palette::DIM)));
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
fn fold_lines(fold: &Fold, width: usize, depth: u8, out: &mut Vec<Line<'static>>) {
    let indent = "  ".repeat(depth.min(1) as usize + 1);
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
        let shown: String = line
            .chars()
            .take(width.saturating_sub(indent.len()))
            .collect();
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
    let used: usize = spans.iter().map(|s| s.content.chars().count()).sum();
    let _ = width.saturating_sub(used);
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
        let lines = lines(&transcript_with_call(), 60, None, 0, true);
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
        let lines = lines(&t, 60, None, 0, true);
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
    fn reduced_motion_freezes_the_chase() {
        let frozen = lines(&Transcript::new(), 60, Some("running tests"), 0, true);
        // Every cell at full INK: a static `▪▪▪`.
        for cell in 0..glyphs::WORKING_CELLS {
            assert_eq!(frozen[0].spans[cell].style.fg, Some(palette::INK));
        }
        // Live, the same instant dims cells below the floor's opposite end.
        let live = lines(&Transcript::new(), 60, Some("running tests"), 0, false);
        assert_ne!(live[0].spans[0].style.fg, Some(palette::INK));
    }
}
