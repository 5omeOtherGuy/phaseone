//! Screen composition (handoff §4, §16): every region of the screen placed on the
//! `geometry::layout` rectangles and drawn by its SLAB renderer. Surfaces separate the regions
//! — GROUND, BLOCK, BLOCK+ — there is no border to draw. The composition is one pure function
//! of the `Screen` and the clock, so every §16 screen snapshots against `TestBackend`.
//!
//! Transcript area, top to bottom: the attach band (§9.5), the conversation (home prelude,
//! transcript, an inline approval as the running element), then the docked stack directly
//! above the composer gap — scroll mark, menu, queue rows (§6.10, §8.2, §8.3).

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::{Color, Style};
use ratatui::text::Line;

use crate::band::Seg;
use crate::face::{CallFace, FaceBody, ResultFace, TargetKind};
use crate::palette;
use crate::render::diff::DiffRow;
use crate::state::{Approval, PaneMode, PaneWidth, Promotion, Screen};
use crate::transcript::{Block, RowStatus, ToolRow, Transcript};

use super::composer::{self, Mode};
use super::{
    block, diff, home, ledger, output, pane, permission, picker, review, scroll, transcript,
    workers,
};

/// The `^L` overlay's widest (§9.6: `P = min(38, W − 4)`).
const OVERLAY_COLS: u16 = 38;
/// A pane at least this wide shows WORKERS in the wide 4-row form (grid 48, §4.3).
const WIDE_PANE: u16 = 56;

/// Draw the whole screen into `area` of `buf` at `now_ms` (the clock of every live elapsed and
/// of the `▪▪▪` pulse; tests pass fake time). Records in `screen.cursor` the cell where the
/// terminal's hardware cursor goes — the composer's text cell — for the driver to place.
pub fn draw(screen: &mut Screen, area: Rect, buf: &mut Buffer, now_ms: u64) {
    screen.cursor = compose(screen, area, buf, now_ms);
}

