//! Three-band tool-call blocks from handoff §7.

use ratatui::{
    style::Style,
    text::{Line, Span},
};

use crate::{
    band::{Band, Seg, truncate_path_middle},
    face::{FaceBody, TargetKind},
    fold::FoldId,
    glyphs, palette,
    transcript::{RowStatus, ToolRow},
    wrap::{cell_width, fit_cells},
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DecisionOption {
    pub key: String,
    pub label: String,
    pub unavailable: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InlineApproval {
    pub permission_rows: Vec<(String, String, bool)>,
    pub diff: bool,
    pub options: Vec<DecisionOption>,
    pub hints: Vec<String>,
}

fn seg(color: ratatui::style::Color, text: impl Into<String>) -> Seg {
    Seg::new(color, text)
}
fn full(line: Line<'static>, width: usize, bg: ratatui::style::Color) -> Line<'static> {
    let used: usize = line.spans.iter().map(|s| cell_width(&s.content)).sum();
    let mut spans = line.spans;
    if used < width {
        spans.push(Span::styled(" ".repeat(width - used), Style::new().bg(bg)));
    }
    for span in &mut spans {
        span.style = span.style.bg(bg);
    }
    Line::from(spans)
}

/// The three `▪` cells of a live element on `bg` (§3.4).
pub fn working_segments(bg: ratatui::style::Color, now_ms: u64, reduced_motion: bool) -> Vec<Seg> {
    (0..glyphs::WORKING_CELLS)
        .map(|cell| {
            let fg = glyphs::working_color(palette::LIVE, bg, cell, now_ms, reduced_motion);
            Seg::new(fg, glyphs::WORKING.to_string())
        })
        .collect()
}

/// A live elapsed time in whole tenths (`4.2s`, never rounded up), minutes
/// past one (`2m10s`); `—` before the clock is known.
pub fn live_elapsed(ms: Option<u64>) -> String {
    match ms {
        None => crate::render::UNKNOWN.into(),
        Some(ms) if ms < 60_000 => format!("{}.{}s", ms / 1_000, ms % 1_000 / 100),
        Some(ms) => crate::render::elapsed(ms),
    }
}

pub fn lines(
    row: &ToolRow,
    width: usize,
    short: bool,
    now_ms: u64,
    reduced_motion: bool,
) -> Vec<Line<'static>> {
    lines_with_approval(row, width, short, now_ms, reduced_motion, None)
}

pub fn lines_with_approval(
    row: &ToolRow,
    width: usize,
    short: bool,
    now_ms: u64,
    reduced_motion: bool,
    approval: Option<&InlineApproval>,
) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let (left_glyph, left_color, mut right) = match row.status {
        RowStatus::AwaitingApproval => (
            glyphs::APPROVAL,
            palette::ATTN,
            "! awaiting approval".into(),
        ),
        RowStatus::Running if row.name.is_empty() => (
            glyphs::TOOL,
            palette::DIM,
            row.result_face
                .as_ref()
                .and_then(|f| f.outcome.clone())
                .unwrap_or_default(),
        ),
        RowStatus::Running => (
            glyphs::TOOL,
            palette::LIVE,
            format!("{}  ▪▪▪", live_elapsed(row.elapsed_ms)),
        ),
        RowStatus::Settled(status) => {
            let (g, _c) = match status {
                p1_contracts::ToolStatus::Ok => (glyphs::DONE, palette::OK),
                p1_contracts::ToolStatus::Error
                | p1_contracts::ToolStatus::Denied
                | p1_contracts::ToolStatus::Unavailable => (glyphs::FAILED, palette::FAIL),
                p1_contracts::ToolStatus::Cancelled | p1_contracts::ToolStatus::Unknown => {
                    (glyphs::PENDING, palette::FAINT)
                }
            };
            let fact = row
                .result_face
                .as_ref()
                .and_then(|f| f.outcome.clone())
                .unwrap_or_else(|| match status {
                    p1_contracts::ToolStatus::Denied => "denied".into(),
                    p1_contracts::ToolStatus::Cancelled => "cancelled".into(),
                    p1_contracts::ToolStatus::Unknown => "unknown · turn ended".into(),
                    p1_contracts::ToolStatus::Unavailable => "unavailable · not assembled".into(),
                    _ => String::new(),
                });
            (
                glyphs::TOOL,
                palette::DIM,
                if fact.is_empty() {
                    String::new()
                } else {
                    format!("{g} {fact}")
                },
            )
        }
    };
    let awaiting = approval.is_some() || row.status == RowStatus::AwaitingApproval;
    if awaiting {
        right = if approval.is_some_and(|a| a.diff) {
            format!(
                "! {}",
                row.result_face
                    .as_ref()
                    .and_then(|f| f.outcome.clone())
                    .unwrap_or_default()
            )
        } else {
            "! awaiting approval".into()
        };
    }
    let (left_glyph, left_color) = if awaiting {
        (glyphs::APPROVAL, palette::ATTN)
    } else {
        (left_glyph, left_color)
    };
    let name = if row.name.is_empty() {
        "…".to_string()
    } else {
        row.name.clone()
    };
    let name_width = cell_width(&name).max(10) + usize::from(cell_width(&name) >= 10);
    let target_budget = width.saturating_sub(
        4 + 2 + name_width + cell_width(&right) + usize::from(!right.is_empty()) * 2,
    );
    let mut target = row.face.target.clone();
    if target.contains('\n') {
        target = target.replace('\n', "␤");
    }
    if row.face.kind == TargetKind::Path {
        target = truncate_path_middle(&target, target_budget);
    }
    let mut left = vec![
        seg(left_color, format!("{left_glyph} ")),
        seg(
            if row.name.is_empty() {
                palette::FAINT
            } else {
                palette::DIM
            },
            format!("{name:<name_width$}"),
        ),
        seg(
            if row.name.is_empty() {
                palette::DIM
            } else if row.face.kind == TargetKind::Path {
                palette::REF
            } else {
                palette::INK
            },
            target,
        ),
    ];
    let right_segments = if right.is_empty() {
        vec![]
    } else if matches!(row.status, RowStatus::Running)
        && approval.is_none()
        && !row.name.is_empty()
        && !awaiting
    {
        let mut live = vec![seg(
            palette::DIM,
            format!("{}  ", live_elapsed(row.elapsed_ms)),
        )];
        live.extend(working_segments(
            palette::BLOCK_PLUS,
            now_ms,
            reduced_motion,
        ));
        live
    } else {
        let (status_glyph, fact) = right.split_once(' ').unwrap_or((&right, ""));
        let color = match status_glyph {
            "✓" => palette::OK,
            "✗" => palette::FAIL,
            "·" => palette::FAINT,
            "!" => palette::ATTN,
            _ => palette::DIM,
        };
        vec![
            seg(color, status_glyph),
            seg(palette::DIM, format!(" {fact}")),
        ]
    };
    out.push(
        Band {
            bg: palette::BLOCK_PLUS,
            left: std::mem::take(&mut left),
            right: right_segments,
            width,
            pad: 2,
        }
        .render(),
    );

    let body = row.result_face.as_ref().map(|f| &f.body);
    if let Some(approval) = approval {
        for (label, value, reference) in &approval.permission_rows {
            out.push(full(
                Line::from(vec![
                    Span::raw("    "),
                    Span::styled(format!("{label:<10}"), Style::new().fg(palette::DIM)),
                    Span::styled(
                        value.clone(),
                        Style::new().fg(if *reference {
                            palette::REF
                        } else {
                            palette::INK
                        }),
                    ),
                ]),
                width,
                palette::BLOCK,
            ));
        }
    }
    let mut body_lines: Vec<(String, ratatui::style::Color)> = if approval.is_some() {
        Vec::new()
    } else {
        match body {
            Some(FaceBody::Lines(_)) => Vec::new(),
            Some(FaceBody::Diff(_)) => Vec::new(),
            Some(FaceBody::Files(_)) => Vec::new(),
            _ if !matches!(row.status, RowStatus::Settled(p1_contracts::ToolStatus::Ok)) => row
                .output
                .as_deref()
                .unwrap_or("")
                .lines()
                .map(|l| (l.to_string(), palette::DIM))
                .collect(),
            _ => Vec::new(),
        }
    };
    let mut diff_fold_meta = None;
    if let Some(FaceBody::Diff(rows)) = body {
        let keep = if short { 4 } else { 8 };
        let visible = if row.line_count > crate::fold::FULL_BLOCK_MAX_LINES {
            &rows[..rows.len().min(keep)]
        } else {
            rows.as_slice()
        };
        for diff_row in visible {
            out.push(render_diff_body_row(diff_row, width));
        }
        if row.line_count > crate::fold::FULL_BLOCK_MAX_LINES {
            let id = row
                .fold
                .clone()
                .unwrap_or_else(|| FoldId::of(&row.output.clone().unwrap_or_default()));
            diff_fold_meta = Some(format!(
                "· {} more diff rows folded → [{}]",
                row.line_count.saturating_sub(visible.len()),
                id
            ));
        }
    }
    let mut files_fold_meta = None;
    if let Some(FaceBody::Files(files)) = body {
        let visible = files.len().min(8);
        for (path, facts) in files.iter().take(visible) {
            out.push(
                Band {
                    bg: palette::BLOCK,
                    left: vec![seg(palette::DIM, "  "), seg(palette::REF, path)],
                    right: vec![seg(palette::DIM, facts)],
                    width,
                    pad: 2,
                }
                .render(),
            );
        }
        if files.len() > visible {
            let id = row
                .fold
                .clone()
                .unwrap_or_else(|| FoldId::of(&row.output.clone().unwrap_or_default()));
            files_fold_meta = Some(format!("· {} more files → [{}]", files.len() - visible, id));
        }
    }
    if let Some(FaceBody::Lines(lines)) = body {
        for text in lines {
            out.push(render_face_line(
                text,
                matches!(row.status, RowStatus::Settled(p1_contracts::ToolStatus::Ok)),
                width,
            ));
        }
    }
    if let Some(preview) = &row.input_preview {
        body_lines = preview
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .map(|s| (s.to_string(), palette::DIM))
            .collect();
    }
    let mut fold_meta = None;
    let limit = if short { 4 } else { 8 };
    // Only a shown body folds: an ok call's long output is registered for `^O` but draws no
    // band (§7.1 "ok → no body").
    let body_shown =
        !body_lines.is_empty() || matches!(body, Some(FaceBody::Lines(lines)) if !lines.is_empty());
    if body_shown
        && !matches!(body, Some(FaceBody::Diff(_)))
        && (body_lines.len() > crate::fold::FULL_BLOCK_MAX_LINES
            || row.line_count > crate::fold::FULL_BLOCK_MAX_LINES)
    {
        let total = row.line_count.max(body_lines.len());
        let id = row
            .fold
            .clone()
            .unwrap_or_else(|| FoldId::of(&row.output.clone().unwrap_or_default()));
        let tail = row.face.kind == TargetKind::Command;
        body_lines = if body_lines.len() > limit && tail {
            body_lines.split_off(body_lines.len() - limit)
        } else {
            body_lines.into_iter().take(limit).collect()
        };
        fold_meta = Some(format!(
            "· {} {} folded → [{}]",
            total.saturating_sub(limit),
            if tail { "earlier lines" } else { "more lines" },
            id
        ));
    }
    for (text, color) in body_lines {
        let room = width.saturating_sub(6);
        let text = if cell_width(&text) >= room {
            format!("{}…", fit_cells(&text, room.saturating_sub(1)))
        } else {
            text
        };
        out.push(full(
            Line::from(vec![
                Span::raw("    "),
                Span::styled(text, Style::new().fg(color)),
            ]),
            width,
            palette::BLOCK,
        ));
    }
    if let Some(meta) = fold_meta.or(diff_fold_meta).or(files_fold_meta) {
        let line = Band {
            bg: palette::BLOCK,
            left: vec![seg(palette::FAINT, meta)],
            right: vec![seg(palette::FAINT, "^O open in pane")],
            width,
            pad: 2,
        }
        .render();
        out.push(line);
    }
    if let Some(meta) = row.result_face.as_ref().and_then(|f| f.meta.as_ref()) {
        out.push(
            Band {
                bg: palette::BLOCK,
                left: vec![seg(palette::DIM, meta.clone())],
                right: vec![],
                width,
                pad: 2,
            }
            .render(),
        );
    }
    if let Some(approval) = approval {
        out.push(decision_line(approval, width));
        for option in &approval.options {
            if let Some(reason) = &option.unavailable {
                let line = Band {
                    bg: palette::BLOCK,
                    left: vec![seg(
                        palette::FAINT,
                        format!(" {}   {:<10} {}", option.key, option.label, reason),
                    )],
                    right: vec![],
                    width,
                    pad: 2,
                }
                .render();
                out.push(line);
            }
        }
    }
    out
}

fn render_face_line(text: &str, success: bool, width: usize) -> Line<'static> {
    let room = width.saturating_sub(6);
    let shown = if cell_width(text) >= room {
        format!("{}…", fit_cells(text, room.saturating_sub(1)))
    } else {
        text.to_string()
    };
    let text = shown.as_str();
    let mut segments = vec![Span::raw("    ")];
    if let Some(rest) = text.strip_prefix("✓ ") {
        segments.push(Span::styled("✓ ", Style::new().fg(palette::OK)));
        if let Some((command, facts)) = rest.split_once("  ") {
            segments.push(Span::styled(
                command.to_string(),
                Style::new().fg(palette::INK),
            ));
            segments.push(Span::styled(
                format!("  {facts}"),
                Style::new().fg(palette::DIM),
            ));
        } else {
            segments.push(Span::styled(
                rest.to_string(),
                Style::new().fg(palette::INK),
            ));
        }
    } else if success && let Some((label, value)) = text.split_once("    ") {
        segments.push(Span::styled(
            format!("{label:<10}"),
            Style::new().fg(palette::DIM),
        ));
        segments.push(Span::styled(
            value.to_string(),
            Style::new().fg(palette::INK),
        ));
    } else {
        segments.push(Span::styled(
            text.to_string(),
            Style::new().fg(if success { palette::INK } else { palette::DIM }),
        ));
    }
    full(Line::from(segments), width, palette::BLOCK)
}

