use p1_contracts::{AgentEvent, StopReason, TurnEnd};
use p1_tui::input::ViewCommand;
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
        route: "worker-route".into(),
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

fn rows(state: BlockState) -> Vec<WorkerBlock> {
    vec![worker("w1", state), worker("w2", state)]
}

fn focus_w2(screen: &mut Screen) {
    screen.pane_mode = PaneMode::Workers;
    screen.workers_ever_started = true;
    screen.pane_width = PaneWidth::Wide;
    screen.sync_workers(rows(BlockState::Running));
    screen.apply_view(ViewCommand::TogglePaneFocus);
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
}

fn draw_frame(screen: &mut Screen, now_ms: u64) -> Buffer {
    let mut terminal = Terminal::new(TestBackend::new(120, 40)).unwrap();
    terminal
        .draw(|frame| draw(screen, frame.area(), frame.buffer_mut(), now_ms))
        .unwrap();
    terminal.backend().buffer().clone()
}

fn transcript_text(screen: &Screen, buffer: &Buffer) -> String {
    let transcript = p1_tui::geometry::layout(120, 40, screen.pane_width, false, 0).transcript;
    (transcript.y..transcript.bottom())
        .map(|y| {
            (transcript.x..transcript.right())
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn text(word: &str) -> AgentEvent {
    AgentEvent::TextDelta { text: word.into() }
}

#[test]
fn buffered_worker_events_attach_live_and_survive_detach() {
    let mut screen = Screen::new(true);
    screen.apply(&text("parent-only-alpha"), 0);
    focus_w2(&mut screen);

    screen.apply_worker("w2", &text("worker-earlier-bravo"), 1);
    screen.apply_view(ViewCommand::AttachWorker);
    let frame = draw_frame(&mut screen, 2);
    let attached = transcript_text(&screen, &frame);
    assert!(attached.contains("worker-earlier-bravo"));
    assert!(!attached.contains("parent-only-alpha"));

    screen.apply_worker("w2", &text("worker-newer-charlie"), 3);
    let frame = draw_frame(&mut screen, 4);
    let live = transcript_text(&screen, &frame);
    assert!(live.contains("worker-earlier-bravo"));
    assert!(live.contains("worker-newer-charlie"));

    screen.apply_view(ViewCommand::DetachWorker);
    let frame = draw_frame(&mut screen, 5);
    let parent = transcript_text(&screen, &frame);
    assert!(parent.contains("parent-only-alpha"));
    assert!(!parent.contains("worker-earlier-bravo"));
    assert!(!parent.contains("worker-newer-charlie"));

    screen.apply_view(ViewCommand::AttachWorker);
    let frame = draw_frame(&mut screen, 6);
    let reattached = transcript_text(&screen, &frame);
    assert!(reattached.contains("worker-earlier-bravo"));
    assert!(reattached.contains("worker-newer-charlie"));
}

#[test]
fn parent_activity_and_worker_refreshes_keep_the_attachment_and_track_state() {
    let mut screen = Screen::new(true);
    focus_w2(&mut screen);
    screen.apply_view(ViewCommand::AttachWorker);

    for cycle in 0..5 {
        screen.apply(&AgentEvent::TurnStarted, cycle * 20);
        screen.apply(&text(&format!("parent-cycle-{cycle}")), cycle * 20 + 1);
        screen.sync_workers(rows(BlockState::Running));
        draw_frame(&mut screen, cycle * 20 + 2);
        assert_eq!(
            screen.attached.as_ref().map(|worker| worker.id.as_str()),
            Some("w2")
        );
        assert!(screen.pane_focused);
        assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
        screen.apply(
            &AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
            },
            cycle * 20 + 10,
        );
    }

    screen.worker_mode_auto = false;
    screen.sync_workers(rows(BlockState::Done));
    draw_frame(&mut screen, 200);
    let attached = screen.attached.as_ref().expect("w2 remains attached");
    assert_eq!(attached.id, "w2");
    assert_eq!(attached.state, BlockState::Done);
    assert!(screen.pane_focused);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));

    screen.sync_workers(vec![]);
    let attached = screen
        .attached
        .as_ref()
        .expect("a lost worker stays attached");
    assert_eq!(attached.id, "w2");
    assert_eq!(attached.state, BlockState::Done);
}
