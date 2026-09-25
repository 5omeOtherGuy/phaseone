//! Screen composition: transcript + right pane + composer into a `Buffer`.
//! Panes are separated by a one-step background shift (GROUND vs BLOCK) and by
//! whitespace — there is no border to draw (SPEC §3). The composition is one
//! pure function so every §4 screen snapshots against `TestBackend`.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Style;
use ratatui::text::Line;

use crate::palette;
use crate::state::{PaneMode, Promotion, Screen};

use super::{home, ledger, output, picker, status, workers};

/// The pane's padding: the content grid is the pane width minus 4 on each
/// side (SPEC §5: LEDGER grid 32 in a 40-ch pane; recorded as a refinement —
/// the mock-up does not pin the split, 4/4 keeps the rule symmetric).
const PANE_PAD: usize = 4;

/// Draw the whole screen. `now_ms` drives the working indicator; callers pass
/// a fake clock in tests.
pub fn draw(screen: &mut Screen, area: Rect, buf: &mut Buffer, now_ms: u64) {
    screen.cursor_position = None;
    screen.tool_hits.clear();
    screen.output_area = Rect::default();
    screen.transcript_area = Rect::default();
    screen.composer_area = Rect::default();
    screen.live_area = Rect::default();
    screen.overlay_area = Rect::default();
    screen.overlay_hidden = false;
    screen.approval_visible = false;
    screen.counting = false;
    screen.animating = false;
    screen.frame_width = area.width;
    fill_bg(area, buf, palette::GROUND);
    if area.width < 5 || area.height < 4 {
        return;
    }
    let inner = Rect::new(area.x + 2, area.y + 1, area.width - 4, area.height - 3);
    draw_lines(
        &[super::statusline::line(
            screen,
            inner.width as usize,
            now_ms,
        )],
        Rect::new(inner.x, area.bottom() - 2, inner.width, 1),
        buf,
    );
    draw_content(screen, inner, buf, now_ms, area.width);
    screen.color_mode.apply(buf);
}