fn compose(screen: &mut Screen, area: Rect, buf: &mut Buffer, now_ms: u64) -> Option<(u16, u16)> {
    if area.width == 0 || area.height == 0 {
        return None;
    }
    screen.last_width = area.width;
    screen.hits = Default::default();
    fill(area, buf, palette::GROUND);
    let (w, h) = (area.width, area.height);
    // §8.5: explicit `/focus` wins; otherwise the driver's `focus`, and always at 12 rows or
    // fewer.
    let focus = screen.focus_explicit.unwrap_or(screen.focus || h <= 12);

    // The full diff review owns the screen from the top row to the statusline gap (§7.5):
    // on `^D`, or by itself when the diff is taller than the transcript area. That region is
    // the transcript area of a screen with no pane and no composer, at full width.
    let unobstructed = crate::geometry::layout(w, h, PaneWidth::Off, true, 0);
    if let Some(Approval::Diff(view)) = &screen.approval {
        let normal = crate::geometry::layout(w, h, screen.pane_width, focus, 2);
        if screen.review.open
            || review::opens_itself(view.rows.len(), normal.transcript.height as usize)
        {
            draw_statusline(screen, area, unobstructed.statusline, buf);
            let files = [review::ReviewFile::new(view.clone())];
            let decision = review::Decision::for_call(view.grantable, files.len());
            let rows = unobstructed.transcript.height as usize;
            let full = Rect {
                width: w.saturating_sub(4),
                ..unobstructed.transcript
            };
            screen.review.body_rows = review::body_rows(&decision, rows);
            let lines = review::lines(&files, &screen.review, &decision, full.width as usize, rows);
            draw_lines(&lines, offset(area, full), buf);
            screen.hits = crate::state::Hits {
                transcript: unobstructed.transcript,
                pane_mode: PaneMode::Ledger,
                review: true,
                ..Default::default()
            };
            return None;
        }
    }

    // The composer first: its height moves the transcript's bottom edge (§8.1 — it grows
    // upward, capped at a third of the screen; one row at H ≤ 12).
    let column = crate::geometry::layout(w, h, screen.pane_width, focus, 2)
        .transcript
        .width as usize;
    let mode = if screen.approval.is_some() {
        Mode::Decision
    } else if let Some(worker) = &screen.attached {
        Mode::Attached(&worker.id)
    } else if screen.working.is_some() {
        Mode::Working
    } else {
        Mode::Idle
    };
    let max_rows = if h <= 12 { 1 } else { (h as usize / 3).max(2) };
    let frame = screen
        .composer
        .visible(focus)
        .then(|| composer::render(&screen.composer, mode, column, max_rows));
    let composer_rows = frame.as_ref().map_or(0, |f| f.lines.len() as u16);
    let geometry = crate::geometry::layout(w, h, screen.pane_width, focus, composer_rows);

    screen.hits.transcript = offset(area, geometry.transcript);
    draw_statusline(screen, area, geometry.statusline, buf);
    draw_transcript_area(
        screen,
        offset(area, geometry.transcript),
        buf,
        focus,
        now_ms,
    );

    let composer_area = offset(area, geometry.composer);
    let mut cursor = None;
    if let Some(frame) = frame {
        draw_lines(&frame.lines, composer_area, buf);
        cursor = frame
            .cursor
            .map(|(x, y)| (composer_area.x + x, composer_area.y + y));
    }

    if geometry.pane.width > 0 {
        let pane = offset(area, geometry.pane);
        screen.hits.pane = pane;
        screen.hits.pane_mode = draw_pane(screen, pane, buf, now_ms);
    } else if screen.ledger_overlay && !focus {
        // §9.6: under 100 columns `^L` draws the pane over the transcript's right side, from
        // the first row to the composer's last; nothing under it reflows.
        let cols = OVERLAY_COLS.min(w.saturating_sub(4));
        let top = geometry.transcript.y;
        let bottom = if composer_rows > 0 {
            geometry.composer.bottom()
        } else {
            geometry.transcript.bottom()
        };
        let overlay = Rect::new(w - 2 - cols, top, cols, bottom.saturating_sub(top));
        let overlay = offset(area, overlay);
        screen.hits.pane = overlay;
        screen.hits.pane_mode = draw_pane(screen, overlay, buf, now_ms);
    }
    cursor
}

/// The statusline (§10); attached to a worker, the chip names the worker's model.
fn draw_statusline(screen: &Screen, area: Rect, at: Rect, buf: &mut Buffer) {
    let line = match &screen.attached {
        Some(worker) => crate::render::statusbar::StatusBar {
            model: Some(worker.route.clone()),
            effort: None,
            ..screen.statusbar.clone()
        }
        .line(at.width as usize),
        None => screen.statusbar.line(at.width as usize),
    };
    draw_lines(&[line], offset(area, at), buf);
}

