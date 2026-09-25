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
    /// A patch touching several files: where each file's rows start and how
    /// it is named (`src/new.rs (new)`, `src/old.rs (deleted)`). Empty for a
    /// one-file change.
    pub files: Vec<(usize, String)>,
    /// A fact the review must not hide: the call will fail as asked (its old
    /// text is not in the file, or matches more than one place).
    pub note: Option<String>,
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
        let mut rows = Vec::new();
        let found = current.and_then(|text| {
            text.find(old)
                .filter(|_| !old.is_empty())
                .map(|at| (text, at))
        });
        if let Some((text, at)) = found {
            let start = text[..at].rfind('\n').map(|n| n + 1).unwrap_or(0);
            // An `old` that ends in a newline ends its last line: the next
            // line is untouched context, not a removed-and-readded row.
            let end = if old.ends_with('\n') {
                at + old.len() - 1
            } else {
                text[at + old.len()..]
                    .find('\n')
                    .map(|n| at + old.len() + n)
                    .unwrap_or(text.len())
            };
            let line = text[..start].bytes().filter(|b| *b == b'\n').count() as u32 + 1;
            if start > 0 {
                let previous = text[..start]
                    .trim_end_matches('\n')
                    .rsplit('\n')
                    .next()
                    .unwrap_or("");
                rows.push(DiffRow::Context {
                    line: line - 1,
                    text: previous.to_owned(),
                });
            }
            for (i, part) in text[start..end].lines().enumerate() {
                rows.push(DiffRow::Del {
                    line: line + i as u32,
                    text: part.to_owned(),
                });
            }
            let tail = text.get(at + old.len()..end).unwrap_or("");
            let replacement = format!("{}{new}{tail}", &text[start..at]);
            for (i, part) in replacement.lines().enumerate() {
                rows.push(DiffRow::Add {
                    line: line + i as u32,
                    text: part.to_owned(),
                });
            }
            if let Some(rest) = text[end..].strip_prefix('\n')
                && let Some(next) = rest.lines().next()
            {
                rows.push(DiffRow::Context {
                    line: line + replacement.lines().count() as u32,
                    text: next.to_owned(),
                });
            }
        } else {
            for text in old.lines() {
                rows.push(DiffRow::Del {
                    line: 0,
                    text: text.to_owned(),
                });
            }
            for text in new.lines() {
                rows.push(DiffRow::Add {
                    line: 0,
                    text: text.to_owned(),
                });
            }
        }
        Self {
            tool: tool.to_owned(),
            file: path.to_owned(),
            summary: "replace exact string".into(),
            position,
            rows,
            grantable: true,
            files: vec![],
            note: None,
        }
    }

    /// Review a `write`: a new file is all additions, numbered from 1; an
    /// existing file shows what the write removes as well as what it adds.
    pub fn from_write(tool: &str, path: &str, content: &str, current: Option<&str>) -> Self {
        let rows = match current {
            None => content
                .lines()
                .enumerate()
                .map(|(i, text)| DiffRow::Add {
                    line: i as u32 + 1,
                    text: text.to_owned(),
                })
                .collect(),
            Some(current) => line_diff(current, content),
        };
        Self {
            tool: tool.to_owned(),
            file: path.to_owned(),
            summary: if current.is_some() {
                "replace file".into()
            } else {
                "new file".into()
            },
            position: (1, 1),
            rows,
            grantable: true,
            files: vec![],
            note: None,
        }
    }

    /// Review a unified-diff `patch`: its own `+`/`-`/context lines, numbered
    /// from the hunk headers. The first file named is the header's path.
    pub fn from_patch(tool: &str, patch: &str) -> Self {
        let parsed = parse_patch(patch);
        let first = parsed
            .files
            .first()
            .map(|f| f.path.clone())
            .unwrap_or_default();
        let files = if parsed.files.len() > 1
            || parsed
                .files
                .iter()
                .any(|f| f.op != FileOp::Update || f.moved_from.is_some())
        {
            parsed
                .files
                .iter()
                .map(|f| (f.first_row, f.label()))
                .collect()
        } else {
            vec![]
        };
        Self {
            tool: tool.to_owned(),
            file: first,
            summary: "apply patch".into(),
            position: (1, parsed.files.len().max(1)),
            rows: parsed.rows,
            grantable: true,
            files,
            note: None,
        }
    }

    /// `(added, removed)` rows: the same count the settled block shows.
    pub fn counts(&self) -> (usize, usize) {
        let added = self
            .rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Add { .. }))
            .count();
        let removed = self
            .rows
            .iter()
            .filter(|r| matches!(r, DiffRow::Del { .. }))
            .count();
        (added, removed)
    }
}