fn draw_content(
    screen: &mut Screen,
    area: Rect,
    buf: &mut Buffer,
    now_ms: u64,
    terminal_width: u16,
) {
    // A pending approval is the transcript's newest block (screens.html 2c): the
    // prompt and reasoning above it stay readable and scrollable, the draft stays
    // below it, and its decision row is pinned right under it. Full width.
    let approval = screen
        .approval
        .as_ref()
        .map(|a| approval_block(a, area.width as usize, screen.approvals_waiting));
    // The pane beside the transcript, when it fits; a focused OUTPUT that
    // cannot fit becomes an opaque overlay — keys never go to an undrawn layer.
    let side = (!screen.focus && approval.is_none() && now_ms >= screen.pane_hold_until)
        .then(|| screen.pane_width.columns(terminal_width as usize))
        .flatten()
        // A wide pane that no longer fits (the terminal shrank) reads at 40
        // until there is room again; the chosen width is kept.
        .and_then(|n| {
            let fits = |n: usize| n + 50 <= area.width as usize;
            if fits(n) {
                Some(n)
            } else {
                (n > 40 && fits(40)).then_some(40)
            }
        });
    let overlay = screen.ledger_overlay
        || (screen.output_focus
            && screen.output.is_some()
            && screen.pane_mode == PaneMode::Output
            && side.is_none());
    let pane_cols = if overlay { None } else { side };
    let width = area
        .width
        .saturating_sub(pane_cols.map(|n| n as u16 + 2).unwrap_or(0));
    screen.transcript_width = width as usize;
    // Focus mode hides the input until it is used, but never what a key just
    // did or can do now: the hint row stays for a notice, an armed quit, a
    // selection, a running turn, or an empty session.
    let input_shown = screen.composer.visible(screen.focus);
    let hint_only = !input_shown
        && (screen.flash.is_some()
            || screen.quit_armed
            || screen.selected.is_some()
            // At the very smallest sizes a running turn keeps its text rows.
            || ((screen.working.is_some() || screen.busy) && area.height > 8)
            || screen.approval.is_some()
            || screen
                .transcript
                .blocks
                .iter()
                .all(|b| matches!(b, crate::transcript::Block::Info { .. })));
    let editor = (input_shown || hint_only).then(|| {
        let mut layout =
            super::editor::layout(screen, width as usize, (area.height as usize / 3).max(2));
        if hint_only {
            layout.lines.drain(..layout.lines.len() - 1);
            layout.rows.clear();
            layout.first_row = 1;
        }
        layout
    });
    if let Some(editor) = editor.as_ref().filter(|_| input_shown) {
        screen.composer.scroll = editor.scroll;
        screen.composer.room = editor.room;
    }
    let composer_height = editor
        .as_ref()
        .map(|e| e.lines.len() as u16)
        .unwrap_or(0)
        .min(area.height.saturating_sub(1));
    let decision_height = approval.as_ref().map_or(0, |(_, d)| d.len() as u16);
    let transcript_area = Rect::new(
        area.x,
        area.y,
        width,
        area.height
            .saturating_sub(composer_height + 2 + decision_height),
    );
    // Overlays (a picker, /help or /status, the slash palette) dock above the
    // composer on the band grid. The transcript ends above them, as it does
    // above a taller composer: the newest rows and the working row stay in
    // view while they are open. A tall one scrolls instead of losing its top.
    let grid = (width as usize).saturating_sub(4);
    // Over a transcript a few of its rows stay above the overlay (the live
    // tail, the working row) with one blank row between them.
    let home = screen.working.is_none()
        && screen
            .transcript
            .blocks
            .iter()
            .all(|b| matches!(b, crate::transcript::Block::Info { .. }));
    let reserve = if home {
        0
    } else {
        (transcript_area.height as usize / 3).min(4)
    };
    let avail = (transcript_area.height as usize).saturating_sub(reserve);
    let gap = usize::from(!home && avail > 4);
    let room = avail - gap;
    let (overlay_lines, kind, selected_line) = overlay_content(screen, grid, room);
    if kind != screen.overlay_kind {
        screen.overlay_kind = kind;
        screen.overlay_scroll = 0;
        screen.overlay_height = 0;
    }
    let had_overlay = !overlay_lines.is_empty();
    let mut window = overlay_window(
        overlay_lines,
        room,
        grid,
        &mut screen.overlay_scroll,
        selected_line,
    );
    screen.overlay_hidden = had_overlay && window.is_empty();
    if window.is_empty() {
        screen.overlay_height = 0;
    } else {
        // An open overlay keeps the height it reached: narrowing the palette
        // by typing never moves the transcript above it.
        let keep = screen.overlay_height.min(room);
        while window.len() < keep {
            window.insert(0, Line::default());
        }
        screen.overlay_height = window.len();
        if gap == 1 {
            window.insert(0, Line::default());
        }
    }
    let height = (transcript_area.height as usize).saturating_sub(window.len());
    let tail = match (&approval, &screen.working) {
        (Some((block, _)), _) => block.clone(),
        (None, Some(w)) if screen.transcript.running_indices().next().is_none() => {
            // After a few silent seconds the row counts them (retries and
            // back-off are otherwise indistinguishable from a hang).
            let waited = now_ms.saturating_sub(screen.phase_since_ms.max(w.started_ms)) / 1_000;
            // The row is drawn: the driver redraws each second so it counts.
            screen.counting = true;
            let label = if waited >= 3 {
                format!("{} · {waited}s", w.label)
            } else {
                w.label.clone()
            };
            vec![super::block::working_line(
                &label,
                width as usize,
                now_ms,
                screen.reduced_motion,
            )]
        }
        // A turn about to start (a follow-up or prompt taken, its first event
        // not yet here) already shows it is working: no frame without it.
        (None, None) if screen.busy && screen.transcript.running_indices().next().is_none() => {
            vec![super::block::working_line(
                crate::state::WAITING,
                width as usize,
                now_ms,
                screen.reduced_motion,
            )]
        }
        _ => vec![],
    };
    let rows = super::block::measure(&screen.transcript, width as usize);
    let total = if tail.is_empty() {
        rows
    } else {
        rows + usize::from(rows > 0) + tail.len()
    };
    let max_top = total.saturating_sub(height);
    // A new approval taller than the view opens at its header: the decision
    // row is pinned below it, the tool and path are read first.
    if std::mem::take(&mut screen.approval_reveal) {
        screen.scroll_written = None;
        screen.scroll_top =
            (tail.len() > height).then_some((rows + usize::from(rows > 0)).min(max_top));
    }
    if let Some(block) = screen.refollow.take()
        && super::block::block_start(&screen.transcript, block)
            .is_some_and(|start| start >= max_top)
    {
        screen.scroll_top = None;
    }
    // A detached view stays on the reader's content: resolve its anchor at this
    // width unless a command (PageUp, /find) moved it since the last frame. A
    // block that re-wrapped keeps the same proportion of its rows above the top.
    // The anchor is taken when the reader moves the view and kept until they
    // move it again: every relayout re-maps the ORIGINAL (block, offset) —
    // scaled by its extent only across a width change — so resize round trips
    // are exact and rounding never accumulates. A view inside the tail (an
    // approval read from its header) keeps its offset into the tail.
    let moved = screen.scroll_top != screen.scroll_written;
    let tail_start = rows + usize::from(rows > 0);
    if let Some(mut top) = screen.scroll_top {
        let mut relayout = false;
        if !moved {
            if let Some((block, offset)) = screen.scroll_anchor {
                let extent = super::block::block_extent(&screen.transcript, block);
                // A re-wrap scales the offset by the block's height at the two
                // widths now (a block that also grew — a stream — keeps the
                // reader on the same text, not on the same fraction).
                let offset = match extent {
                    Some(new) if screen.scroll_anchor_width != width as usize => {
                        let old = super::block::extent_at(
                            &screen.transcript,
                            block,
                            screen.scroll_anchor_width,
                        )
                        .max(1);
                        (offset * new / old).min(new.saturating_sub(1))
                    }
                    Some(new) => offset.min(new.saturating_sub(1)),
                    None => offset,
                };
                if let Some(row) = super::block::row_of_anchor(&screen.transcript, (block, offset))
                {
                    top = row;
                    relayout = true;
                }
            } else if let Some(offset) = screen.scroll_tail.filter(|_| !tail.is_empty()) {
                top = tail_start + offset;
                relayout = true;
            }
        }
        screen.scroll_top = if top < max_top {
            Some(top)
        } else if relayout
            && max_top > 0
            && screen.scroll_anchor.is_some()
            && screen.scroll_anchor_width != width as usize
        {
            // A relayout (not the reader) reached the bottom: show the bottom
            // but keep the place, so the way back restores it.
            Some(max_top)
        } else {
            // Reaching the bottom re-engages following (Iris follow_by_overscroll).
            None
        };
        if screen.scroll_top.is_none() || moved {
            screen.scroll_anchor = None;
            screen.scroll_tail = None;
        }
    } else {
        screen.scroll_anchor = None;
        screen.scroll_tail = None;
    }
    let (_, body) = super::block::viewport_with(
        &screen.transcript,
        width as usize,
        height,
        screen.scroll_top,
        &tail,
        now_ms,
        screen.reduced_motion,
    );
    if let Some(top) = screen.scroll_top
        && screen.scroll_anchor.is_none()
        && screen.scroll_tail.is_none()
    {
        screen.scroll_anchor = super::block::anchor_of(&screen.transcript, top);
        screen.scroll_tail = screen
            .scroll_anchor
            .is_none()
            .then(|| top.checked_sub(tail_start))
            .flatten();
        screen.scroll_anchor_extent = screen
            .scroll_anchor
            .and_then(|(block, _)| super::block::block_extent(&screen.transcript, block))
            .unwrap_or(0);
        screen.scroll_anchor_width = width as usize;
    }
    screen.scroll_written = screen.scroll_top;
    screen.last_rendered = (total, height);
    screen.transcript_area = transcript_area;
    let top = screen.scroll_top.unwrap_or(max_top);
    {
        let running_visible = super::block::hits(&screen.transcript, top, height)
            .iter()
            .any(|(_, hit)| matches!(hit, super::block::Hit::Header(i)
                if matches!(&screen.transcript.blocks[*i],
                    crate::transcript::Block::Call(r) if r.status == crate::transcript::RowStatus::Running)));
        let tail_visible =
            approval.is_none() && !tail.is_empty() && total.saturating_sub(1) < top + height;
        // Without colour the LEDs cannot pulse; the seconds still count.
        screen.animating = ((running_visible && approval.is_none()) || tail_visible)
            && screen.color_mode != crate::palette::ColorMode::Plain;
    }
    draw_lines(&body, transcript_area, buf);
    let mut hits = super::block::hits(&screen.transcript, top, height);
    let mut sticky = false;
    // Inside a block expanded past the screen, its header stays on the top row:
    // it names what is being read and a click folds it back.
    if let Some((block, offset)) = super::block::anchor_of(&screen.transcript, top)
        && screen.transcript.disclosures.get(&block) == Some(&true)
        && matches!(
            screen.transcript.blocks.get(block),
            Some(crate::transcript::Block::Call(_))
        )
        && offset > usize::from(super::block::separated(&screen.transcript, block))
        && let Some(header) = super::block::block_header(&screen.transcript, block)
    {
        draw_lines(&[header], transcript_area, buf);
        sticky = true;
        hits.retain(|(row, _)| *row != 0);
        hits.push((0, super::block::Hit::Header(block)));
    }
    // The keyboard-selected block: its glyph and name field invert (selection
    // is the one highlight, SPEC §1), and so read without colour too.
    if let Some(selected) = screen.selected
        && let Some(row) = super::block::block_start(&screen.transcript, selected)
        && row >= top
        && row < top + height
    {
        let y = transcript_area.y + (row - top) as u16;
        let x0 = transcript_area.x + 2;
        for x in x0..(x0 + 12).min(transcript_area.x + width) {
            let cell = &mut buf[(x, y)];
            cell.set_fg(palette::GROUND);
            cell.set_bg(palette::INK);
        }
    }
    // The current /find match: its row lifts to BLOCK+ with INK text, and is
    // underlined so it reads without colour too.
    screen.refresh_search();
    if let Some(row) = screen.search_row()
        && row >= top + usize::from(sticky)
        && row < top + height
    {
        let y = transcript_area.y + (row - top) as u16;
        for x in transcript_area.x..transcript_area.x + width {
            let cell = &mut buf[(x, y)];
            if cell.bg != palette::INK {
                cell.set_bg(palette::BLOCK_PLUS);
            }
            if cell.symbol() != " " {
                if cell.fg != palette::GROUND {
                    cell.set_fg(palette::INK);
                }
                cell.modifier.insert(ratatui::style::Modifier::UNDERLINED);
            }
        }
    }
    let mut gap_row = None;
    if !window.is_empty() {
        let y0 = transcript_area.y + height as u16;
        if gap == 1 {
            gap_row = Some(y0);
        }
        let rect = Rect::new(transcript_area.x, y0, width, window.len() as u16);
        fill_bg(rect, buf, palette::GROUND);
        draw_lines(
            &window,
            Rect::new(
                transcript_area.x + 2,
                y0,
                width.saturating_sub(4),
                rect.height,
            ),
            buf,
        );
        screen.overlay_area = rect;
    }
    if !overlay {
        screen.tool_hits = hits
            .into_iter()
            .map(|(row, hit)| {
                (
                    Rect::new(
                        transcript_area.x,
                        transcript_area.y + row as u16,
                        transcript_area.width,
                        1,
                    ),
                    hit,
                )
            })
            .collect();
        let mut below_y = transcript_area.bottom();
        if let Some((block, decisions)) = &approval {
            // What y/a/n decide stays named: when the approval's header is
            // scrolled away, it sits on the last row above the decision row.
            let header_row = rows + usize::from(rows > 0);
            if (header_row < top || header_row >= top + height)
                && height >= 2
                && let Some(header) = block.first()
            {
                let y = transcript_area.y + height as u16 - 1;
                draw_lines(
                    std::slice::from_ref(header),
                    Rect::new(area.x, y, width, 1),
                    buf,
                );
            }
            // Right under the block's last row: band C of the blocking block.
            let y = transcript_area.y + (total.min(height)) as u16;
            let h = decision_height.min(area.bottom().saturating_sub(y + 1));
            draw_lines(decisions, Rect::new(area.x, y, width, h), buf);
            screen.approval_visible = h > 0;
            screen.approval_grant_visible = h > 0 && h >= decision_height;
            below_y = transcript_area.bottom() + decision_height;
        }
        // With an overlay docked, the count sits in the gap above it (next
        // to the rows it counts), not between the overlay and the composer.
        if let Some(row) = gap_row {
            below_y = row;
        }
        let composer_top = area.bottom().saturating_sub(composer_height + 1);
        if screen.scroll_top.is_some() && below_y < composer_top && below_y < area.bottom() {
            let rect = Rect::new(area.x, below_y, width, 1);
            screen.live_area = rect;
            let search = screen.search.as_ref().map(|f| {
                format!(
                    " · {}/{} find \"{}\"",
                    f.hits.len() - f.index,
                    f.hits.len(),
                    f.query
                )
            });
            draw_lines(
                &[below_line(max_top - top, width as usize, search.as_deref())],
                rect,
                buf,
            );
        }
    }
    if !screen.focus
        && screen.approval.is_none()
        && screen.working.is_none()
        && window.is_empty()
        && screen
            .transcript
            .blocks
            .iter()
            .all(|b| matches!(b, crate::transcript::Block::Info { .. }))
    {
        let used = (body.len() as u16).min(transcript_area.height);
        home::draw(
            Rect::new(area.x, area.y + used, width, transcript_area.height - used),
            buf,
        );
    }
    if let Some(editor) = editor {
        let composer_area = Rect::new(
            area.x,
            area.bottom().saturating_sub(composer_height + 1),
            width,
            composer_height,
        );
        if !overlay {
            screen.composer_area = composer_area;
            screen.composer_rows = (editor.rows.clone(), editor.first_row);
        }
        draw_lines(&editor.lines, composer_area, buf);
        if !overlay
            && input_shown
            && !screen.output_focus
            && screen.selected.is_none()
            && screen.picker.is_none()
            && screen.status.is_none()
        {
            screen.cursor_position = Some((
                composer_area.x + (editor.cursor.0 as u16).min(width.saturating_sub(1)),
                composer_area.y + (editor.cursor.1 as u16).min(composer_height.saturating_sub(1)),
            ));
        }
    }
    if let Some(cols) = pane_cols {
        draw_pane(
            screen,
            Rect {
                height: area.height.saturating_sub(1),
                ..area
            },
            buf,
            cols,
        );
    }
    if overlay {
        draw_pane(
            screen,
            Rect {
                height: area.height.saturating_sub(1),
                ..area
            },
            buf,
            area.width as usize,
        );
    }
}

