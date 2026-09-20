//! Pickers and the `/status` overlay (SPEC §4.6): max 8 rows, one inverted
//! selection row, `· N more` when truncated, group headers DIM, unavailable
//! rows FAINT across the whole row. Selection inverts (the only highlight
//! mechanism, SPEC §1). Filter-as-you-type is a plain substring match — the
//! model owns the filter text, the renderer only shows what survives.
//!
//! Donor pattern: iris-agent `src/ui/picker.rs` (filter + inverted selection),
//! reduced from 1,629 lines to the two rules p1's spec fixes.

use ratatui::style::Style;
use ratatui::text::Line;

use crate::glyphs;
use crate::grid;
use crate::palette;

/// Visible rows before truncation (SPEC §4.6).
pub const MAX_ROWS: usize = 8;

/// One selectable row: a label, a right-aligned value, availability.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerRow {
    pub label: String,
    pub value: String,
    /// Unavailable rows (`quota exhausted`, `not authed`) are FAINT across the
    /// whole row, label included, and are skipped by selection movement.
    pub available: bool,
}

/// A named group of rows; the header is DIM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PickerGroup {
    pub header: String,
    pub rows: Vec<PickerRow>,
}

/// The picker model: groups plus the filter text and the selected VISIBLE row
/// (index into the flattened, filtered list).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Picker {
    pub groups: Vec<PickerGroup>,
    pub filter: String,
    pub selected: usize,
}

impl Picker {
    /// Rows surviving the filter, as (group header position is separate).
    /// The match is a case-insensitive substring on the label.
    pub fn visible(&self) -> Vec<&PickerRow> {
        let needle = self.filter.to_lowercase();
        self.groups
            .iter()
            .flat_map(|g| &g.rows)
            .filter(|r| needle.is_empty() || r.label.to_lowercase().contains(&needle))
            .collect()
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

    /// The row `⏎` would run, if any available row is selected.
    pub fn selected_row(&self) -> Option<&PickerRow> {
        self.visible().get(self.selected).copied()
    }
}

/// Render the picker on a `width`-column grid.
pub fn lines(picker: &Picker, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    let mut flat_index = 0usize;
    let mut shown = 0usize;
    let mut hidden = 0usize;
    for group in &picker.groups {
        let surviving: Vec<&PickerRow> = group
            .rows
            .iter()
            .filter(|r| {
                picker.filter.is_empty()
                    || r.label
                        .to_lowercase()
                        .contains(&picker.filter.to_lowercase())
            })
            .collect();
        if surviving.is_empty() {
            continue;
        }
        out.push(Line::styled(
            group.header.clone(),
            Style::new().fg(palette::DIM),
        ));
        for row in surviving {
            let index = flat_index;
            flat_index += 1;
            if shown >= MAX_ROWS {
                hidden += 1;
                continue;
            }
            shown += 1;
            let mut line = if row.available {
                grid::row(width, &format!("  {}", row.label), &row.value)
            } else {
                grid::styled_row(
                    width,
                    &format!("  {}", row.label),
                    palette::FAINT,
                    &row.value,
                    palette::FAINT,
                )
            };
            if index == picker.selected && row.available {
                // Selection inverts — the only highlight mechanism (SPEC §1).
                for span in &mut line.spans {
                    span.style = Style::new()
                        .fg(palette::SELECTION_FG)
                        .bg(palette::SELECTION_BG);
                }
            }
            out.push(line);
        }
    }
    if hidden > 0 {
        out.push(Line::styled(
            format!("  {} {hidden} more", glyphs::PENDING),
            Style::new().fg(palette::FAINT),
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    fn picker() -> Picker {
        Picker {
            groups: vec![PickerGroup {
                header: "ANTHROPIC ROUTE".into(),
                rows: vec![
                    PickerRow {
                        label: "claude · sonnet-4.5".into(),
                        value: "300k · $3/$15".into(),
                        available: true,
                    },
                    PickerRow {
                        label: "claude · opus-4.8".into(),
                        value: "300k · $15/$75".into(),
                        available: true,
                    },
                    PickerRow {
                        label: "glm · 5.3".into(),
                        value: "quota exhausted".into(),
                        available: false,
                    },
                ],
            }],
            filter: String::new(),
            selected: 0,
        }
    }

    #[test]
    fn selection_inverts_and_skips_unavailable() {
        let mut p = picker();
        p.selected = 0;
        p.move_selection(1);
        assert_eq!(p.selected, 1);
        // The unavailable row is skipped: next wraps to row 0.
        p.move_selection(1);
        assert_eq!(p.selected, 0);
        let rendered = lines(&p, 40);
        let selected = &rendered[1 + p.selected];
        assert_eq!(selected.spans[0].style.bg, Some(palette::SELECTION_BG));
        // The unavailable row is FAINT across the whole row.
        let unavailable = &rendered[3];
        assert!(
            unavailable
                .spans
                .iter()
                .all(|s| s.style.fg == Some(palette::FAINT))
        );
    }

    #[test]
    fn filtering_and_truncation() {
        let mut p = picker();
        p.filter = "opus".into();
        let rendered = lines(&p, 40);
        let text: Vec<String> = rendered
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        assert_eq!(text.len(), 2, "header + one surviving row");
        assert!(text[1].contains("opus"));

        // Ten rows: eight shown, `· 2 more`.
        let big = Picker {
            groups: vec![PickerGroup {
                header: "G".into(),
                rows: (0..10)
                    .map(|n| PickerRow {
                        label: format!("r{n}"),
                        value: "v".into(),
                        available: true,
                    })
                    .collect(),
            }],
            filter: String::new(),
            selected: 0,
        };
        let text: Vec<String> = lines(&big, 40)
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        assert_eq!(text.len(), 1 + MAX_ROWS + 1);
        assert_eq!(
            text.last().unwrap(),
            &format!("  {} 2 more", glyphs::PENDING)
        );
    }
}
