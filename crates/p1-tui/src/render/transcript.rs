//! The transcript renderer (BLOCK-SPEC §1–§3). A transcript is a vertical list
//! of events; operator input and assistant prose sit on GROUND with no chrome,
//! a tool event earns the three-band block (§2). Events are separated by one
//! blank GROUND row. No box-drawing, no role labels.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use super::block;
use crate::glyphs;
use crate::palette;
use crate::transcript::{Block, RowStatus, Transcript};
use crate::wrap::wrap;

use super::{dimmed, elapsed};

/// Render the whole transcript to lines on a `width`-column grid.
/// `working` is the live working-indicator label while the agent is mid-turn
/// and no tool block is running; `now_ms` drives the LED chase (fake time in
/// tests).
pub fn lines(
    transcript: &Transcript,
    width: usize,
    working: Option<&str>,
    now_ms: u64,
    reduced_motion: bool,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut first = true;
    for block in &transcript.blocks {
        if !first {
            out.push(Line::default());
        }
        first = false;
        block_lines(block, width, now_ms, reduced_motion, &mut out);
    }
    // A running tool block carries the chase in its header, so the standalone
    // label would be a second animated thing. It appears only while the agent
    // is working without a running block (e.g. assistant streaming).
    let has_running = transcript
        .blocks
        .iter()
        .any(|b| matches!(b, Block::Call(row) if row.status == RowStatus::Running));
    if let Some(label) = working
        && !has_running
    {
        if !first {
            out.push(Line::default());
        }
        out.push(working_line(label, now_ms, reduced_motion));
    }
    out
}

fn block_lines(
    block: &Block,
    width: usize,
    now_ms: u64,
    reduced_motion: bool,
    out: &mut Vec<Line<'static>>,
) {
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
        Block::Call(row) => out.extend(block::lines(row, width, now_ms, reduced_motion)),
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

/// The working indicator (SPEC §2): three `▪` cells chasing on a 1.1 s cycle,
/// frozen to a static `▪▪▪` under reduced motion. Own line, short label, INK.
fn working_line(label: &str, now_ms: u64, reduced_motion: bool) -> Line<'static> {
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
    Line::from(spans)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};

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
                    input: ToolInput::Json(r#"{"file_path":"src/edge.rs"}"#.into()),
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
                    content: "     1\ta\n     2\tb\n     3\tc".into(),
                },
            },
            Some(412),
        );
        t
    }

    #[test]
    fn events_are_separated_by_one_blank_ground_row() {
        let lines = lines(&transcript_with_call(), 60, None, 0, true);
        let text = plain(&lines);
        assert_eq!(text[0], "› why does compaction stall?");
        assert_eq!(text[1], "");
        assert_eq!(text[2], "Checking.");
        assert_eq!(text[3], "");
        assert!(text[4].trim_start().starts_with("▸ read"), "{}", text[4]);
    }

    #[test]
    fn a_tool_event_earns_the_three_band_block() {
        let lines = lines(&transcript_with_call(), 60, None, 0, true);
        // Band A on BLOCK+, body rows on BLOCK.
        assert!(
            lines[4]
                .spans
                .iter()
                .all(|s| s.style.bg == Some(palette::BLOCK_PLUS))
        );
        assert_eq!(lines[5].spans[0].style.bg, Some(palette::BLOCK));
        // Band A is one row; the read body carries FAINT line numbers.
        let text = plain(&lines);
        assert!(text[4].contains("▸ read      src/edge.rs"));
        assert!(text[4].trim_end().ends_with("✓ 3 lines · 0.0 kB"));
        assert_eq!(text[5].trim(), "1  a");
    }

    #[test]
    fn a_running_call_animates_its_header_not_a_second_line() {
        let mut t = Transcript::new();
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json(r#"{"command":"cargo test"}"#.into()),
                },
            },
            None,
        );
        let lines = lines(&t, 60, Some("shell"), 0, true);
        let text = plain(&lines);
        assert_eq!(text.len(), 1, "no standalone working line: {text:?}");
        assert!(text[0].trim_start().starts_with("▸ shell     cargo test"));
        assert!(text[0].contains("▪▪▪"));
    }

    #[test]
    fn a_failed_call_shows_the_failure_marker_in_the_header() {
        let mut t = Transcript::new();
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json(r#"{"command":"cargo test"}"#.into()),
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
                    content: "test one ... FAILED\n[exit code: 101]".into(),
                },
            },
            Some(11_400),
        );
        let lines = lines(&t, 60, None, 0, true);
        let text = plain(&lines);
        assert!(text[0].contains("✗ 11.4s · exit 101"), "{}", text[0]);
        assert_eq!(text[1].trim(), "test one ... FAILED");
        assert_eq!(lines[1].spans[0].style.bg, Some(palette::BLOCK));
    }

    #[test]
    fn a_long_summary_truncates_so_the_outcome_never_overflows() {
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
                    content: "ok\n[exit code: 0]".into(),
                },
            },
            Some(120),
        );
        let width = 40;
        let lines = lines(&t, width, None, 0, true);
        let text = plain(&lines);
        assert!(text[0].chars().count() <= width, "never overflows the grid");
        assert!(text[0].contains('…'));
        assert!(text[0].trim_end().ends_with("120ms"));
    }

    #[test]
    fn reduced_motion_freezes_the_chase() {
        let frozen = lines(&Transcript::new(), 60, Some("running tests"), 0, true);
        let cells: Vec<&Span<'static>> = frozen[0]
            .spans
            .iter()
            .filter(|s| s.content == glyphs::WORKING.to_string())
            .collect();
        assert_eq!(cells.len(), glyphs::WORKING_CELLS);
        assert!(cells.iter().all(|s| s.style.fg == Some(palette::INK)));
        let live = lines(&Transcript::new(), 60, Some("running tests"), 0, false);
        let live_cell = live[0]
            .spans
            .iter()
            .find(|s| s.content == glyphs::WORKING.to_string())
            .expect("a working cell");
        assert_ne!(live_cell.style.fg, Some(palette::INK));
    }
}