/// What overlay is open: 0 none, 1 picker, 2 /help or /status, 3 the palette.
const OVERLAY_STATUS: u8 = 2;

/// The open overlay's rows, its kind and the selected row's index (pickers
/// and the palette; the window follows it).
fn overlay_content(
    screen: &Screen,
    grid: usize,
    room: usize,
) -> (Vec<Line<'static>>, u8, Option<usize>) {
    let mut lines: Vec<Line<'static>> = vec![];
    // Entry rows a picker may use: what is left after its headers and footer.
    let rows = |lines: &Vec<Line<'static>>, p: &picker::Picker| {
        room.saturating_sub(lines.len() + p.groups.len() + 1)
    };
    let kind = if let Some(p) = &screen.picker {
        // The typed filter is always visible, and an empty result says so
        // (the picker still owns the keys).
        if !p.filter.is_empty() {
            lines.push(Line::styled(
                format!("/ {}", p.filter),
                Style::new().fg(palette::INK),
            ));
        }
        if p.visible().is_empty() {
            lines.push(Line::styled(
                "  no match — ⌫ widens, esc closes",
                Style::new().fg(palette::DIM),
            ));
        }
        let rows = rows(&lines, p);
        lines.extend(picker::lines_window(p, grid, rows));
        fit_picker(&mut lines, p, room);
        1
    } else if let Some(groups) = &screen.status {
        lines.extend(status::lines(groups, grid));
        OVERLAY_STATUS
    } else {
        let palette_rows = screen.palette();
        if palette_rows.is_empty() {
            return (lines, 0, None);
        }
        let picker = picker::Picker {
            groups: vec![picker::PickerGroup {
                header: "COMMANDS".into(),
                rows: palette_rows
                    .iter()
                    .map(|c| picker::PickerRow {
                        label: match c.args {
                            crate::commands::Args::None => format!("/{}", c.name),
                            crate::commands::Args::Optional(a) => format!("/{} [{a}]", c.name),
                            crate::commands::Args::Required(a) => format!("/{} <{a}>", c.name),
                        },
                        value: c.description.into(),
                        available: true,
                    })
                    .collect(),
            }],
            filter: String::new(),
            selected: screen.palette_selected,
        };
        let rows = rows(&lines, &picker);
        lines.extend(picker::lines_window(&picker, grid, rows));
        fit_picker(&mut lines, &picker, room);
        3
    };
    let selected = (kind != OVERLAY_STATUS)
        .then(|| {
            lines.iter().position(|l| {
                l.spans
                    .iter()
                    .any(|s| s.style.bg == Some(palette::SELECTION_BG))
            })
        })
        .flatten();
    (lines, kind, selected)
}

