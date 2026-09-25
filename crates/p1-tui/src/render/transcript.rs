//! The transcript renderer (handoff §5, §6). Every element is a stack of
//! Band rows at the transcript width with 2 cells of padding, so text starts
//! at the text column; one blank GROUND row separates events and none
//! separates the bands of one event. Tool output earns chrome (a Block);
//! conversation does not.

use ratatui::style::Color;
use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::glyphs;
use crate::palette;
use crate::text::{sanitize_segments, sanitize_text};
use crate::transcript::{
    Block, CommandOutput, CommandRow, NoticeFact, NoticeKind, RowStatus, Transcript, TurnNotice,
    TurnPhase, TurnWorking, WorkerEnd, WorkerReport,
};
use crate::wrap::{cell_width, wrap_len, wrap_paragraphs, wrap_paragraphs_len, wrap_styled};

use super::block::{live_elapsed, working_segments};
use super::elapsed;

/// Band padding on each side of the transcript column (§4.1: `U = T − 4`).
const PAD: usize = 2;
/// Continuation rows of operator input, meta rows and headlines hang this far.
const HANG: usize = 2;
/// Notice fact labels are padded to this field (§6.7); values hang at `HANG + FACT_LABEL`.
const FACT_LABEL: usize = 10;
/// The command field of a command output header (§6.9), like a tool's name field.
const COMMAND_FIELD: usize = 10;
/// The key column of a command output entry.
const COMMAND_KEY: usize = 18;
/// The right-hand tag of a steering message delivered mid-turn.
const STEERING: &str = "steering";

/// Render the transcript tail to lines on a `width`-column grid, building at
/// most `max_rows` rows from the END. The visible window is always at the
/// tail: blocks are append-only and only the LAST block can be streaming-open,
/// so older blocks are immutable and off-screen rows need never be built.
///
/// `turn_live` says a turn is running; the turn working row (§6.5) takes its
/// phase, clock and request from the transcript and is drawn after windowing
/// while no started call runs.
/// `now_ms` drives every live clock and the `▪▪▪` pulse (fake time in tests).
pub fn lines(
    transcript: &Transcript,
    width: usize,
    max_rows: usize,
    turn_live: bool,
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
        block_lines(
            transcript,
            block,
            width,
            now_ms,
            reduced_motion,
            &mut block_out,
        );
        collected += block_out.len() + usize::from(!chunks.is_empty());
        chunks.push(block_out);
    }
    let mut out: Vec<Line<'static>> = Vec::with_capacity(collected);
    for (n, chunk) in chunks.into_iter().rev().enumerate() {
        if n > 0 {
            out.push(Line::default());
        }
        out.extend(chunk);
    }
    // A block that crossed the budget can push rows past it; the oldest rows
    // are the off-screen ones, so keep exactly the tail.
    if out.len() > max_rows {
        out.drain(..out.len() - max_rows);
    }
    if turn_live && !transcript.call_running() {
        let facts = transcript.turn_working(now_ms).unwrap_or(TurnWorking {
            phase: TurnPhase::Waiting,
            elapsed_ms: None,
            request: None,
        });
        if !transcript.blocks.is_empty() {
            out.push(Line::default());
        }
        out.push(turn_working_line(&facts, width, now_ms, reduced_motion));
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
        .map(|block| block_rows(transcript, block, width))
        .sum::<usize>()
        .saturating_add(transcript.blocks.len().saturating_sub(1))
}

/// The text measure inside the band padding.
fn measure(width: usize) -> usize {
    width.saturating_sub(2 * PAD)
}

/// Operator input wraps at `U − 2`; a steering tag keeps its column clear.
fn operator_measure(width: usize, steering: bool) -> usize {
    let tag = if steering {
        cell_width(STEERING) + 2
    } else {
        0
    };
    measure(width).saturating_sub(HANG + tag)
}

/// A meta row's text wraps beside its glyph and clear of its right facts.
fn meta_measure(width: usize, facts: &str) -> usize {
    let right = if facts.is_empty() {
        0
    } else {
        cell_width(facts) + 2
    };
    measure(width).saturating_sub(HANG + right)
}

fn fact_measure(width: usize) -> usize {
    measure(width).saturating_sub(HANG + FACT_LABEL)
}

