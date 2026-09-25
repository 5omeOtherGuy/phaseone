//! Menus (handoff §6.10): command completion, `/model`, `/resume`. A menu is docked in the
//! bottom rows of the transcript area directly above the composer and never floats: an optional
//! BLOCK+ header, BLOCK group headers and rows, `· N more`, a BLOCK footer. Exactly one focused
//! row, filled amber with `▸ `; unavailable rows are faint and selection skips them. Filtering is
//! a plain substring match on the label — the model owns the filter text, the renderer only
//! shows what survives.
//!
//! Donor pattern: iris-agent `src/ui/picker.rs` (filter + skip-unavailable selection).

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::palette;
use crate::wrap::cell_width;

/// Item rows before `· N more` (§6.10); headers, group headers and the footer do not count.
pub const MAX_ROWS: usize = 8;

/// The command field of a header (`› /model    `): 10 cells, never cut (handoff §13 #6).
const COMMAND_FIELD: usize = 10;

/// One selectable row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub label: String,
    /// Dim text in the column after the label.
    pub description: String,
    /// Right-aligned text; on an unavailable row it is the reason.
    pub value: String,
    /// Unavailable rows are faint across the whole row and skipped by selection.
    pub available: bool,
    /// Effort levels `← →` steps through while this row is focused (`/model`); empty when the
    /// row has no effort choice.
    pub efforts: Vec<String>,
    /// Index into `efforts`.
    pub effort: usize,
}

impl Default for PickerRow {
    /// A row is selectable unless something says otherwise.
    fn default() -> Self {
        Self {
            label: String::new(),
            description: String::new(),
            value: String::new(),
            available: true,
            efforts: Vec::new(),
            effort: 0,
        }
    }
}

/// A group of rows under a dim uppercase header; an empty header draws no header row.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PickerGroup {
    pub header: String,
    pub right: String,
    pub rows: Vec<PickerRow>,
}

/// The menu model: groups plus the filter text and the selected VISIBLE row (an index into
/// the flattened, filtered list).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Picker {
    /// The command the header names (`/model`); `None` draws no header row.
    pub title: Option<String>,
    /// The header's right side (`11 models · 5 environments`).
    pub count: String,
    pub groups: Vec<PickerGroup>,
    pub filter: String,
    pub selected: usize,
    /// Cells the label column takes; 0 sizes it to the longest label plus two.
    pub label_width: usize,
    /// Footer: a dim note left, faint keys right. Both empty draws no footer.
    pub note: String,
    pub keys: String,
    /// Command completion: the filter mirrors the composer text instead of its own input.
    pub completion: bool,
}

/// The completion rows (handoff C01): the design's command set, in its order.
const COMMANDS: [(&str, &str); 10] = [
    ("/model", "switch model or effort"),
    ("/effort", "set effort for this model"),
    ("/goal", "set the session objective"),
    ("/focus", "transcript only"),
    ("/status", "session facts"),
    ("/resume", "reopen a previous session"),
    ("/access", "access and sandbox"),
    ("/help", "commands and keys"),
    ("/models", "every model p1 can run"),
    ("/exit", "quit p1"),
];

impl Picker {
    /// The command completion menu `/` opens in an empty composer. Right-hand values that only
    /// the caller knows (the current model, effort, focus) are filled in with `set_value`.
    pub fn commands() -> Self {
        Self {
            groups: vec![PickerGroup {
                rows: COMMANDS
                    .iter()
                    .map(|(label, description)| PickerRow {
                        label: (*label).into(),
                        description: (*description).into(),
                        ..PickerRow::default()
                    })
                    .collect(),
                ..PickerGroup::default()
            }],
            label_width: 12,
            keys: "↑↓ move   tab complete   ⏎ run   esc".into(),
            completion: true,
            ..Self::default()
        }
    }

    /// Set the right-hand value of the row labelled `label`, if there is one.
    pub fn set_value(&mut self, label: &str, value: impl Into<String>) {
        if let Some(row) = self
            .groups
            .iter_mut()
            .flat_map(|g| &mut g.rows)
            .find(|r| r.label == label)
        {
            row.value = value.into();
        }
    }

    /// Rows surviving the filter. The match is a case-insensitive substring on the label — the
    /// ONE matching rule, used by the model and the view.
    pub fn visible(&self) -> Vec<&PickerRow> {
        self.visible_positions()
            .into_iter()
            .map(|(g, r)| &self.groups[g].rows[r])
            .collect()
    }