/// A line diff of `old` → `new` as review rows: common head and tail trimmed,
/// the middle by longest common subsequence (bounded), one line of context
/// around each change. Removals come before additions within a change.
fn line_diff(old: &str, new: &str) -> Vec<DiffRow> {
    let a: Vec<&str> = old.lines().collect();
    let b: Vec<&str> = new.lines().collect();
    let head = a.iter().zip(&b).take_while(|(x, y)| x == y).count();
    let tail = a[head..]
        .iter()
        .rev()
        .zip(b[head..].iter().rev())
        .take_while(|(x, y)| x == y)
        .count();
    let (am, bm) = (&a[head..a.len() - tail], &b[head..b.len() - tail]);
    // Ops over the middle: 0 keep, 1 delete, 2 add.
    let mut ops: Vec<(u8, usize, usize)> = vec![];
    if am.len() * bm.len() <= 4_000_000 {
        let mut lcs = vec![vec![0u32; bm.len() + 1]; am.len() + 1];
        for i in (0..am.len()).rev() {
            for j in (0..bm.len()).rev() {
                lcs[i][j] = if am[i] == bm[j] {
                    lcs[i + 1][j + 1] + 1
                } else {
                    lcs[i + 1][j].max(lcs[i][j + 1])
                };
            }
        }
        let (mut i, mut j) = (0, 0);
        while i < am.len() || j < bm.len() {
            if i < am.len() && j < bm.len() && am[i] == bm[j] {
                ops.push((0, i, j));
                i += 1;
                j += 1;
            } else if j == bm.len() || (i < am.len() && lcs[i + 1][j] >= lcs[i][j + 1]) {
                ops.push((1, i, j));
                i += 1;
            } else {
                ops.push((2, i, j));
                j += 1;
            }
        }
    } else {
        ops.extend((0..am.len()).map(|i| (1, i, 0)));
        ops.extend((0..bm.len()).map(|j| (2, am.len(), j)));
    }
    let mut rows = vec![];
    let changed: Vec<usize> = ops
        .iter()
        .enumerate()
        .filter(|(_, o)| o.0 != 0)
        .map(|(k, _)| k)
        .collect();
    if head > 0 && !changed.is_empty() {
        rows.push(DiffRow::Context {
            line: head as u32,
            text: a[head - 1].to_owned(),
        });
    }
    for (k, (op, i, j)) in ops.iter().enumerate() {
        match op {
            1 => rows.push(DiffRow::Del {
                line: (head + i + 1) as u32,
                text: am[*i].to_owned(),
            }),
            2 => rows.push(DiffRow::Add {
                line: (head + j + 1) as u32,
                text: bm[*j].to_owned(),
            }),
            // Kept lines appear only as the one row of context beside a change.
            _ if changed.iter().any(|c| c.abs_diff(k) == 1) => rows.push(DiffRow::Context {
                line: (head + j + 1) as u32,
                text: bm[*j].to_owned(),
            }),
            _ => {}
        }
    }
    if tail > 0 && !changed.is_empty() {
        rows.push(DiffRow::Context {
            line: (b.len() - tail + 1) as u32,
            text: b[b.len() - tail].to_owned(),
        });
    }
    rows
}

/// What a patch does to one file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileOp {
    Update,
    Add,
    Delete,
}

/// One file of a patch: its path (the new one after a move), what happens to
/// it, and the first of its rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PatchFile {
    pub path: String,
    pub op: FileOp,
    pub moved_from: Option<String>,
    pub first_row: usize,
}

