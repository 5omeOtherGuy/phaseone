//! Renderers: pure `state → Vec<Line>` functions. Each returns styled lines on
//! its own grid width; compositing into a `Buffer` (background fills, pane
//! placement) is the caller's one mechanical step. Keeping renderers line-
//! shaped makes every screen snapshot-testable against `TestBackend`.

pub mod composer;
pub mod diff;
pub mod ledger;
pub mod permission;
pub mod picker;
pub mod screen;
pub mod status;
pub mod transcript;

use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

/// Pad a line to exactly `width` cells with spaces on `bg`, so a background
/// block (fold output, peek banner) reads as one surface, not stripes.
pub fn fill(line: Line<'static>, width: usize, bg: Color) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| s.content.chars().count()).sum();
    let mut line = line;
    if used < width {
        line.spans
            .push(Span::styled(" ".repeat(width - used), Style::new().bg(bg)));
    }
    for span in &mut line.spans {
        span.style = span.style.bg(bg);
    }
    line
}

/// Milliseconds as the transcript shows them: `412ms` under a second, then
/// `11.4s`, then `2m10s`. One decimal under a minute; whole seconds after.
pub fn elapsed(ms: u64) -> String {
    if ms < 1_000 {
        format!("{ms}ms")
    } else if ms < 60_000 {
        format!("{:.1}s", ms as f64 / 1_000.0)
    } else {
        format!("{}m{:02}s", ms / 60_000, ms % 60_000 / 1_000)
    }
}

/// A token count as the ledger shows it: `846` under a thousand, one decimal
/// below 100k (`12.4k`), whole thousands at and above (`120k`, `200k`). The
/// SPEC mock-up mixed `120.0k` with `200k`; one rule wins over both.
pub fn tokens(count: u64) -> String {
    if count < 1_000 {
        count.to_string()
    } else if count < 100_000 {
        format!("{:.1}k", count as f64 / 1_000.0)
    } else {
        format!("{}k", count / 1_000)
    }
}

/// `—` for an unknown quantity (SPEC §5: unknown cost renders `—`, never 0).
pub const UNKNOWN: &str = "—";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn elapsed_scales() {
        assert_eq!(elapsed(412), "412ms");
        assert_eq!(elapsed(11_400), "11.4s");
        assert_eq!(elapsed(130_000), "2m10s");
    }

    #[test]
    fn tokens_scale() {
        assert_eq!(tokens(846), "846");
        assert_eq!(tokens(12_400), "12.4k");
        assert_eq!(tokens(120_000), "120k");
        assert_eq!(tokens(200_000), "200k");
    }
}
