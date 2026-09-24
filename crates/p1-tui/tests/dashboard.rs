use p1_tui::dashboard::workers::WorkersView;
use p1_tui::dashboard::{Dashboard, DashboardView};
use p1_tui::render::workers::{BlockState, WorkerBlock, WorkersHeader, WorkersPane};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

#[derive(Clone)]
struct TextView {
    title: &'static str,
    body: &'static str,
}

impl DashboardView for TextView {
    fn title(&self) -> &str {
        self.title
    }

    fn lines(&self, _width: usize, _height: usize) -> Vec<Line<'static>> {
        // Deliberately ignore the viewport: the shell must enforce its bounds.
        vec![Line::from(self.body).style(Style::new().fg(Color::Red)); 50]
    }
}

fn text(title: &'static str, body: &'static str) -> Box<dyn DashboardView> {
    Box::new(TextView { title, body })
}

fn draw(dashboard: &Dashboard, width: u16, height: u16) -> Vec<String> {
    let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
    terminal
        .draw(|frame| {
            let area = frame.area();
            let lines = dashboard.render(area.width as usize, area.height as usize);
            frame.render_widget(Paragraph::new(lines), area);
        })
        .unwrap();
    let buffer = terminal.backend().buffer();
    (0..height)
        .map(|y| (0..width).map(|x| buffer[(x, y)].symbol()).collect())
        .collect()
}

#[test]
fn empty_single_and_multiple_views_navigate() {
    let mut dashboard = Dashboard::new(Vec::new());
    dashboard.next();
    dashboard.previous();
    assert!(dashboard.selected().is_none());
    assert!(dashboard.render(80, 24).is_empty());

    dashboard.set_views(vec![text("only", "only body")]);
    dashboard.next();
    dashboard.previous();
    assert_eq!(dashboard.selected().unwrap().title(), "only");

    dashboard.set_views(vec![text("alpha", "alpha body"), text("beta", "beta body")]);
    assert_eq!(dashboard.selected().unwrap().title(), "alpha");
    dashboard.next();
    assert_eq!(dashboard.selected().unwrap().title(), "beta");
    dashboard.next();
    assert_eq!(dashboard.selected().unwrap().title(), "alpha");
    dashboard.previous();
    assert_eq!(dashboard.selected().unwrap().title(), "beta");
    dashboard.previous();
    assert_eq!(dashboard.selected().unwrap().title(), "alpha");
    dashboard.previous();
    assert_eq!(dashboard.selected().unwrap().title(), "beta");
    dashboard.next();
    assert_eq!(dashboard.selected().unwrap().title(), "alpha");
}

#[test]
fn replace_and_remove_views_without_shell_changes() {
    let mut dashboard =
        Dashboard::new(vec![text("alpha", "alpha body"), text("beta", "beta body")]);
    dashboard.next();
    for (width, height) in [(80, 24), (120, 40)] {
        let rows = draw(&dashboard, width, height);
        assert!(rows[0].contains("beta body"));
        assert!(rows[height as usize - 1].contains("beta"));
        assert!(rows[height as usize - 1].contains("2/2"));
        assert!(!rows[0].contains("alpha body"));
    }
    dashboard.set_views(vec![text("replacement", "replacement body")]);
    assert_eq!(dashboard.selected().unwrap().title(), "replacement");
    assert!(draw(&dashboard, 80, 24)[0].contains("replacement body"));
    dashboard.set_views(Vec::new());
    assert!(dashboard.selected().is_none());
    assert!(dashboard.render(80, 24).is_empty());
}

#[test]
fn over_rendering_and_zero_tiny_viewports_are_bounded_preserving_styles() {
    let dashboard = Dashboard::new(vec![text("tiny", "界abcdef")]);
    for (width, height) in [(0, 0), (0, 24), (1, 0), (1, 1), (2, 1), (3, 2), (9, 4)] {
        let lines = dashboard.render(width, height);
        assert!(lines.len() <= height);
        assert!(lines.iter().all(|line| line.width() <= width));
    }
    let lines = dashboard.render(3, 2);
    assert_eq!(lines[0].to_string(), "界a");
    assert_eq!(lines[0].style.fg, Some(Color::Red));
    assert_eq!(lines.len(), 2);
}