impl PatchFile {
    fn label(&self) -> String {
        match (self.op, &self.moved_from) {
            (FileOp::Add, _) => format!("{} (new)", self.path),
            (FileOp::Delete, _) => format!("{} (deleted)", self.path),
            (FileOp::Update, Some(from)) => format!("{from} → {}", self.path),
            (FileOp::Update, None) => self.path.clone(),
        }
    }
}

/// A parsed patch: its rows in order and the files they belong to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Patch {
    pub rows: Vec<DiffRow>,
    pub files: Vec<PatchFile>,
}

/// Parse a unified diff or the V4A form `apply_patch` takes (`*** Update
/// File: …`). `---`/`+++` are file headers only between hunks: inside one
/// (counted from its `@@` header, or up to the next `***` marker in V4A) a
/// leading `-`/`+` is always a row, so deleting `-- note` is shown.
pub fn parse_patch(patch: &str) -> Patch {
    let mut out = Patch::default();
    let (mut old_line, mut new_line) = (0u32, 0u32);
    // Unified hunks count their rows; V4A sections run to the next marker.
    let mut remaining: Option<(u32, u32)> = None;
    let mut v4a = false;
    let mut pending_old: Option<String> = None;
    let start = |out: &mut Patch, path: &str, op: FileOp| {
        out.files.push(PatchFile {
            path: path.trim().to_owned(),
            op,
            moved_from: None,
            first_row: out.rows.len(),
        });
    };
    for text in patch.lines() {
        let in_hunk = v4a || remaining.is_some_and(|(o, n)| o > 0 || n > 0);
        let marker = [
            ("*** Update File: ", FileOp::Update),
            ("*** Add File: ", FileOp::Add),
            ("*** Delete File: ", FileOp::Delete),
        ]
        .iter()
        .find_map(|(p, op)| text.strip_prefix(p).map(|path| (path, *op)));
        if let Some((path, op)) = marker {
            start(&mut out, path, op);
            v4a = op != FileOp::Delete;
            remaining = None;
            (old_line, new_line) = (0, 0);
            continue;
        }
        if let Some(to) = text.strip_prefix("*** Move to: ") {
            if let Some(file) = out.files.last_mut() {
                file.moved_from = Some(std::mem::replace(&mut file.path, to.trim().to_owned()));
            }
            continue;
        }
        if text.starts_with("*** ") {
            // `*** Begin Patch`, `*** End Patch`, `*** End of File`.
            v4a = false;
            continue;
        }
        if !in_hunk {
            if text.starts_with("diff ") || text.starts_with("Index: ") {
                continue;
            }
            // `a/path\t2024-01-01 …`: one `a/` or `b/` prefix, no timestamp.
            let path = |p: &str, prefix: &str| {
                let p = p.split('\t').next().unwrap_or("").trim();
                p.strip_prefix(prefix).unwrap_or(p).to_owned()
            };
            if let Some(old) = text.strip_prefix("--- ") {
                pending_old = Some(path(old, "a/"));
                continue;
            }
            if let Some(new) = text.strip_prefix("+++ ") {
                let new = path(new, "b/");
                let new = new.as_str();
                let old = pending_old.take();
                match (old.as_deref(), new) {
                    (Some(old), "/dev/null") => start(&mut out, old, FileOp::Delete),
                    (Some("/dev/null"), new) => start(&mut out, new, FileOp::Add),
                    (_, new) => start(&mut out, new, FileOp::Update),
                }
                continue;
            }
        }
        if let Some(hunk) = text.strip_prefix("@@") {
            let range = |sign: char| {
                hunk.split_whitespace()
                    .find_map(|p| p.strip_prefix(sign))
                    .map(|p| {
                        let mut parts = p.split(',');
                        let at = parts
                            .next()
                            .and_then(|n| n.parse::<u32>().ok())
                            .unwrap_or(0);
                        let count = parts
                            .next()
                            .and_then(|n| n.parse::<u32>().ok())
                            .unwrap_or(1);
                        (at, count)
                    })
            };
            match (range('-'), range('+')) {
                (Some((old_at, old_n)), Some((new_at, new_n))) => {
                    (old_line, new_line) = (old_at, new_at);
                    if !v4a {
                        remaining = Some((old_n, new_n));
                    }
                }
                // A V4A `@@ context` line names where the change is, no numbers.
                _ => (old_line, new_line) = (0, 0),
            }
            if out.files.is_empty() {
                start(&mut out, "", FileOp::Update);
            }
            continue;
        }
        if text.starts_with('\\') {
            // `\ No newline at end of file`.
            continue;
        }
        // Outside a hunk only headers count: a ``` fence, a heredoc marker,
        // git's `index …` / `new file mode` lines are not rows of any file.
        // (A bare `+`/`-` snippet with no header at all is still a diff.)
        if !in_hunk && !v4a && !(out.files.is_empty() && text.starts_with(['+', '-', ' '])) {
            continue;
        }
        if out.files.is_empty() {
            start(&mut out, "", FileOp::Update);
        }
        let count = |remaining: &mut Option<(u32, u32)>, old: bool, new: bool| {
            if let Some((o, n)) = remaining {
                *o = o.saturating_sub(u32::from(old));
                *n = n.saturating_sub(u32::from(new));
            }
        };
        if let Some(t) = text.strip_prefix('+') {
            out.rows.push(DiffRow::Add {
                line: new_line,
                text: t.to_owned(),
            });
            new_line += u32::from(new_line > 0);
            count(&mut remaining, false, true);
        } else if let Some(t) = text.strip_prefix('-') {
            out.rows.push(DiffRow::Del {
                line: old_line,
                text: t.to_owned(),
            });
            old_line += u32::from(old_line > 0);
            count(&mut remaining, true, false);
        } else {
            out.rows.push(DiffRow::Context {
                line: new_line,
                text: text.strip_prefix(' ').unwrap_or(text).to_owned(),
            });
            old_line += u32::from(old_line > 0);
            new_line += u32::from(new_line > 0);
            count(&mut remaining, true, true);
        }
    }
    out
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
    out.extend(footer(view.grantable, view.position.1 > 1));
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
    let room = width.saturating_sub(LINE_NUM_WIDTH + 3); // 4 + space + marker + space
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
fn footer(grantable: bool, multi: bool) -> Vec<Line<'static>> {
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
    let second = if multi {
        Line::styled(
            "                            ^D next file   ^A all files",
            Style::new().fg(palette::FAINT),
        )
    } else {
        Line::styled(
            "                            ^A all files",
            Style::new().fg(palette::FAINT),
        )
    };
    vec![Line::from(first), second]
}