/// The transcript area: attach band, conversation (tail-pinned, or pinned at `scroll_top`
/// with the scroll mark), the docked stack, and the monogram in unused rows at home.
fn draw_transcript_area(
    screen: &mut Screen,
    area: Rect,
    buf: &mut Buffer,
    focus: bool,
    now_ms: u64,
) {
    let width = area.width as usize;
    let rows = area.height as usize;
    let attach: Vec<Line<'static>> = screen
        .attached
        .iter()
        .map(|worker| workers::attach_band(&worker.id, &worker.route, worker.state, width))
        .collect();
    let docked = screen
        .picker
        .as_ref()
        .map(|menu| picker::lines(menu, width))
        .unwrap_or_default();
    let queued = scroll::queue_rows(&screen.queued, width);

    let transcript = match &screen.attached {
        Some(worker) => &worker.transcript,
        None => &screen.transcript,
    };
    let conversation = Conversation {
        prelude: match (&screen.attached, &screen.home) {
            (None, Some(home)) => home.lines(width),
            _ => Vec::new(),
        },
        transcript,
        blocks_rows: transcript::count_rows(transcript, width),
        // The working row stands for a live turn with nothing running; an approval waiting on
        // the operator is the running element instead (§6.5, §7.5).
        turn_live: match &screen.attached {
            Some(worker) => worker.transcript.turn_working(now_ms).is_some(),
            None => screen.working.is_some() && screen.approval.is_none(),
        },
        approval: match &screen.attached {
            Some(_) => Vec::new(),
            None => approval_rows(screen, width, now_ms),
        },
        width,
    };
    // Rows the conversation may use: everything but the attach band, the menu and the queue.
    // The scroll mark takes one of them while scrolled back (`Screen::scroll_mark` counts so).
    let fits = rows.saturating_sub(attach.len() + docked.len() + queued.len());
    let total = conversation.total();
    screen.last_rendered = (total, fits);
    let shown = match screen.scroll_top {
        None => fits,
        Some(_) => fits.saturating_sub(1),
    };
    let (view, mark) = match screen.scroll_top {
        None => {
            let from = total.saturating_sub(shown);
            (
                conversation.rows_from(from, now_ms, screen.reduced_motion),
                None,
            )
        }
        Some(top) => {
            let top = top.min(total.saturating_sub(shown));
            let mut view = conversation.rows_from(top, now_ms, screen.reduced_motion);
            view.truncate(shown);
            let mark = screen.scroll_mark().map(|mark| mark.line(width));
            (view, mark)
        }
    };
    let home_screen = !focus
        && screen.attached.is_none()
        && screen.working.is_none()
        && screen.picker.is_none()
        && screen.approval.is_none()
        && conversation
            .transcript
            .blocks
            .iter()
            .all(|block| matches!(block, Block::Info { .. }));

    draw_lines(&attach, area, buf);
    let body_top = area.y + attach.len() as u16;
    draw_lines(
        &view,
        Rect {
            y: body_top,
            height: area.bottom().saturating_sub(body_top),
            ..area
        },
        buf,
    );
    let stack: Vec<Line<'static>> = mark.into_iter().chain(docked).chain(queued).collect();
    // A stack taller than the room left keeps its bottom rows: the queue and the menu's keys.
    let room = rows.saturating_sub(attach.len());
    let stack = &stack[stack.len().saturating_sub(room)..];
    let stack_top = area.bottom() - stack.len() as u16;
    draw_lines(
        stack,
        Rect {
            y: stack_top,
            height: stack.len() as u16,
            ..area
        },
        buf,
    );
    if home_screen {
        // §6.11: the monogram only in rows nothing else uses; `home::draw` decides whether the
        // free area is big enough.
        let free_top = body_top + view.len() as u16;
        let free = shown.saturating_sub(view.len()) as u16;
        home::draw(
            Rect {
                y: free_top,
                height: free,
                ..area
            },
            buf,
        );
    }
}

/// The conversation as rows: the home prelude, the transcript with its working row, and an
/// inline approval — one blank row between parts. Only the rows from the window's first row
/// on are ever built (a 5 000-block transcript builds one screenful).
struct Conversation<'a> {
    prelude: Vec<Line<'static>>,
    transcript: &'a Transcript,
    /// `transcript::count_rows`, measured once per frame (it wraps every block).
    blocks_rows: usize,
    turn_live: bool,
    approval: Vec<Line<'static>>,
    width: usize,
}