    /// (group, row) indices of the visible rows, in display order.
    fn visible_positions(&self) -> Vec<(usize, usize)> {
        let needle = self.filter.to_lowercase();
        let mut out = Vec::new();
        for (g, group) in self.groups.iter().enumerate() {
            for (r, row) in group.rows.iter().enumerate() {
                if needle.is_empty() || row.label.to_lowercase().contains(&needle) {
                    out.push((g, r));
                }
            }
        }
        out
    }

    /// Move the selection by `delta` over AVAILABLE visible rows.
    pub fn move_selection(&mut self, delta: isize) {
        let visible = self.visible();
        let available: Vec<usize> = visible
            .iter()
            .enumerate()
            .filter(|(_, r)| r.available)
            .map(|(i, _)| i)
            .collect();
        if available.is_empty() {
            return;
        }
        let current = available
            .iter()
            .position(|&i| i == self.selected)
            .unwrap_or(0) as isize;
        let next = (current + delta).rem_euclid(available.len() as isize) as usize;
        self.selected = available[next];
    }

    /// Focus the first available visible row (after the filter changed).
    pub fn select_first(&mut self) {
        self.selected = self.visible().iter().position(|r| r.available).unwrap_or(0);
    }

    /// The row `⏎` would run, if any available row is selected.
    pub fn selected_row(&self) -> Option<&PickerRow> {
        self.visible()
            .get(self.selected)
            .copied()
            .filter(|r| r.available)
    }

    /// `tab`: the label the composer completes to.
    pub fn completion(&self) -> Option<String> {
        self.selected_row().map(|r| r.label.clone())
    }

    /// `← →` on the focused row: step its effort, stopping at either end.
    pub fn step_effort(&mut self, delta: isize) {
        let Some(&(g, r)) = self.visible_positions().get(self.selected) else {
            return;
        };
        let row = &mut self.groups[g].rows[r];
        if !row.available || row.efforts.is_empty() {
            return;
        }
        let last = row.efforts.len() - 1;
        row.effort = row.effort.saturating_add_signed(delta).min(last);
    }

    fn label_column(&self) -> usize {
        if self.label_width > 0 {
            return self.label_width;
        }
        self.groups
            .iter()
            .flat_map(|g| &g.rows)
            .map(|r| cell_width(&r.label))
            .max()
            .unwrap_or(0)
            + 2
    }
}

/// Render the menu on a `width`-column band (the transcript column; the bands pad 2 cells).
/// The 8-row window follows the selection, group headers attach only to shown rows, and
/// `· N more` counts the rows below the window.
pub fn lines(picker: &Picker, width: usize) -> Vec<Line<'static>> {
    let band = |bg, left: Vec<Seg>, right: Vec<Seg>| {
        Band {
            bg,
            left,
            right,
            width,
            pad: 2,
        }
        .render()
    };
    let mut out = Vec::new();
    if let Some(title) = &picker.title {
        out.push(band(
            palette::BLOCK_PLUS,
            vec![
                Seg::new(palette::ATTN, "› "),
                Seg::new(palette::DIM, field(title, COMMAND_FIELD)),
                Seg::new(palette::INK, picker.filter.clone()),
            ],
            right_side(palette::DIM, &picker.count),
        ));
    }
    let positions = picker.visible_positions();
    let total = positions.len();
    let first = picker
        .selected
        .saturating_sub(MAX_ROWS - 1)
        .min(total.saturating_sub(MAX_ROWS));
    let shown = &positions[first..(first + MAX_ROWS).min(total)];
    let label_column = picker.label_column();
    let mut last_group = None;
    for (offset, &(g, r)) in shown.iter().enumerate() {
        let group = &picker.groups[g];
        if last_group != Some(g) {
            last_group = Some(g);
            if !group.header.is_empty() {
                out.push(band(
                    palette::BLOCK,
                    vec![Seg::new(palette::DIM, group.header.clone())],
                    right_side(palette::DIM, &group.right),
                ));
            }
        }
        let row = &group.rows[r];
        let label = field(&row.label, label_column);
        if first + offset == picker.selected && row.available {
            // The effort cell shows the level as the operator reads it: a
            // profile's `extra_high` is `xhigh` (§6.10/§10, owner 2026-09-24).
            let description = match row.efforts.get(row.effort) {
                Some(effort) => format!("effort ← {} →", super::effort_label(effort)),
                None => row.description.clone(),
            };
            let on_fill = |text: String| Seg::new(palette::ON_FILL, text);
            out.push(band(
                palette::AMBER_FILL,
                vec![on_fill("▸ ".into()), on_fill(label), on_fill(description)],
                right_side(palette::ON_FILL, &row.value),
            ));
        } else {
            let (label_fg, description_fg, right_fg) = if row.available {
                (palette::INK, palette::DIM, palette::DIM)
            } else {
                (palette::FAINT, palette::FAINT, palette::FAINT)
            };
            out.push(band(
                palette::BLOCK,
                vec![
                    Seg::new(palette::FAINT, "· "),
                    Seg::new(label_fg, label),
                    Seg::new(description_fg, row.description.clone()),
                ],
                right_side(right_fg, &row.value),
            ));
        }
    }
    let below = total.saturating_sub(first + shown.len());
    if below > 0 {
        out.push(band(
            palette::BLOCK,
            vec![
                Seg::new(palette::FAINT, "· "),
                Seg::new(palette::FAINT, format!("{below} more")),
            ],
            vec![],
        ));
    }
    if !picker.note.is_empty() || !picker.keys.is_empty() {
        out.push(band(
            palette::BLOCK,
            vec![Seg::new(palette::DIM, picker.note.clone())],
            right_side(palette::FAINT, &picker.keys),
        ));
    }
    out
}

