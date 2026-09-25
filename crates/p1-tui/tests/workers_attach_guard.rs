use p1_contracts::AgentEvent;
use p1_tui::fold::FoldId;
use p1_tui::input::ViewCommand;
use p1_tui::render::output::OutputView;
use p1_tui::render::workers::{BlockState, WorkerBlock};
use p1_tui::state::{PaneMode, PaneWidth, Screen};
use p1_tui::transcript::Block;

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

fn workers(state: BlockState) -> Vec<WorkerBlock> {
    vec![worker("w1", state), worker("w2", state)]
}

fn attached_to_w2() -> Screen {
    let mut screen = Screen::new(false);
    screen.pane_width = PaneWidth::Wide;
    screen.sync_workers(workers(BlockState::Running));
    screen.apply_worker(
        "w2",
        &AgentEvent::TextDelta {
            text: "w2-word".into(),
        },
        1,
    );
    screen.apply_view(ViewCommand::TogglePaneFocus);
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
    screen.apply_view(ViewCommand::AttachWorker);
    assert_eq!(
        screen.attached.as_ref().map(|worker| worker.id.as_str()),
        Some("w2")
    );
    screen
}

fn has_text(transcript: &p1_tui::transcript::Transcript) -> bool {
    transcript.blocks.iter().any(|block| {
        matches!(block, Block::Prose { lines } if lines.iter().any(|line| line.as_str() == "w2-word"))
    })
}

fn invariant(screen: &Screen) {
    if screen.attached.is_some() {
        assert!(screen.pane_focused);
        assert_eq!(screen.pane_mode, PaneMode::Workers);
    }
}

#[test]
fn ctrl_f_off_detaches_and_returns_the_buffer() {
    let mut screen = attached_to_w2();
    screen.apply_view(ViewCommand::TogglePaneFocus);

    assert!(screen.attached.is_none());
    assert!(!screen.pane_focused);
    assert!(has_text(screen.worker_transcripts.get("w2").unwrap()));

    screen.apply_view(ViewCommand::TogglePaneFocus);
    screen.pane_step(1);
    screen.apply_view(ViewCommand::AttachWorker);
    assert!(has_text(&screen.attached.as_ref().unwrap().transcript));
    invariant(&screen);
}

#[test]
fn cycling_off_workers_detaches_without_re_attach_on_the_next_workers_mode() {
    let mut screen = attached_to_w2();

    screen.cycle_mode();
    assert!(screen.attached.is_none());
    assert_ne!(screen.pane_mode, PaneMode::Workers);
    invariant(&screen);

    screen.cycle_mode();
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert!(screen.attached.is_none());
    invariant(&screen);
}

#[test]
fn opening_output_detaches_and_switches_the_pane_to_output() {
    let mut screen = attached_to_w2();
    screen.open_output(OutputView {
        id: FoldId::of("output"),
        lines: vec!["output".into()],
        scroll: 0,
    });

    assert!(screen.attached.is_none());
    assert_eq!(screen.pane_mode, PaneMode::Output);
    invariant(&screen);
}

#[test]
fn attachment_owns_workers_through_settlement_while_auto_workers_demote() {
    let mut screen = attached_to_w2();
    screen.sync_workers(workers(BlockState::Done));

    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert_eq!(
        screen.attached.as_ref().map(|worker| worker.id.as_str()),
        Some("w2")
    );
    invariant(&screen);

    let mut auto = Screen::new(false);
    auto.sync_workers(workers(BlockState::Running));
    assert_eq!(auto.pane_mode, PaneMode::Workers);
    auto.sync_workers(workers(BlockState::Done));
    assert_eq!(auto.pane_mode, PaneMode::Ledger);
}

#[test]
fn the_attach_guard_invariant_holds_through_the_full_view_sequence() {
    let mut screen = attached_to_w2();
    invariant(&screen);

    screen.pane_step(1);
    invariant(&screen);
    screen.apply(&AgentEvent::TurnStarted, 2);
    invariant(&screen);
    screen.sync_workers(workers(BlockState::Running));
    invariant(&screen);
    screen.cycle_mode();
    invariant(&screen);
    screen.apply_view(ViewCommand::TogglePaneFocus);
    invariant(&screen);
    screen.apply_view(ViewCommand::TogglePaneFocus);
    invariant(&screen);
    screen.apply_view(ViewCommand::AttachWorker);
    assert!(screen.attached.is_none(), "LEDGER cannot attach a worker");
    invariant(&screen);
    screen.apply_view(ViewCommand::DetachWorker);
    invariant(&screen);
}