/// A picker in fewer rows than its headers and footer need: the group
/// headers go first, then everything but the selected entry, so the one row
/// left is the one Enter runs (never counted twice by a second window).
fn fit_picker(lines: &mut Vec<Line<'static>>, picker: &picker::Picker, room: usize) {
    if lines.len() <= room {
        return;
    }
    let text =
        |l: &Line<'static>| -> String { l.spans.iter().map(|s| s.content.as_ref()).collect() };
    lines.retain(|l| !picker.groups.iter().any(|g| text(l) == g.header));
    if lines.len() > room {
        let selected = lines.iter().position(|l| {
            l.spans
                .iter()
                .any(|s| s.style.bg == Some(palette::SELECTION_BG))
        });
        if let Some(at) = selected {
            let keep = lines.remove(at);
            lines.clear();
            lines.push(keep);
        } else {
            lines.truncate(room);
        }
    }
}

/// The rows of an overlay that fit `avail` rows: all of them, or a window and
/// a footer saying what is hidden on either side. A picker's window follows
/// its selection, so the inverted row is always the one Enter runs; /help and
/// /status scroll by `scroll`.
fn overlay_window(
    mut lines: Vec<Line<'static>>,
    avail: usize,
    grid: usize,
    scroll: &mut usize,
    selected: Option<usize>,
) -> Vec<Line<'static>> {
    if lines.len() <= avail {
        *scroll = 0;
        return lines;
    }
    if avail == 0 {
        return vec![];
    }
    if selected.is_none() {
        // The footer below says `esc`; the overlay's own closing hint goes.
        while lines.last().is_some_and(|l| {
            let text: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            text.trim().is_empty() || text.trim() == "esc"
        }) {
            lines.pop();
        }
    }
    let len = lines.len();
    // One row says what is hidden, when there is room for an entry as well.
    let rows = if avail >= 2 { avail - 1 } else { avail }.min(len);
    let mut first = (*scroll).min(len - rows);
    if let Some(selected) = selected {
        if selected < first {
            first = selected;
        } else if selected >= first + rows {
            first = selected + 1 - rows;
        }
    }
    *scroll = first;
    let mut window = lines[first..first + rows].to_vec();
    if rows < avail {
        let below = len - first - rows;
        let mut more = vec![];
        if first > 0 {
            more.push(format!("↑ {first}"));
        }
        if below > 0 {
            more.push(format!("↓ {below}"));
        }
        let keys = if selected.is_some() {
            "↑↓ select"
        } else {
            "↑↓ scroll"
        };
        let more = if more.is_empty() {
            String::new()
        } else {
            format!("{} more   ", more.join(" "))
        };
        // Shorter wordings before any cut: `esc` is never lost.
        let footer = [
            format!("  {more}{keys}   esc"),
            format!("  {more}esc"),
            "  esc".to_owned(),
        ]
        .into_iter()
        .find(|f| crate::wrap::cell_width(f) <= grid + 2)
        .unwrap_or_else(|| "  esc".to_owned());
        window.push(Line::styled(footer, Style::new().fg(palette::FAINT)));
    }
    window
}

