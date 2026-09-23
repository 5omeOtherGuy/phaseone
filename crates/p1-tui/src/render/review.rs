//! The full diff review (handoff §7.5, second half): the blocking view that owns the screen from
//! the top row to the statusline gap at `W − 4`, pane and composer hidden. A BLOCK+ header, a
//! dim summary row, the scrolling diff body, then the decision band pinned to the bottom (a tall
//! diff never pushes the keys off screen) and a faint hint row. `tab`/`⇧tab` page the call's
//! files for viewing only — the decision is always about the whole call.

use ratatui::style::Color;
use ratatui::text::Line;

use crate::band::{Band, Seg, truncate_path_middle};
use crate::palette;
use crate::render::diff::{DiffRow, DiffView};
use crate::state::FullReview;
use crate::wrap::cell_width;

/// The tool-name field (handoff §13 #6): 10 cells, never cut; longer names take `len + 1`.
const NAME_FIELD: usize = 10;
/// Header, summary and the blank row under it.
const HEAD_ROWS: usize = 3;
/// The hint row under the decision band.
const HINT_ROWS: usize = 1;

/// One file of the call under review.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewFile {
    pub view: DiffView,
    /// Lines added / removed in the whole file, which may be more than the rows shown.
    pub added: usize,
    pub removed: usize,
}

impl ReviewFile {
    /// Counts taken from the rows themselves, for a view that shows the whole change.
    pub fn new(view: DiffView) -> Self {
        let added = view
            .rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Add { .. }))
            .count();
        let removed = view
            .rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Del { .. }))
            .count();
        Self {
            view,
            added,
            removed,
        }
    }
}

/// One decision key. An unavailable key with a reason leaves the band and gets its own faint
/// row with the reason under it; without a reason it stays in the band, faint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionKey {
    pub key: char,
    pub label: String,
    pub available: bool,
    pub reason: Option<String>,
}

/// The decision band's keys and its faint right-hand facts (`all 3 files`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub keys: Vec<DecisionKey>,
    pub secondary: Vec<String>,
}

impl Decision {
    /// The approval decision for one call: `y a p n`. The destructive floor takes `a` away;
    /// `p` has no trust store to write to yet (handoff §13, proposal §14.8).
    pub fn for_call(grantable: bool, files: usize) -> Self {
        let key = |key, label: &str, available: bool, reason: Option<&str>| DecisionKey {
            key,
            label: label.into(),
            available,
            reason: reason.map(str::to_string),
        };
        Self {
            keys: vec![
                key('y', "allow once", true, None),
                key(
                    'a',
                    "session",
                    grantable,
                    (!grantable).then_some("not grantable — destructive floor"),
                ),
                key('p', "project", false, None),
                key('n', "deny", true, None),
            ],
            secondary: if files > 1 {
                vec![format!("all {files} files")]
            } else {
                vec![]
            },
        }
    }
}

/// The decision band (BLOCK+) and one faint BLOCK row per unavailable key that has a reason.
pub fn decision_rows(decision: &Decision, width: usize) -> Vec<Line<'static>> {
    let mut left = Vec::new();
    for key in decision.keys.iter().filter(|k| k.reason.is_none()) {
        if !left.is_empty() {
            left.push(Seg::new(palette::INK, "   "));
        }
        let cap = format!(" {} ", key.key);
        if key.available {
            left.push(Seg {
                fg: palette::ON_FILL,
                bg: Some(palette::AMBER_FILL),
                text: cap,
            });
            left.push(Seg::new(palette::INK, format!(" {}", key.label)));
        } else {
            left.push(Seg::new(palette::FAINT, cap));
            left.push(Seg::new(palette::FAINT, format!(" {}", key.label)));
        }
    }
    let mut out = vec![band(
        palette::BLOCK_PLUS,
        left,
        faint_right(&decision.secondary.join("   ")),
        width,
    )];
    for key in &decision.keys {
        if let Some(reason) = &key.reason {
            out.push(band(
                palette::BLOCK,
                vec![Seg::new(
                    palette::FAINT,
                    format!(" {}   {} {reason}", key.key, field(&key.label, NAME_FIELD)),
                )],
                vec![],
                width,
            ));
        }
    }
    out
}

/// Diff body rows the review has room for at `height` rows.
pub fn body_rows(decision: &Decision, height: usize) -> usize {
    let reason_rows = decision.keys.iter().filter(|k| k.reason.is_some()).count();
    height.saturating_sub(HEAD_ROWS + HINT_ROWS + 1 + reason_rows)
}

/// Whether a diff opens the full review by itself: it is taller than the transcript area.
pub fn opens_itself(diff_rows: usize, transcript_rows: usize) -> bool {
    diff_rows > transcript_rows
}