fn render_diff_body_row(row: &crate::render::diff::DiffRow, width: usize) -> Line<'static> {
    let (number, marker, text, fg, bg) = match row {
        crate::render::diff::DiffRow::Context { line, text } => {
            (*line, ' ', text, palette::DIM, palette::BLOCK)
        }
        crate::render::diff::DiffRow::Add { line, text } => {
            (*line, '+', text, palette::DIFF_ADD_FG, palette::DIFF_ADD_BG)
        }
        crate::render::diff::DiffRow::Del { line, text } => {
            (*line, '−', text, palette::DIFF_DEL_FG, palette::DIFF_DEL_BG)
        }
    };
    let room = width.saturating_sub(9);
    let value = if cell_width(text) > room {
        format!("{}…", fit_cells(text, room.saturating_sub(1)))
    } else {
        text.clone()
    };
    let padding = room.saturating_sub(cell_width(&value));
    Line::from(vec![
        Span::styled("  ", Style::new().bg(bg)),
        Span::styled(
            format!("{number:>3}"),
            Style::new().fg(palette::FAINT).bg(bg),
        ),
        Span::styled(format!("  {marker} "), Style::new().fg(fg).bg(bg)),
        Span::styled(
            format!("{value}{}", " ".repeat(padding)),
            Style::new().fg(fg).bg(bg),
        ),
    ])
}

