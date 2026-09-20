//! Screen composition: transcript + right pane + composer into a `Buffer`.
//! Panes are separated by a one-step background shift (GROUND vs BLOCK) and by
//! whitespace — there is no border to draw (SPEC §3). The composition is one
//! pure function so every §4 screen snapshots against `TestBackend`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::palette;
use crate::state::{PANE_FLOOR_COLS, PaneMode, Promotion, Screen};

use super::{
    composer, diff, home, ledger, output, permission, picker, status, transcript, workers,
};

/// The pane's padding: the content grid is the pane width minus 4 on each
/// side (SPEC §5: LEDGER grid 32 in a 40-ch pane; recorded as a refinement —
/// the mock-up does not pin the split, 4/4 keeps the rule symmetric).
const PANE_PAD: usize = 4;

/// Draw the whole screen. `now_ms` drives the working indicator; callers pass
/// a fake clock in tests.
pub fn draw(screen: &mut Screen, area: Rect, buf: &mut Buffer, now_ms: u64) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    fill_bg(area, buf, palette::GROUND);

    // A pending approval owns the screen: full width, pane hidden (SPEC §4.4).
    // The decision footer is pinned to the bottom rows — a tall diff must
    // never push the decision keys off-screen.
    if let Some(approval) = &screen.approval {
        let (lines, head_rows, foot_rows) = match approval {
            crate::state::Approval::Diff(view) => (diff::lines(view, area.width as usize), 3, 2),
            crate::state::Approval::Permission(view) => {
                let foot = if view.grantable { 1 } else { 3 };
                (permission::lines(view, area.width as usize), 2, foot)
            }
        };
        let foot_area = Rect {
            y: area.y + area.height.saturating_sub(foot_rows as u16),
            height: foot_rows as u16,
            ..area
        };
        draw_lines(&lines[..head_rows.min(lines.len())], area, buf);
        draw_lines(
            &lines[lines.len().saturating_sub(foot_rows)..],
            foot_area,
            buf,
        );
        // The body: clipped between header and footer, newest rows win when
        // it overflows (the change itself matters more than its context).
        let body = &lines[head_rows.min(lines.len())..lines.len().saturating_sub(foot_rows)];
        let body_area = Rect {
            y: area.y + head_rows as u16,
            height: area
                .height
                .saturating_sub(head_rows as u16 + foot_rows as u16),
            ..area
        };
        let skip = body.len().saturating_sub(body_area.height as usize);
        draw_lines(&body[skip..], body_area, buf);
        return;
    }

    let focus = screen.focus;
    let pane_cols = if focus {
        None
    } else if screen.ledger_overlay {
        Some(40usize.min(area.width as usize))
    } else {
        screen.pane_width.columns(area.width as usize)
    };

    // Composer: hidden in focus mode while empty.
    let mut composer_lines = if screen.composer.visible(focus) {
        composer::lines(
            &screen.composer,
            &screen.queued,
            screen.working.is_some(),
            area.width as usize,
        )
    } else {
        Vec::new()
    };
    if (area.width as usize) < PANE_FLOOR_COLS && pane_cols.is_none() && !screen.ledger_overlay {
        // §6: the one bottom-of-screen line, only when the ledger is gone.
        let context = screen
            .spend
            .input
            .map(super::tokens)
            .unwrap_or_else(|| super::UNKNOWN.into());
        composer_lines.push(composer::floor_line(
            &screen.env,
            &screen.route,
            &context,
            area.width as usize,
        ));
    }
    // The composer never eats the screen: at most a third, keeping the
    // newest input rows and always the hint line (a paste of 2 KB must not
    // push the transcript area to zero — that panicked the buffer).
    let cap = ((area.height as usize) / 3)
        .max(2)
        .min(area.height as usize);
    if composer_lines.len() > cap {
        let hints = composer_lines.pop();
        composer_lines.truncate(cap - 1);
        composer_lines.extend(hints);
    }
    let composer_height = composer_lines.len() as u16;

    let transcript_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width - pane_cols.unwrap_or(0) as u16,
        height: area.height.saturating_sub(composer_height),
    };

    // The transcript pins to the bottom: the newest rows are always visible.
    let width = transcript_area.width as usize;
    let fits = transcript_area.height as usize;
    // The full transcript height keeps the scroll math in absolute row
    // numbers (state.rs scroll_by) and locates a scrolled window from the end.
    let total_rows = transcript::count_rows(&screen.transcript, width);
    // Build only what the viewport can show. The live tail is `fits` rows;
    // scrolled back to absolute row `s`, every row from `s` to the end is the
    // window's material. (This is the correct form of `fits + scroll`; the
    // absolute scroll_top is measured from the top, not the bottom.)
    let max_rows = match screen.scroll_top {
        Some(s) => total_rows.saturating_sub(s),
        None => fits,
    };
    let working_label = screen.working.as_ref().map(|w| w.label.as_str());
    let mut body = transcript::lines(
        &screen.transcript,
        width,
        max_rows,
        working_label,
        now_ms,
        screen.reduced_motion,
    );
    let tail_rows = total_rows.min(max_rows);
    // A docked overlay reserves rows above the composer; it never floats.
    if let Some(p) = &screen.picker {
        body.push(Line::default());
        body.extend(picker::lines(p, width));
    }
    if let Some(groups) = &screen.status {
        body.push(Line::default());
        body.extend(status::lines(groups, width));
    }
    // Record the rendered shape for the scroll math (state.rs scroll_by). The
    // body is a suffix of a full render: `total_rows` transcript rows plus the
    // working/overlay rows appended after the tail.
    let appended = body.len().saturating_sub(tail_rows);
    screen.last_rendered = (total_rows + appended, fits);
    match screen.scroll_top {
        // The live tail is bottom-anchored: the working line and overlays sit
        // at the bottom, exactly as a full render would draw them.
        None => draw_lines_bottom(&body, transcript_area, buf, None),
        // Scrolled: the built tail starts at the window's top row, so the
        // window is the first `fits` rows of the body.
        Some(s) => {
            let offset = total_rows - tail_rows;
            let start = s.saturating_sub(offset).min(body.len());
            let end = (start + fits).min(body.len());
            draw_lines(&body[start..end], transcript_area, buf);
        }
    }
    // Welcome metadata remains at the top. Only unused space may animate;
    // any conversation, working state or overlay takes priority immediately.
    // (Visual-overhaul session's static home, preserved over my tail-render
    // change: the draw path above replaced their draw_lines_bottom call.)
    if !screen.focus
        && screen.working.is_none()
        && screen.picker.is_none()
        && screen.status.is_none()
        && screen
            .transcript
            .blocks
            .iter()
            .all(|block| matches!(block, crate::transcript::Block::Info { .. }))
    {
        let used = (body.len() as u16).min(transcript_area.height);
        home::draw(
            Rect {
                y: transcript_area.y + used,
                height: transcript_area.height - used,
                ..transcript_area
            },
            buf,
        );
    }

    let composer_area = Rect {
        x: area.x,
        y: area.y + transcript_area.height,
        width: area.width - pane_cols.unwrap_or(0) as u16,
        height: composer_height,
    };
    draw_lines(&composer_lines, composer_area, buf);

    if let Some(cols) = pane_cols {
        draw_pane(screen, area, buf, cols);
    }
}