/// The whole review, exactly `height` rows of `width` cells (fewer only when `files` is empty).
pub fn lines(
    files: &[ReviewFile],
    review: &FullReview,
    decision: &Decision,
    width: usize,
    height: usize,
) -> Vec<Line<'static>> {
    let Some(file) = files.get(review.file.min(files.len().saturating_sub(1))) else {
        return Vec::new();
    };
    let view = &file.view;
    let position = format!(
        "{} of {} files",
        review.file.min(files.len() - 1) + 1,
        files.len()
    );
    let name = field(&view.tool, NAME_FIELD);
    // The path is pre-cut in its middle so the file name survives (handoff §5 exception).
    let path_room = width.saturating_sub(4 + 2 + cell_width(&name) + cell_width(&position) + 2);
    let mut out = vec![
        band(
            palette::BLOCK_PLUS,
            vec![
                Seg::new(palette::ATTN, "! "),
                Seg::new(palette::DIM, name),
                Seg::new(palette::REF, truncate_path_middle(&view.file, path_room)),
            ],
            vec![Seg::new(palette::INK, position)],
            width,
        ),
        band(
            palette::BLOCK,
            vec![Seg::new(palette::DIM, format!("  {}", view.summary))],
            vec![Seg::new(
                palette::DIM,
                format!("+{} −{}", file.added, file.removed),
            )],
            width,
        ),
        band(palette::BLOCK, vec![], vec![], width),
    ];
    let body = body_rows(decision, height);
    let scroll = review.scroll.min(view.rows.len().saturating_sub(body));
    let number_width = view
        .rows
        .iter()
        .map(|r| row_parts(r).0.to_string().len())
        .max()
        .unwrap_or(1);
    let shown = view.rows.iter().skip(scroll).take(body);
    let drawn = shown.len();
    for row in shown {
        out.push(diff_row(row, number_width, width));
    }
    for _ in drawn..body {
        out.push(band(palette::BLOCK, vec![], vec![], width));
    }
    out.extend(decision_rows(decision, width));
    let hints = if files.len() > 1 {
        "tab next file   ⇧tab previous   ^D back"
    } else {
        "^D back"
    };
    out.push(band(
        palette::BLOCK,
        vec![Seg::new(palette::FAINT, hints)],
        faint_right("PgUp PgDn scroll"),
        width,
    ));
    // Too short for every fixed row: the decision stays, the header goes first.
    let excess = out.len().saturating_sub(height);
    out.drain(..excess);
    out
}

/// (line number, marker, text, row surface, text ink).
fn row_parts(row: &DiffRow) -> (u32, char, &str, Color, Color) {
    match row {
        DiffRow::Context { line, text } => (*line, ' ', text, palette::BLOCK, palette::DIM),
        DiffRow::Add { line, text } => {
            (*line, '+', text, palette::DIFF_ADD_BG, palette::DIFF_ADD_FG)
        }
        DiffRow::Del { line, text } => {
            (*line, '−', text, palette::DIFF_DEL_BG, palette::DIFF_DEL_FG)
        }
    }
}

fn diff_row(row: &DiffRow, number_width: usize, width: usize) -> Line<'static> {
    let (line, marker, text, bg, fg) = row_parts(row);
    // Line 0 is an unnumbered row (the edit adapter could not anchor it).
    let number = if line == 0 {
        String::new()
    } else {
        line.to_string()
    };
    band(
        bg,
        vec![
            Seg::new(palette::FAINT, format!("{number:>number_width$}")),
            Seg::new(fg, format!("  {marker} ")),
            Seg::new(fg, text),
        ],
        vec![],
        width,
    )
}

fn band(bg: Color, left: Vec<Seg>, right: Vec<Seg>, width: usize) -> Line<'static> {
    Band {
        bg,
        left,
        right,
        width,
        pad: 2,
    }
    .render()
}

fn faint_right(text: &str) -> Vec<Seg> {
    if text.is_empty() {
        vec![]
    } else {
        vec![Seg::new(palette::FAINT, text)]
    }
}

/// `text` padded to `cells`, or followed by one space when it is that long already.
fn field(text: &str, cells: usize) -> String {
    let used = cell_width(text);
    format!("{text}{}", " ".repeat(cells.saturating_sub(used).max(1)))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(rows: usize) -> DiffView {
        DiffView {
            tool: "edit".into(),
            file: "crates/p1-context/src/edge.rs".into(),
            summary: "replace exact string · once".into(),
            position: (1, 1),
            rows: (0..rows)
                .map(|n| DiffRow::Add {
                    line: n as u32 + 1,
                    text: format!("line {n}"),
                })
                .collect(),
            grantable: true,
        }
    }

    #[test]
    fn the_decision_is_pinned_and_the_body_scrolls() {
        let files = [ReviewFile::new(view(40))];
        let decision = Decision::for_call(true, 1);
        let mut review = FullReview::default();
        let text: Vec<String> = lines(&files, &review, &decision, 76, 20)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert_eq!(text.len(), 20);
        assert!(text[3].contains(" 1  + line 0"), "{}", text[3]);
        // The `p` key stays faint in the decision band; only the hint follows it.
        assert!(text[18].contains(" y  allow once"));
        assert!(text[18].contains("p  project"));
        assert!(text[19].contains("^D back"));
        review.scroll = 1_000;
        let text: Vec<String> = lines(&files, &review, &decision, 76, 20)
            .iter()
            .map(|l| l.to_string())
            .collect();
        // Scrolled to the end: the last diff row sits right above the decision rows.
        let body = body_rows(&decision, 20);
        assert!(text[3 + body - 1].contains("+ line 39"));
        assert!(text[18].contains(" y  allow once"));
    }

    #[test]
    fn the_destructive_floor_gives_a_its_own_reason_row() {
        let decision = Decision::for_call(false, 3);
        let rows: Vec<String> = decision_rows(&decision, 76)
            .iter()
            .map(|l| l.to_string())
            .collect();
        assert!(!rows[0].contains(" a  session"), "{}", rows[0]);
        assert!(rows[0].contains("p  project"), "{}", rows[0]);
        assert!(rows[0].trim_end().ends_with("all 3 files"));
        assert!(rows[1].starts_with("   a   session    not grantable — destructive floor"));
    }

    #[test]
    fn a_long_path_keeps_its_file_name() {
        let mut v = view(1);
        v.file = "crates/some/very/deep/directory/tree/that/goes/on/src/edge.rs".into();
        let header = lines(
            &[ReviewFile::new(v)],
            &FullReview::default(),
            &Decision::for_call(true, 1),
            60,
            10,
        )[0]
        .to_string();
        assert_eq!(cell_width(&header), 60);
        assert!(header.contains("…/edge.rs"), "{header}");
        assert!(header.contains("1 of 1 files"));
    }
}