#[test]
fn short_views_fill_the_body_and_clipping_is_left_anchored() {
    struct ShortView;
    impl DashboardView for ShortView {
        fn title(&self) -> &str {
            "short"
        }

        fn lines(&self, _width: usize, _height: usize) -> Vec<Line<'static>> {
            vec![Line::from("abcdef").right_aligned()]
        }
    }
    let dashboard = Dashboard::new(vec![Box::new(ShortView)]);
    let lines = dashboard.render(3, 4);
    assert_eq!(lines[0].to_string(), "abc");
    assert_eq!(lines[0].alignment, Some(ratatui::layout::Alignment::Left));
    for filler in &lines[1..3] {
        assert_eq!(filler.to_string(), "   ");
        assert_eq!(filler.style.bg, Some(p1_tui::palette::BLOCK));
    }
    let mut terminal = Terminal::new(TestBackend::new(80, 24)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(dashboard.render(80, 24)), frame.area()))
        .unwrap();
    assert_eq!(
        terminal.backend().buffer()[(79, 22)].bg,
        p1_tui::palette::BLOCK
    );
}

#[test]
fn view_lines_fill_short_empty_and_shell_rows_preserving_styles() {
    struct StyledView;
    impl DashboardView for StyledView {
        fn title(&self) -> &str {
            "styled"
        }

        fn lines(&self, _width: usize, _height: usize) -> Vec<Line<'static>> {
            let line_style = Style::new()
                .fg(Color::Cyan)
                .bg(Color::Blue)
                .add_modifier(Modifier::ITALIC);
            let span_style = Style::new()
                .fg(Color::Magenta)
                .bg(Color::Green)
                .add_modifier(Modifier::BOLD);
            let empty_style = Style::new()
                .fg(Color::Yellow)
                .bg(Color::Blue)
                .add_modifier(Modifier::UNDERLINED);
            vec![
                Line::from("a界"),
                Line::from(Span::styled("x", span_style)).style(line_style),
                Line::default().style(empty_style),
            ]
        }
    }
    let dashboard = Dashboard::new(vec![Box::new(StyledView)]);
    let lines = dashboard.render(2, 5);
    assert_eq!(lines.len(), 5);
    assert_eq!(lines[0].width(), 2);
    assert_eq!(lines[0].to_string(), "a ");
    assert_eq!(lines[1].width(), 2);
    assert_eq!(lines[2].width(), 2);
    assert_eq!(lines[3].to_string(), "  ");

    let mut terminal = Terminal::new(TestBackend::new(2, 5)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(dashboard.render(2, 5)), frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();

    assert_eq!(buffer[(0, 0)].symbol(), "a");
    assert_eq!(buffer[(1, 0)].symbol(), " ");
    assert_eq!(buffer[(1, 0)].bg, p1_tui::palette::BLOCK);

    assert_eq!(buffer[(0, 1)].fg, Color::Magenta);
    assert_eq!(buffer[(0, 1)].bg, Color::Green);
    assert_eq!(buffer[(0, 1)].modifier, Modifier::BOLD | Modifier::ITALIC);
    assert_eq!(buffer[(1, 1)].symbol(), " ");
    assert_eq!(buffer[(1, 1)].fg, Color::Cyan);
    assert_eq!(buffer[(1, 1)].bg, Color::Blue);
    assert_eq!(buffer[(1, 1)].modifier, Modifier::ITALIC);

    for x in 0..2 {
        assert_eq!(buffer[(x, 2)].symbol(), " ");
        assert_eq!(buffer[(x, 2)].fg, Color::Yellow);
        assert_eq!(buffer[(x, 2)].bg, Color::Blue);
        assert_eq!(buffer[(x, 2)].modifier, Modifier::UNDERLINED);
        assert_eq!(buffer[(x, 3)].symbol(), " ");
        assert_eq!(buffer[(x, 3)].bg, p1_tui::palette::BLOCK);
    }
}