impl Conversation<'_> {
    /// The transcript part's height: its blocks, then the working row (a blank row above it
    /// when blocks precede it), exactly as `transcript::lines` appends it.
    fn transcript_rows(&self) -> usize {
        let working = self.turn_live && !self.transcript.call_running();
        self.blocks_rows
            + if working {
                1 + usize::from(!self.transcript.blocks.is_empty())
            } else {
                0
            }
    }

    /// The blank rows after the prelude and after the transcript part.
    fn separators(&self) -> (usize, usize) {
        let (prelude, transcript, approval) = (
            self.prelude.len(),
            self.transcript_rows(),
            self.approval.len(),
        );
        (
            usize::from(prelude > 0 && (transcript > 0 || approval > 0)),
            usize::from(transcript > 0 && approval > 0),
        )
    }

    fn total(&self) -> usize {
        let (after_prelude, after_transcript) = self.separators();
        self.prelude.len()
            + after_prelude
            + self.transcript_rows()
            + after_transcript
            + self.approval.len()
    }

    /// Every row from absolute row `from` to the end.
    fn rows_from(&self, from: usize, now_ms: u64, reduced_motion: bool) -> Vec<Line<'static>> {
        let (after_prelude, after_transcript) = self.separators();
        let transcript_top = self.prelude.len() + after_prelude;
        let count = self.blocks_rows;
        let skip = from.saturating_sub(transcript_top).min(count);
        let mut rows = Vec::new();
        if from < transcript_top {
            rows.extend(self.prelude.iter().cloned());
            rows.extend((0..after_prelude).map(|_| Line::default()));
        }
        rows.extend(transcript::lines(
            self.transcript,
            self.width,
            count - skip,
            self.turn_live,
            now_ms,
            reduced_motion,
        ));
        rows.extend((0..after_transcript).map(|_| Line::default()));
        rows.extend(self.approval.iter().cloned());
        // `rows` is a suffix of the whole conversation that starts at or before `from`.
        let start = self.total() - rows.len();
        rows.split_off(from.saturating_sub(start).min(rows.len()))
    }
}

/// An approval on screen, inline: the call's Block with `!`, the facts or the diff, and the
/// decision band (§7.5). The call has not started yet (authorization comes first), so the
/// Block is built from the approval itself.
fn approval_rows(screen: &Screen, width: usize, now_ms: u64) -> Vec<Line<'static>> {
    let Some(approval) = &screen.approval else {
        return Vec::new();
    };
    let pending = Some((1, 1 + screen.approvals_waiting));
    let row = |name: &str, target: &str, kind, result_face| ToolRow {
        name: name.to_string(),
        summary: target.to_string(),
        status: RowStatus::AwaitingApproval,
        output: None,
        line_count: 0,
        fold: None,
        elapsed_ms: None,
        call_id: String::new(),
        call: None,
        face: CallFace {
            target: target.to_string(),
            kind,
        },
        result_face,
        input_preview: None,
    };
    let (row, inline) = match approval {
        Approval::Diff(view) => {
            let count = |add: bool| {
                view.rows
                    .iter()
                    .filter(|r| {
                        matches!(
                            (r, add),
                            (DiffRow::Add { .. }, true) | (DiffRow::Del { .. }, false)
                        )
                    })
                    .count()
            };
            let (at, of) = view.position;
            let outcome = format!(
                "+{} \u{2212}{} · {at} of {of} files",
                count(true),
                count(false)
            );
            let face = ResultFace {
                outcome: Some(outcome),
                body: FaceBody::Diff(view.rows.clone()),
                meta: None,
                target: None,
            };
            (
                row(&view.tool, &view.file, TargetKind::Path, Some(face)),
                diff::inline_approval(view, pending),
            )
        }
        Approval::Permission(view) => (
            row(
                &screen.approval_tool,
                &view.command,
                TargetKind::Command,
                None,
            ),
            permission::inline_approval(view, pending),
        ),
    };
    block::lines_with_approval(
        &row,
        width,
        false,
        now_ms,
        screen.reduced_motion,
        Some(&inline),
    )
}

