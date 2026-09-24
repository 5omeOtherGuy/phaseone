//! Lead-authored acceptance cases, frozen before inspecting either #92 candidate patch.

use p1_contracts::{AgentEvent, StopReason, TurnEnd};
use p1_tui::fold::FoldId;
use p1_tui::render::output::OutputView;
use p1_tui::render::workers::{BlockState, WorkerBlock};
use p1_tui::state::{PaneMode, PaneWidth, Screen};
use ratatui::buffer::Buffer;
use ratatui::layout::Rect;

fn worker(state: BlockState) -> WorkerBlock {
    WorkerBlock {
        id: "fixture-worker".into(),
        task: "synthetic task".into(),
        route: "synthetic/model".into(),
        state,
        elapsed: None,
        cost_micro_usd: None,
        grants: String::new(),
        activity: "synthetic activity".into(),
    }
}

fn refresh_and_draw(screen: &mut Screen, state: Option<BlockState>, now: u64) {
    screen.sync_workers(state.map(worker).into_iter().collect());
    let area = Rect::new(0, 0, 120, 40);
    let mut buffer = Buffer::empty(area);
    p1_tui::render::screen::draw(screen, area, &mut buffer, now);
}

#[test]
fn completed_history_stays_selected_through_parent_activity_and_refreshes() {
    let mut screen = Screen::new(true);
    screen.sync_workers(vec![worker(BlockState::Done)]);
    screen.cycle_mode();
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert!(!screen.pinned, "navigation must not require a pin");
    screen.apply(&AgentEvent::TurnStarted, 0);
    for tick in 1..=4 {
        refresh_and_draw(&mut screen, Some(BlockState::Done), tick * 50);
        assert_eq!(screen.pane_mode, PaneMode::Workers);
    }
    screen.apply(
        &AgentEvent::TurnFinished {
            end: TurnEnd::Completed {
                stop: StopReason::EndTurn,
            },
        },
        250,
    );
    refresh_and_draw(&mut screen, None, 300);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
}

#[test]
fn explicit_ledger_and_output_survive_running_worker_refreshes() {
    let mut screen = Screen::new(true);
    screen.sync_workers(vec![worker(BlockState::Running)]);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    screen.cycle_mode();
    for tick in 1..=3 {
        refresh_and_draw(&mut screen, Some(BlockState::Running), tick * 50);
        assert_eq!(screen.pane_mode, PaneMode::Ledger);
    }
    screen.open_output(OutputView {
        id: FoldId::of("synthetic-output"),
        lines: vec!["synthetic output".into()],
        scroll: 0,
    });
    for state in [BlockState::Running, BlockState::Done] {
        refresh_and_draw(&mut screen, Some(state), 250);
        assert_eq!(screen.pane_mode, PaneMode::Output);
    }
}

#[test]
fn explicit_workers_survive_completion_without_becoming_pinned() {
    let mut screen = Screen::new(true);
    screen.sync_workers(vec![worker(BlockState::Running)]);
    screen.cycle_mode();
    screen.cycle_mode();
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    refresh_and_draw(&mut screen, Some(BlockState::Done), 50);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert!(!screen.pinned);
}

#[test]
fn automatic_promotion_still_restores_hidden_width_without_operator_override() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Off;
    screen.sync_workers(vec![worker(BlockState::Running)]);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert_eq!(screen.pane_width, PaneWidth::Wide);
    screen.sync_workers(vec![worker(BlockState::Done)]);
    assert_eq!(screen.pane_mode, PaneMode::Ledger);
    assert_eq!(screen.pane_width, PaneWidth::Off);
    assert_eq!(screen.promotion_saved_width, None);
}

#[test]
fn explicit_hide_is_not_undone_by_the_same_worker_snapshot() {
    let mut screen = Screen::new(true);
    screen.sync_workers(vec![worker(BlockState::Running)]);
    screen.pane_width = PaneWidth::Wide;
    screen.cycle_width();
    screen.cycle_width();
    assert_eq!(screen.pane_width, PaneWidth::Off);
    refresh_and_draw(&mut screen, Some(BlockState::Running), 50);
    assert_eq!(screen.pane_width, PaneWidth::Off);
}

#[test]
fn new_review_still_demands_attention_and_self_pins() {
    let mut screen = Screen::new(true);
    screen.sync_workers(vec![worker(BlockState::Running)]);
    screen.cycle_mode();
    assert_eq!(screen.pane_mode, PaneMode::Ledger);
    screen.sync_workers(vec![worker(BlockState::NeedsReview)]);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert!(screen.pinned);
    screen.sync_workers(vec![worker(BlockState::Done)]);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert!(screen.pinned);
}

#[test]
fn pin_still_blocks_ordinary_live_worker_promotion() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Off;
    screen.toggle_pin();
    screen.sync_workers(vec![worker(BlockState::Running)]);
    assert_eq!(screen.pane_mode, PaneMode::Ledger);
    assert_eq!(screen.pane_width, PaneWidth::Off);
    assert!(screen.pinned);
}