#[test]
fn fitting_aligned_views_are_intentionally_top_left_and_keep_styles() {
    struct FittedAlignedView;
    impl DashboardView for FittedAlignedView {
        fn title(&self) -> &str {
            "aligned"
        }

        fn lines(&self, _width: usize, _height: usize) -> Vec<Line<'static>> {
            let line_style = Style::new().fg(Color::Cyan).add_modifier(Modifier::ITALIC);
            let span_style = Style::new().fg(Color::Magenta).add_modifier(Modifier::BOLD);
            vec![
                Line::from("right").right_aligned().style(line_style),
                Line::from(Span::styled("center", span_style))
                    .centered()
                    .style(line_style),
            ]
        }
    }
    let dashboard = Dashboard::new(vec![Box::new(FittedAlignedView)]);
    let mut terminal = Terminal::new(TestBackend::new(10, 4)).unwrap();
    terminal
        .draw(|frame| frame.render_widget(Paragraph::new(dashboard.render(10, 4)), frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();

    let right: String = (0..5).map(|x| buffer[(x, 0)].symbol()).collect();
    let center: String = (0..6).map(|x| buffer[(x, 1)].symbol()).collect();
    assert_eq!(right, "right");
    assert_eq!(center, "center");
    assert_eq!(buffer[(5, 0)].symbol(), " ");
    assert_eq!(buffer[(6, 1)].symbol(), " ");
    assert_eq!(
        (6..10).map(|x| buffer[(x, 1)].symbol()).collect::<String>(),
        "    "
    );

    assert_eq!(buffer[(0, 0)].fg, Color::Cyan);
    assert_eq!(buffer[(0, 0)].modifier, Modifier::ITALIC);
    assert_eq!(buffer[(0, 1)].fg, Color::Magenta);
    assert_eq!(buffer[(0, 1)].modifier, Modifier::BOLD | Modifier::ITALIC);
    assert_eq!(buffer[(6, 1)].fg, Color::Cyan);
    assert_eq!(buffer[(6, 1)].modifier, Modifier::ITALIC);
    assert_eq!(buffer[(6, 1)].bg, p1_tui::palette::BLOCK);
}

fn worker(id: &str, state: BlockState) -> WorkerBlock {
    WorkerBlock {
        id: id.into(),
        task: "synthetic work".into(),
        route: "synthetic/model".into(),
        state,
        elapsed: Some("0m12s".into()),
        cost_micro_usd: None,
        grants: "read shell finish".into(),
        activity: "synthetic worker activity".into(),
    }
}

#[test]
fn workers_adapter_handles_compact_threshold_and_tiny_viewports_read_only() {
    let pane = || WorkersPane {
        header: WorkersHeader {
            live: 1,
            queued: None,
            pool: String::new(),
        },
        workers: vec![worker("w-run", BlockState::Running)],
        focused: Some("w-run".into()),
    };
    let assert_read_only = |output: &str| {
        assert!(!output.contains(" attach"));
        assert!(!output.contains(" stop"));
        assert!(!output.contains(" select"));
        assert!(
            !output.contains('▸'),
            "read-only view must clear worker focus"
        );
    };

    for width in [38, 55] {
        let lines = WorkersView::new(pane()).lines(width, 6);
        assert_eq!(lines.len(), 4);
        assert!(lines.iter().all(|line| line.width() <= width));
        assert!(lines[2].to_string().contains("w-run"));
        assert!(lines[3].to_string().contains("synthetic/model"));
        assert!(!lines.iter().any(|line| line.to_string().contains("grants")));
        let output = lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n");
        assert_read_only(&output);
    }

    let lines = WorkersView::new(pane()).lines(56, 6);
    assert_eq!(lines.len(), 6);
    assert!(lines.iter().all(|line| line.width() <= 56));
    assert!(lines[2].to_string().contains("w-run"));
    assert!(lines[3].to_string().contains("synthetic/model"));
    assert!(lines[4].to_string().contains("grants"));
    assert!(lines[5].to_string().contains("synthetic worker activity"));
    let wide_output = lines
        .iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    assert_read_only(&wide_output);

    let dashboard = Dashboard::new(vec![Box::new(WorkersView::new(pane()))]);
    for (width, height) in [(0, 0), (0, 24), (1, 0), (1, 1), (2, 1), (3, 2), (9, 4)] {
        let lines = dashboard.render(width, height);
        if width == 0 || height == 0 {
            assert!(lines.is_empty());
        } else {
            assert_eq!(lines.len(), height);
            assert!(lines.iter().all(|line| line.width() <= width));
        }
    }
}

#[test]
fn workers_keep_order_and_unknown_cost_without_unwired_actions() {
    let pane = WorkersPane {
        header: WorkersHeader {
            live: 1,
            queued: None,
            pool: String::new(),
        },
        workers: vec![
            worker("w-run", BlockState::Running),
            worker("w-review", BlockState::NeedsReview),
        ],
        focused: Some("w-run".into()),
    };
    let dashboard = Dashboard::new(vec![Box::new(WorkersView::new(pane))]);
    for (width, height) in [(80, 24), (120, 40)] {
        let rows = draw(&dashboard, width, height);
        let output = rows.join("\n");
        assert!(output.find("w-review").unwrap() < output.find("w-run").unwrap());
        assert!(output.contains('—'));
        assert!(!output.contains("$0.0000"));
        assert!(!output.contains(" attach"));
        assert!(!output.contains(" stop"));
        assert!(!output.contains(" select"));
        assert!(
            !output.contains('▸'),
            "read-only view must clear worker focus"
        );
        assert!(rows[height as usize - 1].contains("workers"));
    }
}
