use p1_tui::input::ViewCommand;
use p1_tui::palette;
use p1_tui::render::screen::draw;
use p1_tui::render::workers::{self, BlockState, WorkerBlock};
use p1_tui::state::{PaneWidth, Screen};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use ratatui::buffer::Buffer;

fn worker(id: &str, state: BlockState) -> WorkerBlock {
    WorkerBlock {
        id: id.into(),
        task: format!("task {id}"),
        route: "route".into(),
        model: None,
        state,
        elapsed: None,
        cost_micro_usd: None,
        tokens: None,
        context_window: None,
        grants: String::new(),
        activity: String::new(),
    }
}

fn rows() -> Vec<WorkerBlock> {
    vec![
        worker("w1", BlockState::Running),
        worker("w2", BlockState::Running),
    ]
}

fn focused_w2() -> Screen {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    screen.sync_workers(rows());
    screen.apply_view(ViewCommand::TogglePaneFocus);
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
    screen
}

fn pending_w2() -> Screen {
    let mut screen = focused_w2();
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending.as_deref(), Some("w2"));
    screen
}

fn draw_frame(screen: &mut Screen) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| draw(screen, frame.area(), frame.buffer_mut(), 0))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn pane_text(screen: &Screen, buffer: &Buffer, needle: &str) -> (u16, String) {
    let pane = p1_tui::geometry::layout(120, 40, screen.pane_width, false, 0).pane;
    (pane.y..pane.bottom())
        .map(|y| {
            let text = (pane.x..pane.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>();
            (y, text)
        })
        .find(|(_, text)| text.contains(needle))
        .unwrap_or_else(|| panic!("pane row containing {needle:?}"))
}

#[test]
fn asking_to_stop_selects_w2_and_draws_the_amber_confirmation_in_wide_and_compact_forms() {
    let mut screen = pending_w2();
    let buffer = draw_frame(&mut screen);
    let (y, text) = pane_text(&screen, &buffer, "stop w2?   y stop");
    assert!(text.contains("n keep"));
    let pane = p1_tui::geometry::layout(120, 40, screen.pane_width, false, 0).pane;
    let start = text.find("stop w2?").unwrap() as u16 + pane.x;
    let cell = &buffer[(start, y)];
    assert_eq!(cell.bg, palette::AMBER_FILL);
    assert_eq!(cell.fg, palette::ON_FILL);

    let compact = workers::render_with(&screen.workers, 38, true, Some("w2"));
    let footer = compact.last().unwrap();
    assert_eq!(
        footer.to_string().trim_end(),
        "    stop w2?   y stop   n keep"
    );
    assert!(
        footer
            .spans
            .iter()
            .all(|span| span.style.bg == Some(palette::AMBER_FILL))
    );
    let text = footer
        .spans
        .iter()
        .find(|span| span.content.contains("stop w2?"))
        .unwrap();
    assert_eq!(text.style.fg, Some(palette::ON_FILL));
}

#[test]
fn keeping_a_worker_clears_the_confirmation_and_restores_the_footer() {
    let mut screen = pending_w2();
    screen.apply_view(ViewCommand::KeepWorker);
    assert_eq!(screen.stop_pending, None);
    let buffer = draw_frame(&mut screen);
    let (_, text) = pane_text(&screen, &buffer, "^F select");
    assert!(text.contains("a attach   x stop"));
    assert!(!text.contains("stop w2?"));
}

#[test]
fn a_done_worker_cannot_be_asked_to_stop() {
    let mut screen = focused_w2();
    screen.workers.workers[1].state = BlockState::Done;
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending, None);
}

#[test]
fn an_attached_worker_wins_over_a_different_selection() {
    let mut screen = focused_w2();
    screen.workers.focused = Some("w1".into());
    screen.apply_view(ViewCommand::AttachWorker);
    screen.workers.focused = Some("w2".into());
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending.as_deref(), Some("w1"));
}

#[test]
fn a_refresh_clears_a_pending_stop_when_the_worker_finishes_or_disappears() {
    let mut screen = pending_w2();
    screen.sync_workers(vec![
        worker("w1", BlockState::Running),
        worker("w2", BlockState::Done),
    ]);
    assert_eq!(screen.stop_pending, None);

    let mut screen = pending_w2();
    screen.sync_workers(vec![worker("w1", BlockState::Running)]);
    assert_eq!(screen.stop_pending, None);
}
