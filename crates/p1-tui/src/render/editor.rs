//! One layout for painted input and the terminal caret, from the composer's own
//! row map (word wrap, glyph break for over-long words). Only a window of rows
//! around the caret is laid out, so a 200 kB paste costs what a line does.
use super::block;
use crate::{
    editor::{Row, line_rows, logical_lines, row_of},
    palette as p,
    state::{Queued, Screen},
    wrap::cell_width,
};
use ratatui::{style::Style, text::Line, text::Span};
use unicode_width::UnicodeWidthStr;

pub struct Layout {
    pub lines: Vec<Line<'static>>,
    /// Caret (column, row) relative to the layout's first line.
    pub cursor: (usize, usize),
    /// The composer's first visible row, to keep the viewport stable.
    pub scroll: (usize, usize),
    /// Cells per row, for Up/Down between frames.
    pub room: usize,
    /// The painted input rows and the layout row of the first one (clicks).
    pub rows: Vec<Row>,
    pub first_row: usize,
}

/// Summary of a queue too long to list: `· 1 steering, 2 follow-ups queued`.
fn queue_summary(queued: &std::collections::VecDeque<Queued>) -> String {
    let follow = queued.iter().filter(|q| q.follow_up).count();
    let steer = queued.len() - follow;
    let mut parts = vec![];
    if steer > 0 {
        parts.push(format!("{steer} steering"));
    }
    if follow > 0 {
        parts.push(format!(
            "{follow} follow-up{}",
            if follow == 1 { "" } else { "s" }
        ));
    }
    format!("· {} queued", parts.join(", "))
}