/// `text` padded to `cells`, or followed by one space when it is that long already.
fn field(text: &str, cells: usize) -> String {
    let used = cell_width(text);
    format!("{text}{}", " ".repeat(cells.saturating_sub(used).max(1)))
}

/// A Band right side; empty text is no right side, so the left keeps its full measure.
fn right_side(fg: ratatui::style::Color, text: &str) -> Vec<Seg> {
    if text.is_empty() {
        vec![]
    } else {
        vec![Seg::new(fg, text)]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(label: &str, value: &str, available: bool) -> PickerRow {
        PickerRow {
            label: label.into(),
            value: value.into(),
            available,
            ..PickerRow::default()
        }
    }

    fn picker() -> Picker {
        Picker {
            groups: vec![PickerGroup {
                header: "CLAUDE".into(),
                right: "anthropic-subscription".into(),
                rows: vec![
                    row("claude/sonnet-5", "oauth", true),
                    row("claude/opus-5.5", "current", true),
                    row("glm/5.3", "account exhausted", false),
                ],
            }],
            ..Picker::default()
        }
    }

    #[test]
    fn selection_fills_amber_and_skips_unavailable() {
        let mut p = picker();
        p.move_selection(1);
        assert_eq!(p.selected, 1);
        // The unavailable row is skipped: next wraps to row 0.
        p.move_selection(1);
        assert_eq!(p.selected, 0);
        let rendered = lines(&p, 60);
        let focused = &rendered[1];
        assert!(focused.to_string().starts_with("  ▸ claude/sonnet-5"));
        assert!(
            focused
                .spans
                .iter()
                .all(|s| s.style.bg == Some(palette::AMBER_FILL))
        );
        // The unavailable row is FAINT across the whole row, its reason on the right.
        let unavailable = &rendered[3];
        assert!(unavailable.to_string().contains("account exhausted"));
        assert!(
            unavailable
                .spans
                .iter()
                .filter(|s| !s.content.trim().is_empty())
                .all(|s| s.style.fg == Some(palette::FAINT))
        );
    }

    #[test]
    fn filtering_truncation_and_the_more_row() {
        let mut p = picker();
        p.filter = "OPUS".into();
        let text: Vec<String> = lines(&p, 60).iter().map(|l| l.to_string()).collect();
        assert_eq!(text.len(), 2, "group header + one surviving row");
        assert!(text[1].contains("opus"));

        let big = Picker {
            groups: vec![PickerGroup {
                rows: (0..10).map(|n| row(&format!("r{n}"), "v", true)).collect(),
                ..PickerGroup::default()
            }],
            keys: "esc".into(),
            ..Picker::default()
        };
        let text: Vec<String> = lines(&big, 40).iter().map(|l| l.to_string()).collect();
        assert_eq!(text.len(), MAX_ROWS + 2, "8 rows, `· 2 more`, footer");
        assert_eq!(text[MAX_ROWS].trim_end(), "  · 2 more");
    }

    #[test]
    fn effort_steps_on_the_focused_row_and_stops_at_the_ends() {
        let mut p = picker();
        p.groups[0].rows[0].efforts = vec!["low".into(), "medium".into(), "high".into()];
        p.step_effort(-1);
        assert_eq!(p.groups[0].rows[0].effort, 0);
        p.step_effort(1);
        p.step_effort(1);
        p.step_effort(1);
        assert_eq!(p.groups[0].rows[0].effort, 2);
        assert!(lines(&p, 60)[1].to_string().contains("effort ← high →"));
    }

    #[test]
    fn completion_offers_the_focused_label() {
        let mut p = Picker::commands();
        p.filter = "/mo".into();
        p.select_first();
        assert_eq!(p.completion().as_deref(), Some("/model"));
        p.move_selection(1);
        assert_eq!(p.completion().as_deref(), Some("/models"));
    }
}