fn block_rows(transcript: &Transcript, block: &Block, width: usize) -> usize {
    match block {
        Block::Operator { text, steering } => {
            wrap_paragraphs_len(&sanitize_text(text), operator_measure(width, *steering))
        }
        Block::Prose { lines } => lines
            .iter()
            .map(|line| {
                let plain: String = sanitized_prose_runs(transcript, line)
                    .into_iter()
                    .map(|(text, _)| text)
                    .collect();
                wrap_len(&plain, measure(width))
            })
            .sum(),
        Block::Reasoning {
            lines, expanded, ..
        } => {
            let body = if *expanded {
                lines
                    .iter()
                    .map(|line| wrap_len(&sanitize_text(line), measure(width).saturating_sub(HANG)))
                    .sum()
            } else {
                0
            };
            1 + body
        }
        Block::Call(row) => super::block::lines(row, width, false, 0, true).len(),
        Block::Info { lines } => lines.len(),
        Block::Meta { text } => {
            wrap_paragraphs_len(&sanitize_text(meta_glyph(text).1), meta_measure(width, ""))
        }
        Block::MetaFacts { text, facts } => wrap_paragraphs_len(
            &sanitize_text(meta_glyph(text).1),
            meta_measure(width, facts),
        ),
        Block::Notice(notice) => {
            wrap_paragraphs_len(
                &sanitize_text(&notice.headline),
                measure(width).saturating_sub(HANG),
            ) + notice
                .facts
                .iter()
                .map(|(_, value)| wrap_paragraphs_len(&sanitize_text(value), fact_measure(width)))
                .sum::<usize>()
        }
        Block::WorkerReport(_) => 3,
        Block::CommandOutput(output) => 1 + output.body.len(),
    }
}

fn block_lines(
    transcript: &Transcript,
    block: &Block,
    width: usize,
    now_ms: u64,
    reduced_motion: bool,
    out: &mut Vec<Line<'static>>,
) {
    match block {
        Block::Operator { text, steering } => operator_lines(text, *steering, width, out),
        Block::Prose { lines } => {
            for line in lines {
                for row in wrap_styled(&sanitized_prose_runs(transcript, line), measure(width)) {
                    let left = row
                        .into_iter()
                        .map(|(text, fg)| Seg::new(fg, text))
                        .collect();
                    out.push(ground(left, Vec::new(), width));
                }
            }
        }
        Block::Reasoning {
            lines: body,
            expanded,
            elapsed_ms,
            started_ms,
        } => {
            // Settled reasoning states its span; streaming reasoning counts live.
            let clock = match (elapsed_ms, started_ms) {
                (Some(ms), _) => format!(" {}", elapsed(*ms)),
                (None, Some(started)) => {
                    format!(" {}", live_elapsed(Some(now_ms.saturating_sub(*started))))
                }
                (None, None) => String::new(),
            };
            let hint = if *expanded { "collapse" } else { "expand" };
            out.push(ground(
                vec![
                    Seg::new(palette::FAINT, format!("{} ", glyphs::PENDING)),
                    Seg::new(palette::DIM, format!("reasoning{clock}")),
                ],
                vec![
                    Seg::new(palette::FAINT, "^R"),
                    Seg::new(palette::FAINT, format!(" {hint}")),
                ],
                width,
            ));
            if *expanded {
                for line in body {
                    for part in
                        crate::wrap::wrap(&sanitize_text(line), measure(width).saturating_sub(HANG))
                    {
                        out.push(ground(
                            vec![Seg::new(palette::DIM, format!("{:HANG$}{part}", ""))],
                            Vec::new(),
                            width,
                        ));
                    }
                }
            }
        }
        Block::Call(row) => {
            // A running call's elapsed is live: measured from its start stamp every frame.
            let live = (row.status == RowStatus::Running)
                .then(|| transcript.call_started(&row.call_id))
                .flatten()
                .map(|started| {
                    let mut live = row.clone();
                    live.elapsed_ms = Some(now_ms.saturating_sub(started));
                    live
                });
            out.extend(super::block::lines(
                live.as_ref().unwrap_or(row),
                width,
                false,
                now_ms,
                reduced_motion,
            ));
        }
        Block::Info { lines } => {
            for line in lines {
                let shown: String = line.chars().take(width).collect();
                out.push(Line::styled(
                    shown,
                    ratatui::style::Style::new().fg(palette::INK),
                ));
            }
        }
        Block::Meta { text } => meta_lines(text, "", width, out),
        Block::MetaFacts { text, facts } => meta_lines(text, facts, width, out),
        Block::Notice(notice) => notice_lines(notice, width, out),
        Block::WorkerReport(report) => worker_report_lines(report, width, out),
        Block::CommandOutput(output) => command_output_lines(output, width, out),
    }
}