/// The pane (§9.1): BLOCK, a padding row, the current mode's rows, the mode strip as the last
/// row, and a peek over the top rows.
fn draw_pane(screen: &Screen, rect: Rect, buf: &mut Buffer, now_ms: u64) -> PaneMode {
    fill(rect, buf, palette::BLOCK);
    if rect.height < 2 {
        return PaneMode::Ledger;
    }
    let width = rect.width as usize;
    let inner = rect.height as usize - 2;
    let available = screen.available_modes();
    // A mode with nothing to show is never shown; LEDGER always has something.
    let mode = if available.contains(&screen.pane_mode) {
        screen.pane_mode
    } else {
        PaneMode::Ledger
    };
    let lines = match (mode, &screen.output) {
        (PaneMode::Output, Some(view)) => {
            let body = inner.saturating_sub(OUTPUT_HEAD_ROWS);
            output::render(&view.pane(output_source(screen, view), body), width)
        }
        (PaneMode::Workers, _) => workers::render_with(
            &screen.workers,
            width,
            rect.width < WIDE_PANE,
            screen.stop_pending.as_deref(),
        ),
        _ => ledger::render(&screen.ledger(), width, Some(inner)),
    };
    let content = Rect {
        y: rect.y + 1,
        height: inner as u16,
        ..rect
    };
    draw_lines(&lines, content, buf);
    let strip = pane::mode_strip(width, &available, mode, screen.pinned);
    draw_lines(
        &[strip],
        Rect {
            y: rect.bottom() - 1,
            height: 1,
            ..rect
        },
        buf,
    );
    // A peek never covers a pinned pane or shows while a decision is on screen (§9.1, §7.5).
    if let Promotion::Peek { lines, until_ms } = &screen.promotion
        && !screen.pinned
        && screen.approval.is_none()
        && now_ms < *until_ms
    {
        let seconds = (until_ms - now_ms).div_ceil(1_000);
        draw_lines(
            &pane::peek(width, &lines[0], &lines[1], Some(seconds)),
            content,
            buf,
        );
    }
    mode
}

/// OUTPUT's header, source, range and blank rows above the numbered body (§9.3).
const OUTPUT_HEAD_ROWS: usize = 4;

/// OUTPUT's source row: the call that registered the handle, as its Block states it —
/// `tool · target · <glyph> <outcome>` (the outcome's glyph in its hue).
fn output_source(screen: &Screen, view: &output::OutputView) -> Vec<Seg> {
    let call = screen.transcript.blocks.iter().rev().find_map(|b| match b {
        Block::Call(row) if row.fold.as_ref() == Some(&view.id) => Some(row),
        _ => None,
    });
    let Some(row) = call else {
        return Vec::new();
    };
    let mut source = vec![Seg::new(
        palette::DIM,
        format!("{} · {} · ", row.name, row.face.target),
    )];
    let (glyph, hue) = match row.status {
        RowStatus::Settled(p1_contracts::ToolStatus::Ok) => (crate::glyphs::DONE, palette::OK),
        RowStatus::Settled(
            p1_contracts::ToolStatus::Cancelled | p1_contracts::ToolStatus::Unknown,
        ) => (crate::glyphs::PENDING, palette::FAINT),
        RowStatus::Settled(_) => (crate::glyphs::FAILED, palette::FAIL),
        RowStatus::Running | RowStatus::AwaitingApproval => (crate::glyphs::TOOL, palette::LIVE),
    };
    source.push(Seg::new(hue, glyph.to_string()));
    if let Some(outcome) = row.result_face.as_ref().and_then(|f| f.outcome.as_ref()) {
        source.push(Seg::new(palette::DIM, format!(" {outcome}")));
    }
    source
}

/// `inner` placed inside `area` (layout rectangles are relative to the frame).
fn offset(area: Rect, inner: Rect) -> Rect {
    Rect {
        x: area.x + inner.x,
        y: area.y + inner.y,
        ..inner
    }
    .intersection(area)
}

fn fill(area: Rect, buf: &mut Buffer, bg: Color) {
    let style = Style::new().bg(bg);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
            buf[(x, y)].reset();
            buf[(x, y)].set_style(style);
        }
    }
}

/// Draw lines top-down, clipping at the area bottom.
fn draw_lines(lines: &[Line<'static>], area: Rect, buf: &mut Buffer) {
    for (n, line) in lines.iter().enumerate() {
        let y = area.y + n as u16;
        if y >= area.bottom() {
            break;
        }
        buf.set_line(area.x, y, line, area.width);
    }
}