pub fn layout(screen: &Screen, width: usize, cap: usize) -> Layout {
    let composer = &screen.composer;
    let text = composer.text.as_str();
    let room = width.saturating_sub(6).max(1);
    let caret = composer.caret_byte();
    let lines = logical_lines(text);
    let caret_line = lines
        .iter()
        .position(|(s, e)| caret >= *s && caret <= *e)
        .unwrap_or(0);
    let cap = cap.max(2);
    // Queued inputs sit above the composer; past two rows they summarise.
    let queued = &screen.queued;
    let queued_rows = if queued.len() <= cap.saturating_sub(3).min(2) {
        queued.len()
    } else {
        1
    };
    let visible = cap.saturating_sub(queued_rows + 1).max(1);
    // Lay out a window of logical lines around the caret: enough rows for any
    // viewport, never the whole draft.
    // Any viewport holding the caret starts at most `visible` lines above it.
    let from = caret_line.saturating_sub(visible);
    let to = (caret_line + visible + 1).min(lines.len());
    let window: Vec<Row> = (from..to)
        .flat_map(|i| line_rows(text, i, lines[i].0, lines[i].1, room))
        .collect();
    let caret_row = window
        .iter()
        .position(|r| r.line == caret_line)
        .map(|first| {
            let rows: Vec<Row> = window
                .iter()
                .filter(|r| r.line == caret_line)
                .copied()
                .collect();
            first + row_of(&rows, caret)
        })
        .unwrap_or(0);
    // Keep the previous top row while the caret stays inside the viewport.
    let previous = window
        .iter()
        .position(|r| (r.line, r.index) == composer.scroll)
        .unwrap_or(0);
    let mut top = previous;
    if caret_row < top {
        top = caret_row;
    } else if caret_row >= top + visible {
        top = caret_row + 1 - visible;
    }
    // Never leave blank rows under the draft while rows above are hidden.
    let window_end = window.len();
    if to == lines.len() && window_end > visible {
        top = top.min(window_end - visible);
    }
    let shown = &window[top..(top + visible).min(window_end)];
    // Lines hidden (wholly or in part) above and below the rows shown — one
    // unit, whatever their wrapping, so the count never depends on the window.
    let above_lines = shown
        .first()
        .map_or(0, |r| r.line + usize::from(r.index > 0));
    let below_lines = shown.last().map_or(0, |r| {
        lines.len().saturating_sub(r.line + 1) + usize::from(!r.last)
    });

    let mut out = vec![];
    if queued_rows == queued.len() {
        for q in queued {
            out.push(block::body(
                &format!(
                    "· {}: {}",
                    if q.follow_up { "follow-up" } else { "steering" },
                    q.text.replace('\n', " ↳ ")
                ),
                width,
                p::DIM,
                p::GROUND,
            ));
        }
    } else {
        out.push(block::body(
            &queue_summary(queued),
            width,
            p::DIM,
            p::GROUND,
        ));
    }
    let first_row = out.len();
    let focused_elsewhere = screen.output_focus || screen.selected.is_some();
    let ink = if focused_elsewhere { p::DIM } else { p::INK };
    for (n, row) in shown.iter().enumerate() {
        // Spaces may hang past the edge (the caret can sit after them); they
        // are not painted, so a word that fills the row keeps its last glyph.
        let mut body = &text[row.start..row.end];
        while cell_width(body) > room && body.ends_with(' ') {
            body = &body[..body.len() - 1];
        }
        let prefix = if n == 0 { "› " } else { "  " };
        out.push(block::body(
            &format!("{prefix}{body}"),
            width,
            ink,
            p::BLOCK_PLUS,
        ));
    }
    if shown.is_empty() {
        out.push(block::body("› ", width, ink, p::BLOCK_PLUS));
    }
    let cursor_col = window
        .get(caret_row)
        .map_or(0, |r| text[r.start..caret.max(r.start).min(r.end)].width());
    let cursor = (
        cursor_col.min(room) + 4,
        caret_row.saturating_sub(top) + first_row,
    );

    // The hint row names keys that work on THIS terminal, chosen before any
    // truncation; the right side tells what ^C will do now.
    let newline = if screen.legacy_keyboard {
        "^J newline"
    } else {
        "⇧⏎ newline"
    };
    let working = screen.working.is_some() || screen.busy;
    let overlay = screen.picker.is_some() || screen.status.is_some() || screen.ledger_overlay;
    let palette = !screen.palette().is_empty() && !screen.overlay_hidden;
    // One description per key and state: what the keys do in the layer that
    // has them (the OUTPUT pane, a picker, the palette), never two meanings
    // for Esc.
    let mut left = if let Some((flash, _)) = &screen.flash {
        flash.clone()
    } else if screen.selected.is_some() {
        "↑↓ select   ⏎ toggle   o open   y copy".to_owned()
    } else if screen.output_focus && screen.output_search {
        "typing goes to the filter   ⏎ keep   esc cancel".to_owned()
    } else if screen.output_focus && !screen.output_filter.is_empty() {
        "type to return   esc clears the filter".to_owned()
    } else if screen.output_focus {
        "type to return   esc closes the pane".to_owned()
    } else if screen.picker.is_some() {
        "↑↓ select   ⏎ open   type to filter".to_owned()
    } else if screen.status.is_some() {
        "↑↓ scroll   typing returns to the draft".to_owned()
    } else if palette {
        "↑↓ select   Tab complete   ⏎ run".to_owned()
    } else if working {
        format!("⏎ steer   {newline}   ⌥⏎ follow-up")
    } else {
        format!("⏎ send   {newline}")
    };
    if above_lines + below_lines > 0
        && !focused_elsewhere
        && screen.selected.is_none()
        && screen.flash.is_none()
    {
        let mut parts = vec![];
        if above_lines > 0 {
            parts.push(format!("↑ {above_lines}"));
        }
        if below_lines > 0 {
            parts.push(format!("↓ {below_lines}"));
        }
        left = format!("{} more   {left}", parts.join(" "));
    }
    let right = if screen.selected.is_some() {
        "esc back"
    } else if working {
        "^C cancel"
    } else if screen.output_focus {
        ""
    } else if overlay || palette {
        // ^C closes the overlay first; it never quits through one.
        "esc close"
    } else if screen.quit_armed && text.is_empty() {
        "^C again to quit"
    } else if screen.quit_armed {
        "^C again to clear"
    } else if !text.is_empty() {
        "/help   ^C clear"
    } else {
        "/help   ^C quit"
    };
    let u = width.saturating_sub(4);
    // A notice is read whole: the standing hint on the right gives way.
    let right = if cell_width(right) + 2 > u
        || (screen.flash.is_some() && cell_width(&left) + 2 + cell_width(right) > u)
    {
        ""
    } else {
        right
    };
    let left = block::ellipsis(&left, u.saturating_sub(cell_width(right) + 2));
    let pad = u.saturating_sub(cell_width(&left) + cell_width(right));
    out.push(block::padded(
        vec![
            Span::styled(left, Style::new().fg(p::FAINT)),
            Span::raw(" ".repeat(pad)),
            Span::styled(right, Style::new().fg(p::FAINT)),
        ],
        width,
        p::BLOCK,
    ));
    Layout {
        lines: out,
        cursor,
        scroll: shown.first().map_or((0, 0), |r| (r.line, r.index)),
        room,
        rows: shown.to_vec(),
        first_row,
    }
}
