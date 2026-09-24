//! Offline synthetic preview: `cargo run -p p1-tui --example dashboard_preview`
//! or append `-- wide next` for 120x40 and the second composed view.

use p1_tui::dashboard::workers::WorkersView;
use p1_tui::dashboard::{Dashboard, DashboardView};
use p1_tui::render::workers::{BlockState, WorkerBlock, WorkersHeader, WorkersPane};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::text::Line;
use ratatui::widgets::Paragraph;

struct SyntheticView;

impl DashboardView for SyntheticView {
    fn title(&self) -> &str {
        "synthetic"
    }

    fn lines(&self, _width: usize, _height: usize) -> Vec<Line<'static>> {
        vec![
            Line::from("Synthetic dashboard preview"),
            Line::from("  This page is fixture data only."),
            Line::from("  It demonstrates navigation and viewport bounds."),
            Line::from("  No quota or brain module is implemented."),
        ]
    }
}

fn draw(dashboard: &Dashboard, width: u16, height: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            let lines = dashboard.render(area.width as usize, area.height as usize);
            frame.render_widget(Paragraph::new(lines), area);
        })
        .unwrap();

    let buffer = terminal.backend().buffer();
    for y in 0..height {
        let row: String = (0..width).map(|x| buffer[(x, y)].symbol()).collect();
        println!("{row}");
    }
}

fn main() {
    let (width, height) = match std::env::args().nth(1).as_deref() {
        Some("wide") => (120, 40),
        _ => (80, 24),
    };
    let pane = WorkersPane {
        header: WorkersHeader {
            live: 1,
            queued: None,
            pool: "synthetic".into(),
        },
        workers: vec![WorkerBlock {
            id: "synthetic-worker".into(),
            task: "synthetic work".into(),
            route: "synthetic/model".into(),
            state: BlockState::Running,
            elapsed: Some("0m12s".into()),
            cost_micro_usd: None,
            grants: "none (synthetic fixture)".into(),
            activity: "synthetic worker activity".into(),
        }],
        focused: None,
    };
    let mut dashboard = Dashboard::new(vec![
        Box::new(WorkersView::new(pane)),
        Box::new(SyntheticView),
    ]);
    if std::env::args().nth(2).as_deref() == Some("next") {
        dashboard.next();
    }
    draw(&dashboard, width, height);
}
