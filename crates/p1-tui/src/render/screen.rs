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

use super::{composer, diff, ledger, permission, picker, status, transcript};

/// The pane's padding: the content grid is the pane width minus 4 on each
/// side (SPEC §5: LEDGER grid 32 in a 40-ch pane; recorded as a refinement —
/// the mock-up does not pin the split, 4/4 keeps the rule symmetric).
const PANE_PAD: usize = 4;

/// Draw the whole screen. `now_ms` drives the working indicator; callers pass
/// a fake clock in tests.
pub fn draw(screen: &Screen, area: Rect, buf: &mut Buffer, now_ms: u64) {
    fill_bg(area, buf, palette::GROUND);

    // A pending approval owns the screen: full width, pane hidden (SPEC §4.4).
    if let Some(approval) = &screen.approval {
        let lines = match approval {
            crate::state::Approval::Diff(view) => diff::lines(view, area.width as usize),
            crate::state::Approval::Permission(view) => {
                permission::lines(view, area.width as usize)
            }
        };
        draw_lines(&lines, area, buf, palette::GROUND);
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
            screen.working.is_some(),
            area.width as usize,
        )
    } else {
        Vec::new()
    };
    if (area.width as usize) < PANE_FLOOR_COLS && pane_cols.is_none() && !screen.ledger_overlay {
        // §6: the one bottom-of-screen line, only when the ledger is gone.
        composer_lines.push(composer::floor_line(
            "ask",
            "claude",
            "—",
            area.width as usize,
        ));
    }
    let composer_height = composer_lines.len() as u16;

    let transcript_area = Rect {
        x: area.x,
        y: area.y,
        width: area.width - pane_cols.unwrap_or(0) as u16,
        height: area.height.saturating_sub(composer_height),
    };

    // The transcript pins to the bottom: the newest rows are always visible.
    let working_label = screen.working.as_ref().map(|w| w.label.as_str());
    let mut body = transcript::lines(
        &screen.transcript,
        transcript_area.width as usize,
        working_label,
        now_ms,
        screen.reduced_motion,
    );
    // A docked overlay reserves rows above the composer; it never floats.
    if let Some(p) = &screen.picker {
        body.push(Line::default());
        body.extend(picker::lines(p, transcript_area.width as usize));
    }
    if let Some(groups) = &screen.status {
        body.push(Line::default());
        body.extend(status::lines(groups));
    }
    draw_lines_bottom(&body, transcript_area, buf, palette::GROUND);

    let composer_area = Rect {
        x: area.x,
        y: area.y + transcript_area.height,
        width: area.width - pane_cols.unwrap_or(0) as u16,
        height: composer_height,
    };
    draw_lines(&composer_lines, composer_area, buf, palette::GROUND);

    if let Some(cols) = pane_cols {
        draw_pane(screen, area, buf, cols, now_ms);
    }
}

/// The right pane: BLOCK background, content on its inner grid, PEEK banner
/// on BLOCK+ when promoted (SPEC §5).
fn draw_pane(screen: &Screen, area: Rect, buf: &mut Buffer, cols: usize, _now_ms: u64) {
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
        // OUTPUT / DIFF / WORKERS land in M4+; the pane still earns its place.
        _ => vec![],
    };
    draw_lines(&lines, content, buf, palette::BLOCK);
    // PEEK: a two-line banner on BLOCK+ drawn OVER the ledger's top rows —
    // the ledger underneath does not move (SPEC §5).
    if let Promotion::Peek { lines: peek, .. } = &screen.promotion {
        for (n, line) in peek.iter().enumerate() {
            let y = content.y + n as u16;
            if y >= content.bottom() {
                break;
            }
            let banner = super::fill(
                Line::styled(line.clone(), Style::new().fg(palette::INK)),
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
                palette::BLOCK_PLUS,
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
fn draw_lines(lines: &[Line<'static>], area: Rect, buf: &mut Buffer, bg: ratatui::style::Color) {
    for (n, line) in lines.iter().enumerate() {
        let y = area.y + n as u16;
        if y >= area.bottom() {
            break;
        }
        buf.set_line(area.x, y, line, area.width);
    }
    // Lines narrower than the area still sit on the region's background.
    let _ = bg;
}

/// Draw the transcript: top-aligned while it fits (the conversation grows
/// downward from the top), scrolling only once it overfills the area.
fn draw_lines_bottom(
    lines: &[Line<'static>],
    area: Rect,
    buf: &mut Buffer,
    bg: ratatui::style::Color,
) {
    let fits = area.height as usize;
    let start = lines.len().saturating_sub(fits);
    draw_lines(&lines[start..], area, buf, bg);
}