/// The right pane: BLOCK background, content on its inner grid, PEEK banner
/// on BLOCK+ when promoted (SPEC §5).
fn draw_pane(screen: &Screen, area: Rect, buf: &mut Buffer, cols: usize) {
    let pane = Rect {
        x: area.x + area.width - cols as u16,
        y: area.y,
        width: cols as u16,
        height: area.height,
    };
    fill_bg(pane, buf, palette::BLOCK);
    let grid = cols.saturating_sub(2 * PANE_PAD);
    let content = Rect {
        x: pane.x + PANE_PAD as u16,
        y: pane.y,
        width: grid as u16,
        height: pane.height,
    };
    let lines = match screen.pane_mode {
        PaneMode::Ledger => ledger::lines(&screen.ledger()),
        PaneMode::Output => match &screen.output {
            Some(view) => output::lines(view, grid),
            None => vec![],
        },
        PaneMode::Workers => workers::lines(&screen.workers, grid),
        // DIFF lands with its pane mode; the pane still earns its place.
        _ => vec![],
    };
    draw_lines(&lines, content, buf);
    // PEEK: a two-line banner on BLOCK+ drawn OVER the ledger's top rows —
    // the ledger underneath does not move (SPEC §5). Only over the ledger:
    // a banner must never stamp over an open OUTPUT or WORKERS pane.
    if let Promotion::Peek { lines: peek, .. } = &screen.promotion
        && screen.pane_mode == PaneMode::Ledger
    {
        for (n, line) in peek.iter().enumerate() {
            let y = content.y + n as u16;
            if y >= content.bottom() {
                break;
            }
            let banner = super::fill(
                Line::styled(ellipsize(line, grid), Style::new().fg(palette::INK)),
                grid,
                palette::BLOCK_PLUS,
            );
            draw_lines(
                &[banner],
                Rect {
                    y,
                    height: 1,
                    ..content
                },
                buf,
            );
        }
    }
}

fn fill_bg(area: Rect, buf: &mut Buffer, bg: ratatui::style::Color) {
    let style = Style::new().bg(bg);
    for y in area.y..area.bottom() {
        for x in area.x..area.right() {
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

/// Draw the transcript: top-aligned while it fits (the conversation grows
/// downward from the top), scrolling only once it overfills the area.
fn draw_lines_bottom(
    lines: &[Line<'static>],
    area: Rect,
    buf: &mut Buffer,
    scroll_top: Option<usize>,
) {
    let fits = area.height as usize;
    let start = scroll_top
        .unwrap_or_else(|| lines.len().saturating_sub(fits))
        .min(lines.len().saturating_sub(fits));
    let end = (start + fits).min(lines.len());
    draw_lines(&lines[start..end], area, buf);
}

/// Cut a banner line at the grid edge with `…` — a hard cut mid-word reads as
/// a rendering bug, an ellipsis as a folded fact.
fn ellipsize(text: &str, width: usize) -> String {
    if text.chars().count() <= width {
        return text.to_string();
    }
    let mut out: String = text.chars().take(width.saturating_sub(1)).collect();
    out.push('\u{2026}');
    out
}
