//! The diff review (SPEC §4.4): blocking, full width. Line numbers FAINT,
//! context DIM, changed lines in the two reserved hues with a literal `+`/`-`
//! column. Decisions are single characters spelled out in the footer — never
//! a button row. The two hues appear NOWHERE else in the interface (SPEC §8).
//!
//! The renderer takes a prepared view: parsing a tool's raw input into rows is
//! a presentation adapter (the driver's job), not something the renderer
//! infers from wire formats.

use ratatui::style::Style;
use ratatui::text::{Line, Span};

use crate::glyphs;
use crate::palette;

/// One row of the diff body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DiffRow {
    Context { line: u32, text: String },
    Add { line: u32, text: String },
    Del { line: u32, text: String },
}

/// A prepared diff review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DiffView {
    pub tool: String,
    pub file: String,
    /// `replace exact string · once` — the call's own summary line.
    pub summary: String,
    /// (current, total) files in this approval.
    pub position: (usize, usize),
    pub rows: Vec<DiffRow>,
    /// The destructive floor (SPEC §4.5): session/project grants stay visible
    /// but FAINT with the reason inline when the call is not grantable.
    pub grantable: bool,
}

impl DiffView {
    /// Build the review for an `edit`-shaped call (JSON `{file_path,
    /// old_string, new_string}`). `current` is the file's content now; when
    /// the string is not found (the file moved on) the review still shows the
    /// change, unnumbered. Context is two lines around the change.
    ///
    /// This is a presentation adapter (seams: renderers never parse wire
    /// formats; the caller parses the tool's input and reads the file).
    pub fn from_edit(
        tool: &str,
        path: &str,
        old: &str,
        new: &str,
        current: Option<&str>,
        position: (usize, usize),
    ) -> Self {
        let old_lines: Vec<&str> = old.lines().collect();
        let new_lines: Vec<&str> = new.lines().collect();
        let current_lines: Vec<&str> = current.map(|c| c.lines().collect()).unwrap_or_default();
        // Locate old_string: exact match of the first line anchors the hunk.
        let anchor = old_lines.first().and_then(|first| {
            current_lines
                .iter()
                .position(|l| l.trim_end() == first.trim_end())
        });
        let mut rows = Vec::new();
        // Up to two context lines immediately before the anchor.
        if let Some(anchor) = anchor {
            let from = anchor.saturating_sub(2);
            for (n, line) in current_lines
                .iter()
                .enumerate()
                .skip(from)
                .take(anchor - from)
            {
                rows.push(DiffRow::Context {
                    line: n as u32 + 1,
                    text: line.to_string(),
                });
            }
        }
        let first_line = anchor.map(|a| a as u32 + 1).unwrap_or(0);
        for (n, line) in old_lines.iter().enumerate() {
            rows.push(DiffRow::Del {
                line: first_line + n as u32,
                text: line.to_string(),
            });
        }
        for (n, line) in new_lines.iter().enumerate() {
            rows.push(DiffRow::Add {
                line: first_line + n as u32,
                text: line.to_string(),
            });
        }
        Self {
            tool: tool.to_string(),
            file: path.to_string(),
            summary: format!("replace exact string · {} line(s)", old_lines.len()),
            position,
            rows,
            grantable: true,
        }
    }
}

const LINE_NUM_WIDTH: usize = 4;

/// The review fills the width; the pane is hidden while it is up (SPEC §4.4).
pub fn lines(view: &DiffView, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    // Header: the approval glyph is INK — it is the thing demanding attention.
    let header = format!("{} {:<10}{}", glyphs::APPROVAL, view.tool, view.file);
    let position = format!("{} of {} files", view.position.0, view.position.1);
    let pad = width.saturating_sub(header.chars().count() + position.chars().count());
    out.push(Line::from(vec![
        Span::styled(header, Style::new().fg(palette::INK)),
        Span::raw(" ".repeat(pad)),
        Span::styled(position, Style::new().fg(palette::INK)),
    ]));
    out.push(Line::styled(
        format!("  {}", view.summary),
        Style::new().fg(palette::DIM),
    ));
    out.push(Line::default());
    for row in &view.rows {
        out.push(diff_row(row, width));
    }
    out.push(Line::default());
    out.extend(footer(view.grantable));
    out
}

fn diff_row(row: &DiffRow, width: usize) -> Line<'static> {
    let (num, marker, text, fg, bg) = match row {
        DiffRow::Context { line, text } => {
            (*line, ' ', text.as_str(), palette::DIM, palette::GROUND)
        }
        DiffRow::Add { line, text } => (
            *line,
            '+',
            text.as_str(),
            palette::DIFF_ADD_FG,
            palette::DIFF_ADD_BG,
        ),
        DiffRow::Del { line, text } => (
            *line,
            '−',
            text.as_str(),
            palette::DIFF_DEL_FG,
            palette::DIFF_DEL_BG,
        ),
    };
    let room = width.saturating_sub(LINE_NUM_WIDTH + 2);
    let text: String = text.chars().take(room).collect();
    let pad = room.saturating_sub(text.chars().count());
    let style = Style::new().fg(fg).bg(bg);
    Line::from(vec![
        Span::styled(
            format!("{num:>LINE_NUM_WIDTH$} {marker} "),
            Style::new().fg(palette::FAINT).bg(bg),
        ),
        Span::styled(format!("{text}{}", " ".repeat(pad)), style),
    ])
}

