//! The statusline (BLOCK §2.1): one BLOCK+ row under everything, host state
//! only. Left: the route chip (inverted), repo INK, branch DIM, `effort` DIM +
//! value INK. Right: `ctx`, `spend`, the clock and the task diff — labels DIM,
//! values INK. Segments are separated by three spaces, never a bar. As width
//! shrinks the least-needed facts go first; the route chip is cut last.
use super::block;
use crate::{palette as p, state::Screen, wrap::cell_width};
use ratatui::{
    style::Style,
    text::{Line, Span},
};

fn spans_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| cell_width(&s.content)).sum()
}

pub fn line(screen: &Screen, width: usize, now: u64) -> Line<'static> {
    let dim = Style::new().fg(p::DIM);
    let ink = Style::new().fg(p::INK);
    let cost = if screen.spend.responses == 0 {
        None
    } else {
        screen.spend.cost_micro_usd
    };
    let cost = cost
        .map(|n| format!("${:.2}", n as f64 / 1_000_000.0))
        .unwrap_or_else(|| "—".into());
    let ctx = screen
        .context_view
        .as_ref()
        .filter(|c| c.window > 0)
        .map(|c| format!("{}%", c.used.saturating_mul(100) / c.window))
        .unwrap_or_else(|| "—".into());
    // Unknown is never zero: a resumed session without rebuilt task stats shows `—`.
    let diff = screen
        .task_view
        .as_ref()
        .and_then(|t| t.diff)
        .map(|(a, b)| format!("+{a} −{b}"))
        .unwrap_or_else(|| "—".into());
    let clock = format!("{}h{:02}", now / 3_600_000, now / 60_000 % 60);
    // Right-hand facts in the order they are dropped when room runs out.
    let facts: Vec<Vec<Span<'static>>> = vec![
        vec![Span::styled("ctx ", dim), Span::styled(ctx, ink)],
        vec![Span::styled("spend ", dim), Span::styled(cost, ink)],
        vec![Span::styled(clock, ink)],
        vec![Span::styled(diff, ink)],
    ];
    let route = if screen.model.is_empty() {
        &screen.route
    } else {
        &screen.model
    };
    let route = if route.is_empty() { "p1" } else { route };
    let effort = if screen.effort.is_empty() {
        "—"
    } else {
        &screen.effort
    };
    // Inner width: one column of padding at each end.
    let u = width.saturating_sub(2);
    // What is kept longest: the whole route chip, then `ask` (decisions will
    // be asked for), the effort, and the right-hand facts; repo and branch
    // only fill what is left.
    let ask_spans = if screen.asking {
        vec![Span::styled("   ask", ink)]
    } else {
        vec![]
    };
    let effort_spans = vec![
        Span::styled("   effort ", dim),
        Span::styled(effort.to_owned(), ink),
    ];
    let effort_width = spans_width(&effort_spans);
    let chip = cell_width(route) + 2;
    let mut keep = facts.len();
    let right = |keep: usize| -> Vec<Span<'static>> {
        let mut out = vec![];
        for (n, fact) in facts.iter().take(keep).enumerate() {
            if n > 0 {
                out.push(Span::raw("   "));
            }
            out.extend(fact.iter().cloned());
        }
        out
    };
    // Drop the diff first, then the clock, then spend; ctx goes last.
    while keep > 0
        && chip + spans_width(&ask_spans) + effort_width + 3 + spans_width(&right(keep)) > u
    {
        keep -= 1;
    }
    let right = right(keep);
    let room = u.saturating_sub(spans_width(&right) + if right.is_empty() { 0 } else { 3 });
    let route = block::ellipsis(
        route,
        room.saturating_sub(2 + spans_width(&ask_spans)).max(1),
    );
    let mut left = vec![
        Span::raw(" "),
        Span::styled(format!(" {route} "), Style::new().fg(p::GROUND).bg(p::INK)),
    ];
    let mut used = cell_width(&route) + 2;
    let mut push = |spans: Vec<Span<'static>>, left: &mut Vec<Span<'static>>| {
        let w = spans_width(&spans);
        if used + w <= room {
            used += w;
            left.extend(spans);
            true
        } else {
            false
        }
    };
    push(ask_spans, &mut left);
    if !screen.repo.is_empty() {
        let mut repo = vec![Span::styled(format!("   {}", screen.repo), ink)];
        if !screen.branch.is_empty() {
            repo.push(Span::styled(format!(" {}", screen.branch), dim));
        }
        let only_repo = vec![Span::styled(format!("   {}", screen.repo), ink)];
        for candidate in [repo, only_repo] {
            if spans_width(&left) - 1 + spans_width(&candidate) + effort_width <= room {
                push(candidate, &mut left);
                break;
            }
        }
    }
    push(effort_spans, &mut left);
    let used_left = spans_width(&left);
    let mut spans = left;
    spans.push(Span::raw(
        " ".repeat(width.saturating_sub(used_left + spans_width(&right) + 1)),
    ));
    spans.extend(right);
    block::band(spans, width, p::BLOCK_PLUS)
}
