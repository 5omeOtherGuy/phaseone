//! Read-only adapter over the shipped worker renderer, separate from the shell.

use ratatui::text::Line;

use super::DashboardView;
use crate::render::workers::{self, WorkersPane};

pub struct WorkersView {
    pane: WorkersPane,
}

impl WorkersView {
    pub fn new(mut pane: WorkersPane) -> Self {
        // A read-only view has no worker-selection interaction.
        pane.focused = None;
        Self { pane }
    }
}

impl DashboardView for WorkersView {
    fn title(&self) -> &str {
        "workers"
    }

    fn lines(&self, width: usize, height: usize) -> Vec<Line<'static>> {
        workers::render_body(&self.pane, width, width < 56)
            .into_iter()
            .take(height)
            .collect()
    }
}
