//! The permission prompt (SPEC §4.5): the command echoed on a BLOCK+ line,
//! then label/value rows (`cwd`, `sandbox`, `network`, `reason`). Destructive
//! commands re-prompt every call and are not grantable: `a`/`p` stay visible
//! but FAINT with the reason inline.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::grid;
use crate::palette;

use super::fill;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionView {
    /// The tool asking (`shell`, `worker_start`…): the header names it.
    pub tool: String,
    /// The exact command being asked about, echoed as it will run.
    pub command: String,
    /// Label/value rows in display order.
    pub rows: Vec<(String, String)>,
    /// The destructive floor: `a`/`p` grey out with the reason inline.
    pub grantable: bool,
}

pub fn lines(view: &PermissionView, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    out.push(fill(
        Line::styled(format!("  {}", view.command), Style::new().fg(palette::INK)),
        width,
        palette::BLOCK_PLUS,
    ));
    out.push(Line::default());
    for (label, value) in &view.rows {
        out.push(grid::row(width.min(60), &format!("  {label}"), value));
    }
    out.push(Line::default());
    out.extend(decision_lines(view.grantable));
    out
}

/// `y  allow once     a  session     p  project     n  deny` on one line; with
/// the destructive floor the grant keys move to their OWN lines, greyed, with
/// the reason inline (SPEC §4.5's layout).
fn decision_lines(grantable: bool) -> Vec<Line<'static>> {
    fn key(spans: &mut Vec<Span<'static>>, k: &str, label: &str, available: bool) {
        let key_fg = if available {
            palette::INK
        } else {
            palette::FAINT
        };
        spans.push(Span::styled(format!(" {k}  "), Style::new().fg(key_fg)));
        spans.push(Span::styled(
            format!("{label}     "),
            Style::new().fg(if available {
                palette::DIM
            } else {
                palette::FAINT
            }),
        ));
    }
    let mut first = Vec::new();
    key(&mut first, "y", "allow once", true);
    if grantable {
        key(&mut first, "a", "session", true);
        key(&mut first, "p", "project", true);
    }
    key(&mut first, "n", "deny", true);
    let mut out = vec![Line::from(first)];
    if !grantable {
        for (k, label) in [("a", "session"), ("p", "project")] {
            let mut spans = Vec::new();
            key(&mut spans, k, label, false);
            spans.push(Span::styled(
                "not grantable — destructive floor",
                Style::new().fg(palette::FAINT),
            ));
            out.push(Line::from(spans));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    fn grid_line(label: &str, value: &str) -> String {
        let pad = 60 - label.chars().count() - value.chars().count();
        format!("{label}{}{value}", " ".repeat(pad))
    }

    #[test]
    fn the_prompt_echoes_the_command_on_block_plus() {
        let view = PermissionView {
            tool: "shell".into(),
            command: "rm -rf target/".into(),
            rows: vec![
                ("cwd".into(), "~/dev/phaseone".into()),
                ("sandbox".into(), "bubblewrap · writes: workspace".into()),
                ("network".into(), "off".into()),
                ("reason".into(), "destructive floor".into()),
            ],
            grantable: false,
        };
        let lines = lines(&view, 100);
        assert_eq!(lines[0].spans[0].style.bg, Some(palette::BLOCK_PLUS));
        let text: Vec<String> = lines
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        assert_eq!(text[2], grid_line("  cwd", "~/dev/phaseone"));
        // y and n share the decision line; the greyed grants sit on their own
        // lines below it, reason inline.
        let decision = &text[text.len() - 3];
        assert!(decision.contains("y  allow once"));
        assert!(decision.contains("n  deny"));
        assert!(!decision.contains("session"));
        assert!(text[text.len() - 2].contains("not grantable — destructive floor"));
        assert!(text[text.len() - 1].contains("not grantable — destructive floor"));
        let grant_line = &lines[lines.len() - 2];
        assert_eq!(grant_line.spans[0].style.fg, Some(palette::FAINT));
    }
}
