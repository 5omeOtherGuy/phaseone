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
    out.push(decision_line(view.grantable));
    out
}

/// `y  allow once     a  session     p  project     n  deny` — with the
/// destructive floor, the grant keys grey out and carry their reason.
fn decision_line(grantable: bool) -> Line<'static> {
    let mut spans = Vec::new();
    let mut key = |k: &str, label: &str, available: bool, reason: Option<&str>| {
        let key_fg = if available { palette::INK } else { palette::FAINT };
        spans.push(Span::styled(format!(" {k}  "), Style::new().fg(key_fg)));
        let text = match reason {
            Some(reason) => format!("{label}      {reason}"),
            None => label.to_string(),
        };
        spans.push(Span::styled(
            format!("{text}     "),
            Style::new().fg(if available { palette::DIM } else { palette::FAINT }),
        ));
    };
    key("y", "allow once", true, None);
    let floor = (!grantable).then_some("not grantable — destructive floor");
    key("a", "session", grantable, floor);
    key("p", "project", grantable, floor);
    key("n", "deny", true, None);
    Line::from(spans)
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
        let text: Vec<String> = lines.iter().map(|l| Text::from(l.clone()).to_string()).collect();
        assert_eq!(text[2], grid_line("  cwd", "~/dev/phaseone"));
        let last = &text[text.len() - 1];
        assert!(last.contains("not grantable — destructive floor"));
        // y and n stay live; a and p grey out.
        let decision = &lines[lines.len() - 1];
        assert_eq!(decision.spans[0].style.fg, Some(palette::INK));
        assert_eq!(decision.spans[2].style.fg, Some(palette::FAINT));
    }
}
