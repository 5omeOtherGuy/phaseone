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
    /// Rows surviving the filter. The match is a case-insensitive substring
    /// on the label — the ONE matching rule, used by the model and the view.
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

/// Render the picker on a `width`-column grid. The 8-row window follows the
/// selection (SPEC §4.6: there is always exactly one inverted row), group
/// headers attach only to visible rows, and `· N more` counts the rows below
/// the window.
pub fn lines(picker: &Picker, width: usize) -> Vec<Line<'static>> {
    let visible = picker.visible();
    let total = visible.len();
    let first = picker
        .selected
        .saturating_sub(MAX_ROWS - 1)
        .min(total.saturating_sub(MAX_ROWS));
    let window: &[&PickerRow] = &visible[first..(first + MAX_ROWS).min(total)];
    let mut out = Vec::new();
    // Group headers above their first visible row. A row's group is found by
    // walking the groups' surviving rows in order.
    let mut by_ptr: std::collections::HashMap<*const PickerRow, &str> =
        std::collections::HashMap::new();
    let needle = picker.filter.to_lowercase();
    for group in &picker.groups {
        for row in &group.rows {
            if needle.is_empty() || row.label.to_lowercase().contains(&needle) {
                by_ptr.insert(row as *const _, group.header.as_str());
            }
        }
    }
    let mut last_header: Option<&str> = None;
    for (offset, row) in window.iter().enumerate() {
        let header = by_ptr.get(&(*row as *const PickerRow)).copied();
        if header != last_header {
            if let Some(header) = header {
                out.push(Line::styled(
                    header.to_string(),
                    Style::new().fg(palette::DIM),
                ));
            }
            last_header = header;
        }
        let mut line = if row.available {
            crate::grid::row(width, &format!("  {}", row.label), &row.value)
        } else {
            crate::grid::styled_row(
                width,
                &format!("  {}", row.label),
                palette::FAINT,
                &row.value,
                palette::FAINT,
            )
        };
        if first + offset == picker.selected && row.available {
            // Selection inverts — the only highlight mechanism (SPEC §1).
            for span in &mut line.spans {
                span.style = Style::new()
                    .fg(palette::SELECTION_FG)
                    .bg(palette::SELECTION_BG);
            }
        }
        out.push(line);
    }
    let below = total.saturating_sub(first + window.len());
    if below > 0 {
        out.push(Line::styled(
            format!("  {} {below} more", crate::glyphs::PENDING),
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
            &format!("  {} 2 more", crate::glyphs::PENDING)
        );
    }
}