fn decision_line(approval: &InlineApproval, width: usize) -> Line<'static> {
    let mut left = Vec::new();
    let visible: Vec<_> = approval
        .options
        .iter()
        .filter(|option| option.unavailable.is_none() || (approval.diff && option.key == "p"))
        .collect();
    for (index, option) in visible.iter().enumerate() {
        if index > 0 {
            left.push(seg(palette::DIM, "   "));
        }
        let disabled = option.unavailable.is_some() || (approval.diff && option.key == "p");
        let key_style = if disabled {
            palette::FAINT
        } else {
            palette::GROUND
        };
        let key_bg = if disabled {
            palette::BLOCK_PLUS
        } else {
            palette::ATTN
        };
        left.push(Seg {
            fg: key_style,
            bg: Some(key_bg),
            text: format!(" {} ", option.key),
        });
        left.push(seg(
            if disabled {
                palette::FAINT
            } else {
                palette::INK
            },
            format!(" {}", option.label),
        ));
    }
    let right: Vec<Seg> = if approval.hints.is_empty() {
        vec![]
    } else {
        vec![seg(palette::FAINT, approval.hints.join("   "))]
    };
    Band {
        bg: palette::BLOCK_PLUS,
        left,
        right,
        width,
        pad: 2,
    }
    .render()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        face::{CallFace, ResultFace},
        fold::FoldId,
    };

    #[test]
    fn apply_patch_files_body_keeps_eight_rows_and_folds_the_rest() {
        let files = (0..11)
            .map(|n| (format!("src/file{n}.rs"), format!("+{n} −0")))
            .collect();
        let row = ToolRow {
            name: "apply_patch".into(),
            summary: "11 files".into(),
            status: RowStatus::Settled(p1_contracts::ToolStatus::Ok),
            output: None,
            line_count: 0,
            fold: Some(FoldId("h-12345678".into())),
            elapsed_ms: None,
            call_id: "patch".into(),
            call: None,
            face: CallFace {
                target: "11 files".into(),
                kind: TargetKind::Plain,
            },
            result_face: Some(ResultFace {
                outcome: Some("+11 −0 · 11 files".into()),
                body: FaceBody::Files(files),
                meta: None,
                target: None,
            }),
            input_preview: None,
        };
        let rendered = lines(&row, 76, false, 0, true);
        let text: Vec<String> = rendered.iter().map(ToString::to_string).collect();
        assert_eq!(rendered.len(), 10, "header, eight files, fold row");
        for index in 0..8 {
            assert!(text[index + 1].contains(&format!("src/file{index}.rs")));
        }
        assert!(!text.iter().any(|line| line.contains("src/file8.rs")));
        assert!(text[9].contains("· 3 more files → [h-12345678]"));
        assert!(text[9].contains("^O open in pane"));
    }
}