#[cfg(test)]
mod tests {
    #[test]
    fn a_v4a_patch_keeps_its_files_deletions_and_dash_lines_apart() {
        let patch = "*** Begin Patch\n*** Update File: db/schema.sql\n@@ create table\n-- legacy note\n+-- new note\n+create index i on t(id);\n*** Add File: src/new.rs\n+fn new() {}\n*** Delete File: src/old_important.rs\n*** End Patch\n";
        let view = DiffView::from_patch("apply_patch", patch);
        assert_eq!(view.file, "db/schema.sql");
        assert_eq!(view.position, (1, 3));
        assert_eq!(
            view.files,
            vec![
                (0, "db/schema.sql".to_owned()),
                (3, "src/new.rs (new)".to_owned()),
                (4, "src/old_important.rs (deleted)".to_owned()),
            ]
        );
        // `--- legacy`-shaped rows inside a section are rows, not headers.
        assert_eq!(
            view.rows[0],
            DiffRow::Del {
                line: 0,
                text: "- legacy note".into()
            }
        );
        assert_eq!(view.counts(), (3, 1));
    }

    #[test]
    fn wrappers_moves_and_git_headers_add_no_phantom_file() {
        let fenced =
            "```\n*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** End Patch\n```\n";
        let parsed = parse_patch(fenced);
        assert_eq!(parsed.files.len(), 1);
        assert_eq!(parsed.files[0].path, "src/a.rs");
        assert_eq!(parsed.rows.len(), 2);
        let git = "diff --git a/a/x b/a/x\nindex 83db48f..bf269f4 100644\n--- a/a/x\t2024-01-01\n+++ b/a/x\t2024-01-01\n@@ -1 +1 @@\n-1\n+2\ndiff --git a/y b/y\nnew file mode 100644\n--- /dev/null\n+++ b/y\n@@ -0,0 +1 @@\n+y\n";
        let parsed = parse_patch(git);
        let paths: Vec<_> = parsed
            .files
            .iter()
            .map(|f| (f.path.as_str(), f.op))
            .collect();
        assert_eq!(paths, [("a/x", FileOp::Update), ("y", FileOp::Add)]);
        assert_eq!(parsed.rows.len(), 3);
        let moved = DiffView::from_patch(
            "apply_patch",
            "*** Begin Patch\n*** Update File: src/old.rs\n*** Move to: src/new.rs\n*** End Patch\n",
        );
        assert_eq!(moved.files, vec![(0, "src/old.rs → src/new.rs".to_owned())]);
    }