/// One GROUND band row: the element rows that carry no surface.
fn ground(left: Vec<Seg>, right: Vec<Seg>, width: usize) -> Line<'static> {
    band(palette::GROUND, left, right, width)
}

fn band(bg: Color, left: Vec<Seg>, right: Vec<Seg>, width: usize) -> Line<'static> {
    Band {
        bg,
        left,
        right,
        width,
        pad: PAD,
    }
    .render()
}

/// §6.2: `›` attn, the prompt in ink hanging 2; a steering message carries a faint tag.
// Adapted from `iris-donor/src/ui/tui/pane.rs` `push_user_rows`, pinned at
// `5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`, MIT License.
fn operator_lines(text: &str, steering: bool, width: usize, out: &mut Vec<Line<'static>>) {
    let clean = sanitize_text(text);
    let parts = wrap_paragraphs(&clean, operator_measure(width, steering));
    // The marker identifies the turn, so blank rows before the first prompt text
    // must not consume the only visual anchor.
    let marked = parts
        .iter()
        .position(|part| !part.trim().is_empty())
        .unwrap_or(0);
    for (n, part) in parts.into_iter().enumerate() {
        if n == marked {
            let tag = if steering {
                vec![Seg::new(palette::FAINT, STEERING)]
            } else {
                Vec::new()
            };
            if steering && operator_measure(width, steering) == 0 {
                // The tag consumes the whole measured body at very narrow widths;
                // put the marker beside it so the visual anchor is not truncated.
                out.push(ground(
                    Vec::new(),
                    vec![
                        Seg::new(palette::ATTN, format!("{} ", glyphs::OPERATOR)),
                        Seg::new(palette::FAINT, STEERING),
                    ],
                    width,
                ));
            } else {
                out.push(ground(
                    vec![
                        Seg::new(palette::ATTN, format!("{} ", glyphs::OPERATOR)),
                        Seg::new(palette::INK, part),
                    ],
                    tag,
                    width,
                ));
            }
        } else {
            out.push(ground(
                vec![Seg::new(palette::INK, format!("{:HANG$}{part}", ""))],
                Vec::new(),
                width,
            ));
        }
    }
}

/// §6.3: prose is ink; a backticked span becomes a bare ref only when the host
/// says the path exists — anything else keeps its backticks, verbatim.
fn prose_runs(transcript: &Transcript, line: &str) -> Vec<(String, Color)> {
    let mut runs = Vec::new();
    let mut ink = String::new();
    let mut rest = line;
    while let Some(open) = rest.find('`') {
        let after = &rest[open + 1..];
        let Some(close) = after.find('`') else {
            break;
        };
        let inner = &after[..close];
        ink.push_str(&rest[..open]);
        if !inner.is_empty() && transcript.path_exists(inner) {
            if !ink.is_empty() {
                runs.push((std::mem::take(&mut ink), palette::INK));
            }
            runs.push((inner.to_string(), palette::REF));
        } else {
            ink.push('`');
            ink.push_str(inner);
            ink.push('`');
        }
        rest = &after[close + 1..];
    }
    ink.push_str(rest);
    if !ink.is_empty() || runs.is_empty() {
        runs.push((ink, palette::INK));
    }
    runs
}

/// Sanitize prose runs as one stream so controls split across run boundaries
/// cannot become visible wrapping cells.
///
/// The render-time ordering follows `iris-donor/src/ui/textengine.rs`'s
/// `clean_text`/`transform` before `wrap_to_width`, pinned at
/// `5b04a1ad3412ad0bb663b6355f77a024aec0ddfa`, MIT License.
fn sanitized_prose_runs(transcript: &Transcript, line: &str) -> Vec<(String, Color)> {
    let segments: Vec<Seg> = prose_runs(transcript, line)
        .into_iter()
        .map(|(text, fg)| Seg::new(fg, text))
        .collect();
    sanitize_segments(&segments)
        .into_iter()
        .map(|segment| (segment.text, segment.fg))
        .collect()
}

/// Split a meta row's leading glyph off its text: `·` is faint, `↳` dim (§3.3).
fn meta_glyph(text: &str) -> (Option<Seg>, &str) {
    for (glyph, fg) in [
        (glyphs::PENDING, palette::FAINT),
        (glyphs::NESTED, palette::DIM),
    ] {
        if let Some(rest) = text.strip_prefix(glyph).and_then(|r| r.strip_prefix(' ')) {
            return (Some(Seg::new(fg, format!("{glyph} "))), rest);
        }
    }
    (None, text)
}

