//! The `/status` overlay (SPEC §4.6): label/value rows on a 40-column grid,
//! grouped with blank lines — not comma-joined summary lines. Unavailable rows
//! are FAINT across the whole row, label included.

use ratatui::text::Line;

use crate::grid;
use crate::palette;

/// The overlay's content grid (SPEC §4.6 fixes 40; widened to the caller's
/// width up to 60 so long route ids do not eat their own labels).
pub const STATUS_GRID: usize = 40;
pub const STATUS_GRID_MAX: usize = 60;

/// One group of rows under a DIM header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusGroup {
    pub header: String,
    pub rows: Vec<StatusRow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StatusRow {
    pub label: String,
    pub value: String,
    pub available: bool,
}

pub fn lines(groups: &[StatusGroup], width: usize) -> Vec<Line<'static>> {
    let grid = width.clamp(STATUS_GRID, STATUS_GRID_MAX);
    let mut out = Vec::new();
    for (n, group) in groups.iter().enumerate() {
        if n > 0 {
            out.push(Line::default());
        }
        out.push(Line::styled(
            group.header.clone(),
            ratatui::style::Style::new().fg(palette::DIM),
        ));
        for row in &group.rows {
            let label = format!("  {}", row.label);
            out.push(if row.available {
                grid::row(grid, &label, &row.value)
            } else {
                grid::styled_row(grid, &label, palette::FAINT, &row.value, palette::FAINT)
            });
        }
    }
    out.push(Line::default());
    out.push(Line::styled(
        "  esc",
        ratatui::style::Style::new().fg(palette::FAINT),
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Text;

    #[test]
    fn the_spec_status_example_holds() {
        let groups = vec![
            StatusGroup {
                header: "ROUTES".into(),
                rows: vec![
                    StatusRow {
                        label: "claude".into(),
                        value: "oauth · cached".into(),
                        available: true,
                    },
                    StatusRow {
                        label: "deepseek".into(),
                        value: "api key".into(),
                        available: true,
                    },
                    StatusRow {
                        label: "glm".into(),
                        value: "quota exhausted".into(),
                        available: false,
                    },
                ],
            },
            StatusGroup {
                header: "TOOLS".into(),
                rows: vec![StatusRow {
                    label: "assembled".into(),
                    value: "9".into(),
                    available: true,
                }],
            },
        ];
        let lines = lines(&groups, 40);
        let text: Vec<String> = lines
            .iter()
            .map(|l| Text::from(l.clone()).to_string())
            .collect();
        assert_eq!(text[0], "ROUTES");
        assert_eq!(text[1], "  claude                  oauth · cached");
        assert_eq!(text[3], "  glm                    quota exhausted");
        assert_eq!(text[4], "");
        assert_eq!(text[5], "TOOLS");
        assert_eq!(text[7], "");
        assert_eq!(text[8], "  esc");
        // The unavailable row is FAINT label included.
        assert!(
            lines[3]
                .spans
                .iter()
                .all(|s| s.style.fg == Some(palette::FAINT))
        );
    }
}