    #[test]
    fn a_unified_diff_counts_its_hunk_so_a_deleted_dash_line_is_a_row() {
        let patch = "--- a/q.sql\n+++ b/q.sql\n@@ -1,2 +1,1 @@\n--- old comment\n select 1;\n--- a/r.txt\n+++ /dev/null\n@@ -1 +0,0 @@\n-gone\n";
        let parsed = parse_patch(patch);
        assert_eq!(parsed.files.len(), 2);
        assert_eq!(parsed.files[1].op, FileOp::Delete);
        assert_eq!(
            parsed.rows[0],
            DiffRow::Del {
                line: 1,
                text: "-- old comment".into()
            }
        );
        assert_eq!(parsed.rows.len(), 3);
    }

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
            files: vec![],
            note: None,
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
        assert!(matches!(view.rows[0], DiffRow::Context { line: 2, .. }));
        assert!(matches!(view.rows[1], DiffRow::Del { line: 3, .. }));
        assert!(matches!(view.rows[2], DiffRow::Add { line: 3, .. }));
        assert!(matches!(view.rows[3], DiffRow::Context { line: 4, .. }));
        // Unfindable anchors still review, unnumbered.
        let view = DiffView::from_edit("edit", "src/x.rs", "gone", "new", Some(current), (1, 1));
        assert!(matches!(view.rows[0], DiffRow::Del { line: 0, .. }));
    }

    #[test]
    fn an_edit_ending_in_a_newline_leaves_the_next_line_alone() {
        let current = "one\ntwo\nthree\nfour\n";
        let view = DiffView::from_edit("edit", "f", "two\nthree\n", "TWO\n", Some(current), (1, 1));
        assert_eq!(view.counts(), (1, 2));
        assert!(matches!(&view.rows[0], DiffRow::Context { line: 1, text } if text == "one"));
        assert!(
            matches!(view.rows.last(), Some(DiffRow::Context { line: 3, text }) if text == "four")
        );
    }

    #[test]
    fn overwriting_a_file_shows_what_goes_as_well_as_what_comes() {
        let view = DiffView::from_write("write", "f", "a\nB\nc\nd\n", Some("a\nb\nc\nd\n"));
        assert_eq!(view.counts(), (1, 1));
        assert!(matches!(&view.rows[1], DiffRow::Del { line: 2, text } if text == "b"));
        assert!(matches!(&view.rows[2], DiffRow::Add { line: 2, text } if text == "B"));
        let new = DiffView::from_write("write", "f", "x\ny\n", None);
        assert!(matches!(new.rows[1], DiffRow::Add { line: 2, .. }));
    }

    #[test]
    fn a_patch_reviews_its_own_hunks() {
        let view = DiffView::from_patch(
            "patch",
            "--- a/src/x.rs\n+++ b/src/x.rs\n@@ -3,2 +3,2 @@\n keep\n-old\n+new\n",
        );
        assert_eq!(view.file, "src/x.rs");
        assert_eq!(view.counts(), (1, 1));
        assert!(matches!(view.rows[1], DiffRow::Del { line: 4, .. }));
        assert!(matches!(view.rows[2], DiffRow::Add { line: 4, .. }));
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