/// The right pane: BLOCK background, content on its inner grid, PEEK banner
/// on BLOCK+ when promoted (SPEC §5).
fn draw_pane(screen: &mut Screen, area: Rect, buf: &mut Buffer, cols: usize) {
    let pane = Rect {
        x: area.x + area.width - cols as u16,
        y: area.y,
        width: cols as u16,
        height: area.height,
    };
    if screen.pane_mode == PaneMode::Output {
        screen.output_area = pane;
        screen.scroll_output_by(0);
    }
    fill_bg(pane, buf, palette::BLOCK);
    let grid = cols.saturating_sub(2 * PANE_PAD);
    let content = Rect {
        x: pane.x + PANE_PAD as u16,
        y: pane.y,
        width: grid as u16,
        height: pane.height,
    };
    let lines = match screen.pane_mode {
        PaneMode::Ledger => ledger::lines_at(&screen.ledger(), grid),
        PaneMode::Output => match &screen.output {
            Some(view) => {
                // Only an output newer than the one shown is "newer".
                let at = |id: &crate::fold::FoldId| {
                    screen.transcript.blocks.iter().position(|b| {
                        matches!(b, crate::transcript::Block::Call(r) if r.output_id.as_ref() == Some(id))
                    })
                };
                let newer = screen
                    .transcript
                    .latest_fold
                    .as_ref()
                    .filter(|latest| **latest != view.id && at(latest) > at(&view.id));
                output::pane_lines(&output::Pane {
                    view,
                    grid,
                    filter: &screen.output_filter,
                    matches: screen
                        .output_matches
                        .as_deref()
                        .filter(|_| !screen.output_filter.is_empty()),
                    horizontal: screen.output_horizontal,
                    height: content.height as usize,
                    focused: screen.output_focus,
                    newer,
                    searching: screen.output_search,
                })
            }
            None => vec![Line::styled(
                super::block::ellipsis("· no output open   ^O latest", grid),
                Style::new().fg(palette::DIM),
            )],
        },
        PaneMode::Workers => workers::lines(&screen.workers, grid),
        // DIFF lands with its pane mode; the pane still earns its place.
        PaneMode::Diff => screen
            .transcript
            .blocks
            .iter()
            .rev()
            .find_map(|b| match b {
                crate::transcript::Block::Call(row)
                    if matches!(row.name.as_str(), "edit" | "write") =>
                {
                    Some(super::block::call_lines(row, grid, 0, true))
                }
                _ => None,
            })
            .unwrap_or_else(|| {
                vec![Line::styled(
                    "No file changes in this session.",
                    Style::new().fg(palette::DIM),
                )]
            }),
    };
    draw_lines(&lines, content, buf);
    if content.height > 1 {
        // The longest wording that fits the pane's grid: the keys stay, the
        // words shorten (a 32-cell grid never loses `esc`).
        let output = screen.pane_mode == PaneMode::Output;
        let kept_filter = !screen.output_filter.is_empty();
        let choices: &[&str] = if screen.output_search {
            &[
                "type to filter   ⏎ keep   esc cancel",
                "filter  ⏎ keep  esc cancel",
                "⏎ keep  esc",
            ]
        } else if output && screen.output_focus && kept_filter {
            &[
                "↑↓ scroll  ←→ pan  / filter  y copy  esc clear filter",
                "↑↓ ←→  /  y copy  esc clear filter",
                "↑↓ ←→ / y  esc clear",
            ]
        } else if output && screen.output_focus {
            &[
                "↑↓ scroll  ←→ pan  / filter  y copy  esc close",
                "↑↓ ←→ pan  / filter  y copy  esc",
                "↑↓ ←→  /  y  esc",
            ]
        } else if output && screen.output.is_some() {
            match screen.pane_ctrl_o() {
                Some(crate::state::CtrlO::Open(_)) => {
                    &["^O next output   esc close", "^O next  esc"]
                }
                _ => &["^O focus   esc close"],
            }
        } else if screen.ledger_overlay {
            &["^L close"]
        } else if screen.legacy_keyboard {
            &["F6 pane   ^W width"]
        } else {
            &["^Tab pane   ^W width"]
        };
        // A notice for what just happened in the pane (a copy) shows here
        // when the pane covers the composer's hint row.
        let flash = screen
            .flash
            .as_ref()
            .filter(|_| cols >= area.width as usize)
            .map(|(text, _)| text.as_str());
        let hint = flash
            .or_else(|| {
                choices
                    .iter()
                    .copied()
                    .find(|c| crate::wrap::cell_width(c) <= grid)
            })
            .unwrap_or(choices[choices.len() - 1]);
        let hint = super::block::ellipsis(hint, grid);
        draw_lines(
            &[super::block::band(
                vec![ratatui::text::Span::styled(
                    hint,
                    Style::new().fg(palette::FAINT),
                )],
                grid,
                palette::BLOCK,
            )],
            Rect {
                y: content.bottom() - 1,
                height: 1,
                ..content
            },
            buf,
        );
    }
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

/// The approval's block (header + body, blocking glyph) and its decision rows.
fn approval_block(
    approval: &crate::state::Approval,
    width: usize,
    waiting: usize,
) -> (Vec<Line<'static>>, Vec<Line<'static>>) {
    use super::diff::DiffRow;
    // `p project` is unavailable until a trust store persists grants: say why.
    let grantable = match approval {
        crate::state::Approval::Diff(v) => v.grantable,
        crate::state::Approval::Permission(v) => v.grantable,
    };
    let note = match (waiting > 0, grantable) {
        (true, true) => format!("+{waiting} waiting · p: no trust store yet"),
        (true, false) => format!("+{waiting} waiting"),
        (false, true) => "p: no trust store yet".into(),
        (false, false) => String::new(),
    };
    match approval {
        crate::state::Approval::Diff(view) => {
            let (added, removed) = view.counts();
            // A patch over several files shows them all, each under its name.
            let (argument, files) = if view.files.len() > 1 {
                (
                    format!("{} +{}", view.file, view.files.len() - 1),
                    format!("{} files", view.files.len()),
                )
            } else {
                let (at, of) = view.position;
                (
                    view.file.clone(),
                    format!("{at} of {of} {}", if of == 1 { "file" } else { "files" }),
                )
            };
            let mut block = vec![super::block::header(
                &view.tool,
                &argument,
                &format!("+{added} −{removed} · {files}"),
                width,
                true,
            )];
            // What will happen when it runs comes before the rows.
            if let Some(note) = &view.note {
                // Wrapped, never cut: the consequence is the point.
                let room = width.saturating_sub(4 + 9).max(8);
                for (i, part) in crate::wrap::wrap(note, room).into_iter().enumerate() {
                    block.push(super::block::label(
                        if i == 0 { "note" } else { "" },
                        &part,
                        width,
                    ));
                }
            }
            let digits = view
                .rows
                .iter()
                .map(|r| match r {
                    DiffRow::Add { line, .. }
                    | DiffRow::Del { line, .. }
                    | DiffRow::Context { line, .. } => line.to_string().len(),
                })
                .max()
                .unwrap_or(3);
            let mut sections = view.files.iter().peekable();
            for (i, r) in view.rows.iter().enumerate() {
                while let Some((_, name)) = sections.next_if(|(at, _)| *at <= i) {
                    block.push(super::block::label("file", name, width));
                }
                let (n, sign, text) = match r {
                    DiffRow::Add { line, text } => (*line, '+', text),
                    DiffRow::Del { line, text } => (*line, '-', text),
                    DiffRow::Context { line, text } => (*line, ' ', text),
                };
                block.push(super::block::numbered(
                    n as usize,
                    digits,
                    Some(sign),
                    text,
                    width,
                ));
            }
            // Files with no rows (a deletion) still say what happens to them.
            for (_, name) in sections {
                block.push(super::block::label("file", name, width));
            }
            if view.rows.is_empty() && view.files.is_empty() {
                block.push(super::block::body(
                    "No change to show",
                    width,
                    palette::DIM,
                    palette::BLOCK,
                ));
            }
            (
                block,
                super::block::decisions_with(view.grantable, width, &note),
            )
        }
        crate::state::Approval::Permission(view) => {
            // The header names the first line; the body shows the whole
            // command, wrapped, because it is what is being approved.
            let mut lines = view.command.lines();
            let first = lines.next().unwrap_or("");
            let arg = if lines.next().is_some() {
                format!("{first} …")
            } else {
                first.to_owned()
            };
            let mut block = vec![super::block::header(
                &view.tool,
                &arg,
                "approval required",
                width,
                true,
            )];
            let room = width.saturating_sub(4 + 9).max(8);
            let mut first_row = true;
            // Wrapped by cells, not words: a command's spacing (`' '`) is
            // part of what is approved and is never dropped at a break.
            for line in view.command.lines() {
                let line = super::block::clean(line);
                let mut rest = line.as_str();
                loop {
                    let part = crate::wrap::fit_cells(rest, room);
                    block.push(super::block::label(
                        if first_row { "command" } else { "" },
                        &part,
                        width,
                    ));
                    first_row = false;
                    rest = &rest[part.len()..];
                    if rest.is_empty() || part.is_empty() {
                        break;
                    }
                }
            }
            // A long path keeps its distinguishing end: `…/src/deep/leaf`.
            block.extend(view.rows.iter().map(|(k, v)| {
                let value = if k == "cwd" && crate::wrap::cell_width(v) > room {
                    let tail: String = v
                        .chars()
                        .rev()
                        .scan(0, |used, ch| {
                            *used += crate::wrap::cell_width(ch.encode_utf8(&mut [0; 4]));
                            (*used < room).then_some(ch)
                        })
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    // Start at a whole component where one begins in the tail.
                    let tail = match tail.find('/') {
                        Some(at) if at > 0 => tail[at..].to_owned(),
                        _ => tail,
                    };
                    format!("…{tail}")
                } else {
                    v.clone()
                };
                super::block::label(k, &value, width)
            }));
            (
                block,
                super::block::decisions_with(view.grantable, width, &note),
            )
        }
    }
}

/// `↓ N lines below · click to follow · ^End`: the count DIM, the hint FAINT,
/// shortened before it would be cut.
fn below_line(below: usize, width: usize, search: Option<&str>) -> Line<'static> {
    use ratatui::text::Span;
    // A held place at the very bottom (the terminal grew) says so plainly.
    let count = if below == 0 {
        format!("  at the end{}", search.unwrap_or(""))
    } else {
        format!(
            "  ↓ {} below{}",
            super::block::plural(below, "line"),
            search.unwrap_or("")
        )
    };
    let hint = [" · click to follow · ^End", " · ^End", ""]
        .into_iter()
        .find(|h| crate::wrap::cell_width(&count) + crate::wrap::cell_width(h) <= width)
        .unwrap_or("");
    Line::from(vec![
        Span::styled(
            super::block::ellipsis(&count, width),
            Style::new().fg(palette::DIM),
        ),
        Span::styled(hint, Style::new().fg(palette::FAINT)),
    ])
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