/// §6.6: glyph, dim text hanging 2, optional dim facts on the right of the first row.
fn meta_lines(text: &str, facts: &str, width: usize, out: &mut Vec<Line<'static>>) {
    let (glyph, body) = meta_glyph(text);
    let clean = sanitize_text(body);
    for (n, part) in wrap_paragraphs(&clean, meta_measure(width, facts))
        .into_iter()
        .enumerate()
    {
        if n == 0 {
            let right = if facts.is_empty() {
                Vec::new()
            } else {
                vec![Seg::new(palette::DIM, facts)]
            };
            let left = glyph
                .clone()
                .into_iter()
                .chain([Seg::new(palette::DIM, part)])
                .collect();
            out.push(ground(left, right, width));
        } else {
            out.push(ground(
                vec![Seg::new(palette::DIM, format!("{:HANG$}{part}", ""))],
                Vec::new(),
                width,
            ));
        }
    }
}

/// §6.7: glyph + ink headline, then `label  value` fact rows; `next` is a faint hint.
fn notice_lines(notice: &TurnNotice, width: usize, out: &mut Vec<Line<'static>>) {
    let (glyph, fg) = match notice.kind {
        NoticeKind::Failed => (glyphs::FAILED, palette::FAIL),
        NoticeKind::Stopped => (glyphs::PENDING, palette::FAINT),
    };
    let headline = sanitize_text(&notice.headline);
    for (n, part) in wrap_paragraphs(&headline, measure(width).saturating_sub(HANG))
        .into_iter()
        .enumerate()
    {
        let mut left = Vec::new();
        if n == 0 {
            left.push(Seg::new(fg, format!("{glyph} ")));
            left.push(Seg::new(palette::INK, part));
        } else {
            left.push(Seg::new(palette::INK, format!("{:HANG$}{part}", "")));
        }
        out.push(ground(left, Vec::new(), width));
    }
    for (fact, value) in &notice.facts {
        let value_fg = if *fact == NoticeFact::Next {
            palette::FAINT
        } else {
            palette::INK
        };
        let clean = sanitize_text(value);
        for (n, part) in wrap_paragraphs(&clean, fact_measure(width))
            .into_iter()
            .enumerate()
        {
            let label = if n == 0 { fact.label() } else { "" };
            out.push(ground(
                vec![
                    Seg::new(palette::DIM, format!("{:HANG$}{label:<FACT_LABEL$}", "")),
                    Seg::new(value_fg, part),
                ],
                Vec::new(),
                width,
            ));
        }
    }
}

/// §6.8: three settled BLOCK bands — who and where, what it could do, how it ended.
fn worker_report_lines(report: &WorkerReport, width: usize, out: &mut Vec<Line<'static>>) {
    let (glyph, fg) = match report.end {
        WorkerEnd::Done | WorkerEnd::NotVerified => (glyphs::DONE, palette::OK),
        WorkerEnd::Blocked | WorkerEnd::Failed | WorkerEnd::Stalled => {
            (glyphs::FAILED, palette::FAIL)
        }
        WorkerEnd::Cancelled => (glyphs::PENDING, palette::FAINT),
    };
    let elapsed = report
        .elapsed
        .clone()
        .unwrap_or_else(|| super::UNKNOWN.into());
    let cost = report
        .cost_micro_usd
        .map_or_else(|| super::UNKNOWN.into(), super::cost_string);
    out.push(band(
        palette::BLOCK,
        vec![
            Seg::new(fg, format!("{glyph} ")),
            Seg::new(palette::INK, format!("{:<4} ", report.id)),
            Seg::new(palette::DIM, report.route.clone()),
        ],
        vec![
            Seg::new(palette::INK, elapsed),
            Seg::new(palette::DIM, format!(" {} ", glyphs::PENDING)),
            Seg::new(palette::INK, cost),
        ],
        width,
    ));
    out.push(band(
        palette::BLOCK,
        vec![
            Seg::new(palette::DIM, format!("{:HANG$}{:<8}", "", "grants")),
            Seg::new(palette::INK, report.grants.join(" ")),
        ],
        Vec::new(),
        width,
    ));
    out.push(band(
        palette::BLOCK,
        vec![
            Seg::new(palette::DIM, format!("{:HANG$}{} ", "", glyphs::NESTED)),
            Seg::new(palette::INK, report.line.clone()),
        ],
        Vec::new(),
        width,
    ));
}