/// The decision footer (SPEC §4.4): single characters, spelled out. The
/// destructive floor greys the grant rows with the reason inline (§4.5).
fn footer(grantable: bool) -> Vec<Line<'static>> {
    let key = |k: &str, label: &str, available: bool| {
        let fg = if available {
            palette::INK
        } else {
            palette::FAINT
        };
        vec![
            Span::styled(format!(" {k}  "), Style::new().fg(fg)),
            Span::styled(
                format!("{label}     "),
                Style::new().fg(if available {
                    palette::DIM
                } else {
                    palette::FAINT
                }),
            ),
        ]
    };
    let mut first = key("y", "allow once", true);
    if grantable {
        first.extend(key("a", "session", true));
        first.extend(key("p", "project", true));
    } else {
        first.extend(key(
            "a",
            "session      not grantable — destructive floor",
            false,
        ));
        first.extend(key(
            "p",
            "project      not grantable — destructive floor",
            false,
        ));
    }
    first.extend(key("n", "deny", true));
    let second = Line::styled(
        "                            ^D next file   ^A all files",
        Style::new().fg(palette::FAINT),
    );
    vec![Line::from(first), second]
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    fn view() -> DiffView {
        DiffView {
            tool: "edit".into(),
            file: "p1-context/src/edge.rs".into(),
            summary: "replace exact string · once".into(),
            position: (1, 3),
            rows: vec![
                DiffRow::Context {
                    line: 411,
                    text: "let pressure = self.pressure_at_edge();".into(),
                },
                DiffRow::Del {
                    line: 412,
                    text: "if pressure == Pressure::Hard {".into(),
                },
                DiffRow::Add {
                    line: 412,
                    text: "if let Some(summary) = ready {".into(),
                },
            ],
            grantable: true,
        }
    }

    #[test]
    fn the_review_layout_and_hues() {
        let lines = lines(&view(), 100);
        let text: Vec<String> = lines
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        assert!(text[0].starts_with("! edit      p1-context/src/edge.rs"));
        assert!(text[0].ends_with("1 of 3 files"));
        assert_eq!(text[2], "");
        assert_eq!(
            text[3].trim_end(),
            " 411   let pressure = self.pressure_at_edge();"
        );
        // The minus column is the UNICODE minus, and the hues sit on the row.
        assert_eq!(text[4].trim_end(), " 412 − if pressure == Pressure::Hard {");
        assert_eq!(text[5].trim_end(), " 412 + if let Some(summary) = ready {");
        assert_eq!(lines[4].spans[1].style.bg, Some(palette::DIFF_DEL_BG));
        assert_eq!(lines[5].spans[1].style.bg, Some(palette::DIFF_ADD_BG));
        assert_eq!(lines[3].spans[1].style.bg, Some(palette::GROUND));
        // Footer: single characters, spelled out.
        let footer = &text[text.len() - 2];
        assert!(footer.contains("y  allow once"));
        assert!(footer.contains("n  deny"));
    }

    #[test]
    fn the_edit_adapter_anchors_and_marks() {
        let current = "fn a() {}\nlet ready = worker.take_summary();\nlet old = true;\nfn b() {}\n";
        let view = DiffView::from_edit(
            "edit",
            "src/x.rs",
            "let old = true;",
            "let old = false;",
            Some(current),
            (1, 1),
        );
        assert!(matches!(view.rows[0], DiffRow::Context { line: 1, .. }));
        assert!(matches!(view.rows[1], DiffRow::Context { line: 2, .. }));
        assert!(matches!(view.rows[2], DiffRow::Del { line: 3, .. }));
        assert!(matches!(view.rows[3], DiffRow::Add { line: 3, .. }));
        // Unfindable anchors still review, unnumbered.
        let view = DiffView::from_edit("edit", "src/x.rs", "gone", "new", Some(current), (1, 1));
        assert!(matches!(view.rows[0], DiffRow::Del { line: 0, .. }));
    }

    #[test]
    fn the_destructive_floor_greys_grants_with_the_reason() {
        let mut v = view();
        v.grantable = false;
        let lines = lines(&v, 100);
        let footer = &lines[lines.len() - 2];
        let text: String = footer.spans.iter().map(|s| s.content.as_ref()).collect();
        assert!(text.contains("not grantable — destructive floor"));
        // The greyed keys are FAINT, the live keys INK.
        assert_eq!(footer.spans[0].style.fg, Some(palette::INK)); // y
        assert_eq!(footer.spans[2].style.fg, Some(palette::FAINT)); // a
    }
}
