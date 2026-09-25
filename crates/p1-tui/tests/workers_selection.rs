use p1_contracts::{AgentEvent, StopReason, TurnEnd};
use p1_tui::fold::FoldId;
use p1_tui::input::ViewCommand;
use p1_tui::render::output::OutputView;
use p1_tui::render::screen::draw;
use p1_tui::render::workers::{BlockState, WorkerBlock};
use p1_tui::state::{PaneMode, PaneWidth, Screen};
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

fn input_order_rows() -> Vec<WorkerBlock> {
    vec![
        worker("w3", BlockState::Queued),
        worker("w2", BlockState::Running),
        worker("w4", BlockState::Done),
        worker("w1", BlockState::NeedsReview),
    ]
}

fn focus_workers(screen: &mut Screen) {
    screen.apply_view(ViewCommand::TogglePaneFocus);
    assert!(screen.pane_focused);
}

fn draw_frame(screen: &mut Screen, now_ms: u64) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| draw(screen, frame.area(), frame.buffer_mut(), now_ms))
        .unwrap();
    terminal.backend().buffer().clone()
}

#[test]
fn focus_entry_selects_the_first_worker_in_display_order() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    screen.sync_workers(input_order_rows());
    assert_eq!(screen.pane_mode, PaneMode::Workers);

    focus_workers(&mut screen);

    assert_eq!(screen.workers.focused.as_deref(), Some("w1"));
}

#[test]
fn pane_step_moves_in_display_order_and_clamps_at_both_ends() {
    let mut screen = Screen::new(true);
    screen.sync_workers(input_order_rows());
    focus_workers(&mut screen);

    assert_eq!(screen.workers.focused.as_deref(), Some("w1"));
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w3"));
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w4"));
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w4"));
    screen.pane_step(-1);
    screen.pane_step(-1);
    screen.pane_step(-1);
    screen.pane_step(-1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w1"));
}

#[test]
fn selection_survives_lead_activity_refreshes_and_draws() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    screen.sync_workers(vec![
        worker("w1", BlockState::Running),
        worker("w2", BlockState::Running),
        worker("w3", BlockState::Queued),
    ]);
    focus_workers(&mut screen);
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));

    for cycle in 0..5 {
        screen.apply(&AgentEvent::TurnStarted, cycle * 20);
        screen.sync_workers(vec![
            worker("w1", BlockState::Running),
            worker("w2", BlockState::Running),
            worker("w3", BlockState::Queued),
        ]);
        draw_frame(&mut screen, cycle * 20 + 1);
        assert!(screen.pane_focused);
        assert_eq!(screen.pane_mode, PaneMode::Workers);
        assert_eq!(screen.workers.focused.as_deref(), Some("w2"));

        screen.apply(
            &AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
            },
            cycle * 20 + 10,
        );
        screen.sync_workers(vec![
            worker("w1", BlockState::Running),
            worker("w2", BlockState::Running),
            worker("w3", BlockState::Queued),
        ]);
        draw_frame(&mut screen, cycle * 20 + 11);
        assert!(screen.pane_focused);
        assert_eq!(screen.pane_mode, PaneMode::Workers);
        assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
    }
}

#[test]
fn a_lost_focused_worker_selects_the_first_remaining_display_row() {
    let mut focused = Screen::new(true);
    focused.sync_workers(vec![
        worker("w1", BlockState::NeedsReview),
        worker("w2", BlockState::Running),
        worker("w3", BlockState::Queued),
    ]);
    focus_workers(&mut focused);
    focused.pane_step(1);
    assert_eq!(focused.workers.focused.as_deref(), Some("w2"));
    focused.sync_workers(vec![
        worker("w3", BlockState::Queued),
        worker("w1", BlockState::NeedsReview),
    ]);
    assert_eq!(focused.workers.focused.as_deref(), Some("w1"));

    let mut unfocused = Screen::new(true);
    unfocused.sync_workers(vec![
        worker("w1", BlockState::NeedsReview),
        worker("w2", BlockState::Running),
    ]);
    unfocused.workers.focused = Some("w2".into());
    unfocused.sync_workers(vec![worker("w1", BlockState::NeedsReview)]);
    assert_eq!(unfocused.workers.focused, None);
}

#[test]
fn focus_exit_clears_selection_and_refresh_does_not_restore_it() {
    let mut screen = Screen::new(true);
    screen.sync_workers(input_order_rows());
    focus_workers(&mut screen);
    assert!(screen.workers.focused.is_some());

    screen.apply_view(ViewCommand::TogglePaneFocus);

    assert!(!screen.pane_focused);
    assert_eq!(screen.workers.focused, None);
    screen.sync_workers(input_order_rows());
    assert_eq!(screen.workers.focused, None);
}

#[test]
fn output_mode_pane_step_scrolls_output_without_touching_worker_selection() {
    let mut screen = Screen::new(true);
    screen.sync_workers(input_order_rows());
    screen.workers.focused = Some("w2".into());
    screen.output = Some(OutputView {
        id: FoldId::of("call"),
        lines: vec!["one".into(), "two".into()],
        scroll: 0,
    });
    screen.pane_mode = PaneMode::Output;

    screen.pane_step(1);

    assert_eq!(screen.output.as_ref().unwrap().scroll, 1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
}

#[test]
fn selected_worker_first_row_is_drawn_with_the_focus_fill() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    screen.sync_workers(vec![worker("w1", BlockState::Running)]);
    focus_workers(&mut screen);

    let buffer = draw_frame(&mut screen, 0);
    let pane = p1_tui::geometry::layout(120, 40, PaneWidth::Wide, false, 0).pane;
    let selected_row = (pane.y..pane.bottom())
        .find(|y| {
            (pane.x..pane.right()).any(|x| buffer[(x, *y)].bg == p1_tui::palette::AMBER_FILL)
                && (pane.x..pane.right())
                    .map(|x| buffer[(x, *y)].symbol())
                    .collect::<String>()
                    .contains("w1")
        })
        .expect("selected worker id on its amber first row");
    assert_eq!(
        buffer[(pane.x, selected_row)].bg,
        p1_tui::palette::AMBER_FILL
    );
}
