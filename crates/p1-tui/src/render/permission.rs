//! The permission prompt (SPEC §4.5): the command echoed on a BLOCK+ line,
//! then label/value rows (`cwd`, `sandbox`, `network`, `reason`). Destructive
//! commands re-prompt every call and are not grantable: `a`/`p` stay visible
//! but FAINT with the reason inline.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::grid;
use crate::palette;
use crate::render::block::{DecisionOption, InlineApproval};

use super::{FLOOR_REASON, decision_key, fill};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PermissionView {
    /// The exact command being asked about, echoed as it will run.
    pub command: String,
    /// Label/value rows in display order.
    pub rows: Vec<(String, String)>,
    /// The destructive floor: `a`/`p` grey out with the reason inline.
    pub grantable: bool,
}

pub fn inline_approval(view: &PermissionView, pending: Option<(usize, usize)>) -> InlineApproval {
    let mut options = vec![DecisionOption {
        key: "y".into(),
        label: "allow once".into(),
        unavailable: None,
    }];
    for (key, label) in [("a", "session"), ("p", "project")] {
        let unavailable = if !view.grantable {
            Some(FLOOR_REASON.to_string())
        } else if key == "p" {
            Some("not available — no trust store yet".into())
        } else {
            None
        };
        options.push(DecisionOption {
            key: key.into(),
            label: label.into(),
            unavailable,
        });
    }
    options.push(DecisionOption {
        key: "n".into(),
        label: "deny".into(),
        unavailable: None,
    });
    let mut hints = Vec::new();
    if let Some((current, total)) = pending
        && total > 1
    {
        hints.push(format!("{current} of {total} pending"));
    }
    InlineApproval {
        permission_rows: view
            .rows
            .iter()
            .map(|(k, v)| (k.clone(), v.clone(), k == "cwd"))
            .collect(),
        diff: false,
        options,
        hints,
    }
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
    let mut first = Vec::new();
    decision_key(&mut first, "y", "allow once", true);
    if grantable {
        decision_key(&mut first, "a", "session", true);
        decision_key(&mut first, "p", "project", true);
    }
    decision_key(&mut first, "n", "deny", true);
    let mut out = vec![Line::from(first)];
    if !grantable {
        for (k, label) in [("a", "session"), ("p", "project")] {
            let mut spans = Vec::new();
            decision_key(&mut spans, k, label, false);
            spans.push(Span::styled(FLOOR_REASON, Style::new().fg(palette::FAINT)));
            out.push(Line::from(spans));
        }
    }
    out
}

/// Plain UI data; no runtime or tool types cross into the view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UserQuestion {
    pub question: String,
    pub header: String,
    pub options: Vec<(String, String, Option<String>)>,
    pub multi_select: bool,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionView {
    pub worker: Option<String>,
    pub questions: Vec<UserQuestion>,
    pub at: usize,
    pub selected: Vec<usize>,
    pub text: String,
    pub answers: Vec<(Vec<String>, Option<String>)>,
}
impl QuestionView {
    pub fn new(worker: Option<String>, questions: Vec<UserQuestion>) -> Self {
        Self {
            worker,
            questions,
            at: 0,
            selected: Vec::new(),
            text: String::new(),
            answers: Vec::new(),
        }
    }
    pub fn select(&mut self, index: usize) {
        let q = &self.questions[self.at];
        if index >= q.options.len() {
            return;
        }
        if q.multi_select {
            if self.selected.contains(&index) {
                self.selected.retain(|&i| i != index);
            } else {
                self.selected.push(index);
            }
        } else {
            self.selected = vec![index];
        }
    }
    pub fn advance(&mut self) -> bool {
        if self.selected.is_empty() && self.text.trim().is_empty() {
            return false;
        }
        let q = &self.questions[self.at];
        let chosen = q
            .options
            .iter()
            .enumerate()
            .filter(|(i, _)| self.selected.contains(i))
            .map(|(_, o)| o.0.clone())
            .collect();
        self.answers.push((
            chosen,
            (!self.text.trim().is_empty()).then(|| self.text.clone()),
        ));
        if self.at + 1 == self.questions.len() {
            return true;
        }
        self.at += 1;
        self.selected.clear();
        self.text.clear();
        false
    }
}
pub fn inline_questions(view: &QuestionView) -> InlineApproval {
    let q = &view.questions[view.at];
    let mut rows = vec![(q.header.clone(), q.question.clone(), false)];
    if let Some(worker) = &view.worker {
        rows.insert(0, ("worker".into(), worker.clone(), false));
    }
    for (i, (label, description, preview)) in q.options.iter().enumerate() {
        rows.push((
            format!(
                "{} {}",
                i + 1,
                if view.selected.contains(&i) {
                    "[x]"
                } else {
                    "[ ]"
                }
            ),
            format!("{label} — {description}"),
            false,
        ));
        if let Some(preview) = preview {
            rows.push(("preview".into(), preview.clone(), false));
        }
    }
    rows.push(("Other".into(), view.text.clone(), false));
    InlineApproval {
        permission_rows: rows,
        diff: false,
        options: vec![
            DecisionOption {
                key: "enter".into(),
                label: "answer / next".into(),
                unavailable: None,
            },
            DecisionOption {
                key: "esc".into(),
                label: "cancel".into(),
                unavailable: None,
            },
        ],
        hints: vec![format!(
            "{} of {} · numbers choose{}, type free text (Alt+digit starts numeric text)",
            view.at + 1,
            view.questions.len(),
            if q.multi_select { " multiple" } else { "" }
        )],
    }
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
