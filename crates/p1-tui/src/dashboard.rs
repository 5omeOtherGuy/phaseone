//! Pure dashboard navigation and viewport composition. Views are supplied by
//! the caller; this module owns no services, events, storage, or terminal.

pub mod workers;

use ratatui::text::Line;

use crate::band::{Band, Seg};
use crate::palette;
use crate::wrap::{cell_width, fit_cells};

/// One replaceable dashboard view. `height` excludes the navigation strip.
/// The shell clips content from the top left; views own any internal scrolling.
pub trait DashboardView {
    fn title(&self) -> &str;
    fn lines(&self, width: usize, height: usize) -> Vec<Line<'static>>;
}

/// Explicitly composed views and selection, without a registry or loader.
pub struct Dashboard {
    views: Vec<Box<dyn DashboardView>>,
    selected: usize,
}

impl Dashboard {
    pub fn new(views: Vec<Box<dyn DashboardView>>) -> Self {
        Self { views, selected: 0 }
    }

    pub fn selected(&self) -> Option<&dyn DashboardView> {
        self.views.get(self.selected).map(Box::as_ref)
    }

    /// Retain the selected position if it still exists in the new composition.
    pub fn set_views(&mut self, views: Vec<Box<dyn DashboardView>>) {
        self.views = views;
        self.selected = self.selected.min(self.views.len().saturating_sub(1));
    }

    pub fn next(&mut self) {
        if !self.views.is_empty() {
            self.selected = (self.selected + 1) % self.views.len();
        }
    }

    pub fn previous(&mut self) {
        if !self.views.is_empty() {
            self.selected = if self.selected == 0 {
                self.views.len() - 1
            } else {
                self.selected - 1
            };
        }
    }

    /// Bound even an over-rendering view and anchor navigation to the last row.
    pub fn render(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        let Some(view) = self.selected() else {
            return Vec::new();
        };
        if width == 0 || height == 0 {
            return Vec::new();
        }
        let body_height = height - 1;
        let mut lines: Vec<_> = view
            .lines(width, body_height)
            .into_iter()
            .take(body_height)
            .map(|line| clip(line, width))
            .collect();
        lines.resize_with(body_height, || {
            Line::styled(
                " ".repeat(width),
                ratatui::style::Style::new().bg(palette::BLOCK),
            )
        });
        lines.push(self.strip(width));
        lines
    }

    fn strip(&self, width: usize) -> Line<'static> {
        let mut left = Vec::with_capacity(self.views.len());
        for (index, view) in self.views.iter().enumerate() {
            let title = if index == 0 {
                view.title().to_string()
            } else {
                format!("  {}", view.title())
            };
            left.push(Seg::new(
                if index == self.selected {
                    palette::DIM
                } else {
                    palette::FAINT
                },
                title,
            ));
        }
        Band {
            bg: palette::BLOCK,
            left,
            right: vec![Seg::new(
                palette::FAINT,
                format!("{}/{}", self.selected + 1, self.views.len()),
            )],
            width,
            pad: 4,
        }
        .render()
    }
}

fn clip(mut line: Line<'static>, width: usize) -> Line<'static> {
    let mut used = 0;
    let mut spans = Vec::new();
    for span in line.spans {
        let span_width = cell_width(&span.content);
        if used + span_width <= width {
            used += span_width;
            spans.push(span);
        } else {
            let room = width.saturating_sub(used);
            if room > 0 {
                let fitted = fit_cells(&span.content, room);
                used += cell_width(&fitted);
                spans.push(ratatui::text::Span::styled(fitted, span.style));
            }
            break;
        }
    }
    // Clipping is top-left anchored; retain styling, not a second alignment pass.
    line.alignment = Some(ratatui::layout::Alignment::Left);
    line.style = line.style.bg(line.style.bg.unwrap_or(palette::BLOCK));
    if used < width {
        spans.push(ratatui::text::Span::raw(" ".repeat(width - used)));
    }
    line.spans = spans;
    line
}