/// Pad `text` with spaces to `cells` terminal cells.
fn pad_cells(text: &str, cells: usize) -> String {
    format!(
        "{text}{}",
        " ".repeat(cells.saturating_sub(cell_width(text)))
    )
}

/// §6.9: a BLOCK+ header (`›` — operator-invoked), BLOCK body rows.
fn command_output_lines(output: &CommandOutput, width: usize, out: &mut Vec<Line<'static>>) {
    // Like a tool's name field, a long command is never cut: it pushes the argument right.
    let field = COMMAND_FIELD.max(cell_width(&output.command) + 1);
    let right = if output.facts.is_empty() {
        Vec::new()
    } else {
        vec![Seg::new(palette::DIM, output.facts.clone())]
    };
    out.push(band(
        palette::BLOCK_PLUS,
        vec![
            Seg::new(palette::ATTN, format!("{} ", glyphs::OPERATOR)),
            Seg::new(palette::DIM, pad_cells(&output.command, field)),
            Seg::new(palette::INK, output.argument.clone()),
        ],
        right,
        width,
    ));
    for row in &output.body {
        let left = match row {
            CommandRow::Head(text) => {
                vec![Seg::new(palette::DIM, format!("{:HANG$}{text}", ""))]
            }
            CommandRow::Entry { key, text } => vec![
                Seg::new(
                    palette::INK,
                    format!(
                        "{:HANG$}{}",
                        "",
                        if cell_width(key) >= COMMAND_KEY {
                            format!("{key}  ")
                        } else {
                            pad_cells(key, COMMAND_KEY)
                        }
                    ),
                ),
                Seg::new(palette::DIM, text.clone()),
            ],
        };
        out.push(band(palette::BLOCK, left, Vec::new(), width));
    }
}

/// §6.5: `▪▪▪` at the text column, `phase · elapsed` dim, `request N` dim on the right.
pub fn turn_working_line(
    facts: &TurnWorking,
    width: usize,
    now_ms: u64,
    reduced_motion: bool,
) -> Line<'static> {
    let mut left = working_segments(palette::GROUND, now_ms, reduced_motion);
    let clock = facts
        .elapsed_ms
        .map(|ms| format!(" {} {}", glyphs::PENDING, live_elapsed(Some(ms))))
        .unwrap_or_default();
    left.push(Seg::new(
        palette::DIM,
        format!("  {}{clock}", facts.phase.label()),
    ));
    let right = facts
        .request
        .map(|n| vec![Seg::new(palette::DIM, format!("request {n}"))])
        .unwrap_or_default();
    ground(left, right, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transcript::Transcript;
    use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus, TurnEnd};

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
            Some(0),
        );
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "read".into(),
                    input: ToolInput::Json("src/edge.rs".into()),
                },
            },
            Some(100),
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
            Some(512),
        );
        t
    }

    #[test]
    fn a_tool_call_uses_three_bands() {
        let rendered = lines(&transcript_with_call(), 60, usize::MAX, false, 0, true);
        let text: Vec<String> = plain(&rendered)
            .iter()
            .map(|row| row.trim_end().to_string())
            .collect();
        assert_eq!(text[0], "  › why does compaction stall?");
        assert_eq!(text[1], "", "one blank row between events");
        assert_eq!(text[2], "  Checking.");
        assert!(text[4].starts_with("  ▸ read"));
        assert!(text[4].contains("✓ 3 lines"));
        assert_eq!(rendered[4].spans[0].style.bg, Some(palette::BLOCK_PLUS));
    }

    /// The `▪` cells of a rendered row, in order.
    fn working_cells(line: &Line<'static>) -> Vec<Option<Color>> {
        line.spans
            .iter()
            .filter(|span| span.content.contains(glyphs::WORKING))
            .map(|span| span.style.fg)
            .collect()
    }

    #[test]
    fn reduced_motion_freezes_the_chase() {
        let frozen = lines(&Transcript::new(), 60, usize::MAX, true, 0, true);
        // Every cell at full LIVE: a static `▪▪▪`.
        assert_eq!(working_cells(&frozen[0]), vec![Some(palette::LIVE); 3]);
        // Live, the same instant dims cells toward the ground.
        let live = lines(&Transcript::new(), 60, usize::MAX, true, 0, false);
        assert_ne!(working_cells(&live[0])[0], Some(palette::LIVE));
    }

    #[test]
    fn a_running_block_counts_its_elapsed_live() {
        let mut t = Transcript::new();
        t.apply(
            &AgentEvent::ToolStarted {
                call: ToolCall {
                    call_id: "c1".into(),
                    name: "shell".into(),
                    input: ToolInput::Json("cargo test".into()),
                },
            },
            Some(10_000),
        );
        let at = |now_ms| plain(&lines(&t, 76, usize::MAX, true, now_ms, true))[0].clone();
        assert!(at(10_000).contains("0.0s  ▪▪▪"), "{}", at(10_000));
        // Whole tenths, truncated: 4.29 s is still 4.2 s.
        assert!(at(14_290).contains("4.2s  ▪▪▪"), "{}", at(14_290));
        assert!(at(14_300).contains("4.3s  ▪▪▪"), "{}", at(14_300));
        // The block's own ▪▪▪ replaces the turn working row.
        assert_eq!(lines(&t, 76, usize::MAX, true, 0, true).len(), 1);
    }

    #[test]
    fn count_rows_matches_the_full_render() {
        let mut mixed = transcript_with_call();
        mixed.set_path_exists(|path| path.ends_with(".rs"));
        mixed.apply(
            &AgentEvent::ReasoningDelta {
                text: "weigh the two designs carefully".into(),
            },
            Some(1_000),
        );
        mixed.apply(
            &AgentEvent::TextDelta {
                text: "a long sentence in `crates/p1-context/src/edge.rs` that must wrap more than once at a narrow width".into(),
            },
            Some(2_000),
        );
        mixed.queue_steering("use the existing helper and keep the notice text");
        mixed.apply(&AgentEvent::InboxDelivered { count: 2 }, Some(2_100));
        mixed.apply(
            &AgentEvent::ContextReplaced {
                items_before: 214,
                items_after: 31,
                usage: None,
            },
            Some(2_200),
        );
        mixed.toggle_reasoning();
        mixed.worker_report(crate::transcript::WorkerReport {
            id: "w2".into(),
            route: "deepseek2/v4.1-flash".into(),
            end: crate::transcript::WorkerEnd::Done,
            elapsed: Some("2m10s".into()),
            cost_micro_usd: None,
            grants: vec!["read".into(), "edit".into()],
            line: "done · verified · cargo test -p p1-provider-http".into(),
        });
        mixed.command_output(crate::transcript::CommandOutput {
            command: "/help".into(),
            argument: String::new(),
            facts: "10 commands".into(),
            body: vec![
                crate::transcript::CommandRow::Head("COMMANDS".into()),
                crate::transcript::CommandRow::Entry {
                    key: "/model [REF]".into(),
                    text: "switch model".into(),
                },
            ],
        });
        mixed.apply(
            &AgentEvent::TurnFinished {
                end: TurnEnd::CommitFailed {
                    message: "No space left on device (os error 28)".into(),
                },
            },
            Some(3_000),
        );
        for t in [transcript_with_call(), mixed, Transcript::new()] {
            for width in [1usize, 7, 40, 120] {
                let built = lines(&t, width, usize::MAX, false, 0, true);
                assert_eq!(
                    count_rows(&t, width),
                    built.len(),
                    "count_rows disagrees with a full render at width {width}"
                );
            }
        }
    }

    #[test]
    fn prose_sanitizes_before_measuring_and_wrapping() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Prose {
            lines: vec!["a\u{1b}[31mb".into()],
        });
        for width in 10..=40 {
            let rows = lines(&t, width, usize::MAX, false, 0, true);
            assert_eq!(rows.len(), 1, "unexpected rows at width {width}");
            let row = plain(&rows).join("");
            assert!(
                !row.contains('['),
                "escape leaked at width {width}: {row:?}"
            );
            assert!(
                !row.contains("31m"),
                "escape leaked at width {width}: {row:?}"
            );
            assert_eq!(row.trim(), "ab", "unexpected text at width {width}");
            assert_eq!(count_rows(&t, width), rows.len());
        }
    }

    #[test]
    fn prose_sanitizes_osc_hyperlinks_before_wrapping() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Prose {
            lines: vec!["see \u{1b}]8;;https://x.dev/a b c\u{7}link\u{1b}]8;;\u{7} now".into()],
        });
        for width in 12..=40 {
            let rows = lines(&t, width, usize::MAX, false, 0, true);
            let rendered = plain(&rows);
            let words: Vec<&str> = rendered
                .iter()
                .flat_map(|row| row.split_whitespace())
                .collect();
            assert_eq!(
                words,
                ["see", "link", "now"],
                "unexpected words at width {width}"
            );
            for row in rendered {
                assert!(
                    !row.contains("x.dev"),
                    "OSC payload leaked at width {width}"
                );
                assert!(!row.contains("8;;"), "OSC payload leaked at width {width}");
                assert!(
                    !row.contains("b c"),
                    "wrapped OSC payload leaked at width {width}"
                );
            }
        }
    }

    #[test]
    fn sanitizing_preserves_operator_paragraph_breaks() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Operator {
            text: "a\nb".into(),
            steering: false,
        });
        let rows = lines(&t, 40, usize::MAX, false, 0, true);
        assert_eq!(
            plain(&rows)
                .iter()
                .map(|row| row.trim())
                .collect::<Vec<_>>(),
            ["› a", "b"]
        );
        assert_eq!(count_rows(&t, 40), rows.len());
    }

    #[test]
    fn operator_marker_skips_leading_blank_rows() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Operator {
            text: "\n\nhello".into(),
            steering: false,
        });
        let rows = lines(&t, 40, usize::MAX, false, 0, true);
        let text: Vec<String> = plain(&rows)
            .iter()
            .map(|row| row.trim_end().to_string())
            .collect();
        assert_eq!(text, ["", "", "  › hello"]);
        assert_eq!(count_rows(&t, 40), rows.len());
    }

    #[test]
    fn operator_marker_skips_leading_whitespace_row() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Operator {
            text: "  \nhi\nthere".into(),
            steering: false,
        });
        let rows = lines(&t, 40, usize::MAX, false, 0, true);
        let text: Vec<String> = plain(&rows)
            .iter()
            .map(|row| row.trim_end().to_string())
            .collect();
        assert_eq!(text, ["", "  › hi", "    there"]);
    }

    #[test]
    fn operator_marker_keeps_paragraph_breaks_after_the_first_line() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Operator {
            text: "hello\n\nworld".into(),
            steering: false,
        });
        let rows = lines(&t, 40, usize::MAX, false, 0, true);
        let text: Vec<String> = plain(&rows)
            .iter()
            .map(|row| row.trim_end().to_string())
            .collect();
        assert_eq!(text, ["  › hello", "", "    world"]);
    }

    #[test]
    fn steering_tag_uses_the_first_nonblank_operator_row() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Operator {
            text: "\nnow".into(),
            steering: true,
        });
        let rows = lines(&t, 40, usize::MAX, false, 0, true);
        let text = plain(&rows);
        assert!(text[0].trim().is_empty());
        assert!(!text[0].contains(STEERING));
        assert!(text[1].starts_with("  › now"));
        assert!(text[1].trim_end().ends_with(STEERING));
    }

    #[test]
    fn all_blank_operator_input_keeps_the_marker_on_row_zero() {
        for text in ["", "\n"] {
            let mut t = Transcript::new();
            t.blocks.push(Block::Operator {
                text: text.into(),
                steering: true,
            });
            let rows = lines(&t, 40, usize::MAX, false, 0, true);
            let rendered = plain(&rows);
            assert!(rendered[0].contains(glyphs::OPERATOR));
            assert!(rendered[0].trim_end().ends_with(STEERING));
        }
    }

    #[test]
    fn operator_marker_invariants_hold_across_widths_and_steering() {
        for text in [
            "\n\nhello",
            "  \nhi\nthere",
            "hello\n\nworld",
            "\nalpha beta gamma delta epsilon zeta",
            "a\nb",
            "why does compaction stall?",
            " \n \nx",
        ] {
            for width in 12..=40 {
                for steering in [false, true] {
                    let mut t = Transcript::new();
                    t.blocks.push(Block::Operator {
                        text: text.into(),
                        steering,
                    });
                    let rows = lines(&t, width, usize::MAX, false, 0, true);
                    let expected =
                        wrap_paragraphs(&sanitize_text(text), operator_measure(width, steering))
                            .iter()
                            .position(|part| !part.trim().is_empty())
                            .unwrap_or(0);
                    let marked: Vec<usize> = rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| {
                            row.spans.iter().any(|span| {
                                span.style.fg == Some(palette::ATTN)
                                    && span.content.starts_with(glyphs::OPERATOR)
                            })
                        })
                        .map(|(n, _)| n)
                        .collect();
                    assert_eq!(
                        marked,
                        [expected],
                        "text {text:?}, width {width}, steering {steering}"
                    );
                    let steering_rows = rows
                        .iter()
                        .enumerate()
                        .filter(|(_, row)| {
                            row.spans.iter().any(|span| span.content.contains(STEERING))
                        })
                        .map(|(n, _)| n)
                        .collect::<Vec<_>>();
                    assert_eq!(
                        steering_rows,
                        if steering { vec![expected] } else { vec![] },
                        "text {text:?}, width {width}"
                    );
                    assert_eq!(count_rows(&t, width), rows.len());
                }
            }
        }
    }

    #[test]
    fn reasoning_and_operator_measure_sanitized_text_exactly() {
        for width in 10..=40 {
            let mut reasoning = Transcript::new();
            reasoning.blocks.push(Block::Reasoning {
                lines: vec!["run \u{1b}[2Jnow".into()],
                expanded: true,
                elapsed_ms: Some(1),
                started_ms: Some(0),
            });
            let reasoning_rows = lines(&reasoning, width, usize::MAX, false, 0, true);
            let reasoning_rendered = plain(&reasoning_rows);
            let reasoning_text: Vec<String> = reasoning_rendered[1..]
                .iter()
                .flat_map(|row| row.split_whitespace().map(str::to_string))
                .collect();
            assert_eq!(
                reasoning_text,
                ["run", "now"],
                "unexpected reasoning text at width {width}"
            );
            assert_eq!(count_rows(&reasoning, width), reasoning_rows.len());

            let mut operator = Transcript::new();
            operator.blocks.push(Block::Operator {
                text: "run \u{1b}[2Jnow".into(),
                steering: false,
            });
            let operator_rows = lines(&operator, width, usize::MAX, false, 0, true);
            let operator_rendered = plain(&operator_rows);
            let operator_text: Vec<String> = operator_rendered
                .iter()
                .flat_map(|row| row.split_whitespace().map(str::to_string))
                .filter(|word| !word.starts_with(glyphs::OPERATOR))
                .collect();
            assert_eq!(
                operator_text,
                ["run", "now"],
                "unexpected operator text at width {width}"
            );
            assert_eq!(count_rows(&operator, width), operator_rows.len());
        }
    }

    #[test]
    fn escaped_transcript_text_is_not_mutated() {
        let mut t = Transcript::new();
        t.blocks.push(Block::Prose {
            lines: vec!["a\u{1b}[31mb".into()],
        });
        t.blocks.push(Block::Operator {
            text: "run \u{1b}[2Jnow".into(),
            steering: false,
        });
        let before = t.blocks.clone();
        let _ = lines(&t, 40, usize::MAX, false, 0, true);
        assert_eq!(t.blocks, before);
    }

    #[test]
    fn count_rows_matches_the_full_render_with_terminal_controls() {
        let mut t = Transcript::new();
        t.blocks.extend([
            Block::Prose {
                lines: vec!["csi \u{1b}[31mplain unterminated \u{1b}[31".into()],
            },
            Block::Reasoning {
                lines: vec![
                    "osc \u{1b}]8;;https://x\u{7}link\u{1b}]8;;\u{7} st \u{1b}]0;title\u{1b}\\body"
                        .into(),
                ],
                expanded: true,
                elapsed_ms: Some(1),
                started_ms: Some(0),
            },
            Block::Operator {
                text: "csi \u{1b}[2Jplain".into(),
                steering: false,
            },
            Block::Meta {
                text: "· osc \u{1b}]0;title\u{7}plain".into(),
            },
        ]);
        for width in [1usize, 7, 40, 120] {
            let built = lines(&t, width, usize::MAX, false, 0, true);
            let rendered = plain(&built);
            assert!(
                rendered.iter().all(|row| !row.contains('\u{1b}')),
                "terminal control leaked at width {width}"
            );
            assert_eq!(
                count_rows(&t, width),
                built.len(),
                "count disagreement at width {width}"
            );
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
        let full = lines(&t, width, usize::MAX, false, 0, true);
        assert_eq!(full.len(), 9_999);
        let tail = lines(&t, width, max_rows, true, 0, true);
        // The working row and its separating blank row are appended after
        // windowing; the transcript rows never exceed the budget.
        assert!(
            tail.len() <= max_rows + 2,
            "tail is {} rows for a {max_rows}-row budget",
            tail.len()
        );
        assert!(plain(&tail[tail.len() - 1..])[0].contains("▪▪▪  waiting"));
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
        let full = lines(&t, 40, usize::MAX, false, 0, true);
        let tail = lines(&t, 40, 20, false, 0, true);
        assert_eq!(tail.len(), 20);
        assert_eq!(plain(&tail), plain(&full[full.len() - 20..]));
    }
}
