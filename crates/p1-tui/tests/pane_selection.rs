//! Selection is a state transition, not a property of every worker poll.
use p1_tui::fold::FoldId;
use p1_tui::render::output::OutputView;
use p1_tui::render::workers::{BlockState, WorkerBlock};
use p1_tui::state::{PaneMode, PaneWidth, Screen};

fn worker(id: &str, state: BlockState) -> WorkerBlock {
    WorkerBlock {
        id: id.into(),
        task: "task".into(),
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

fn refresh(screen: &mut Screen, id: &str, state: BlockState) {
    // Each poll delivers newly constructed rows, not necessarily a new event.
    screen.sync_workers(vec![worker(id, state)]);
}

fn output() -> OutputView {
    OutputView {
        id: FoldId::of("call"),
        lines: vec!["tool output".into()],
        scroll: 0,
    }
}

#[test]
fn running_refreshes_do_not_reverse_ctrl_tab_or_hide_tool_output() {
    let mut s = Screen::new(false);
    refresh(&mut s, "w1", BlockState::Running);
    assert_eq!(s.pane_mode, PaneMode::Workers, "new worker gets attention");
    s.open_output(output());
    for _ in 0..4 {
        refresh(&mut s, "w1", BlockState::Running);
        assert_eq!(s.pane_mode, PaneMode::Output);
        assert_eq!(s.output.as_ref().unwrap().lines[0], "tool output");
    }
    s.cycle_mode(); // OUTPUT -> WORKERS
    assert_eq!(s.pane_mode, PaneMode::Workers);
    s.cycle_mode(); // WORKERS -> LEDGER
    for _ in 0..4 {
        refresh(&mut s, "w1", BlockState::Running);
        assert_eq!(s.pane_mode, PaneMode::Ledger);
    }
    // A genuinely new worker is attention, but the old worker's polls are not.
    s.sync_workers(vec![
        worker("w1", BlockState::Running),
        worker("w2", BlockState::Running),
    ]);
    assert_eq!(s.pane_mode, PaneMode::Workers);
    s.cycle_mode(); // WORKERS -> LEDGER
    s.cycle_mode(); // LEDGER -> OUTPUT
    for _ in 0..3 {
        s.sync_workers(vec![
            worker("w1", BlockState::Running),
            worker("w2", BlockState::Running),
        ]);
        assert_eq!(s.pane_mode, PaneMode::Output);
    }
}

#[test]
fn completed_rows_can_be_selected_and_remain_selected_after_polling() {
    let mut s = Screen::new(false);
    refresh(&mut s, "w1", BlockState::Running);
    s.cycle_mode(); // WORKERS -> LEDGER
    s.cycle_mode(); // LEDGER -> WORKERS, now deliberately selected
    for _ in 0..3 {
        refresh(&mut s, "w1", BlockState::Running);
        assert_eq!(s.pane_mode, PaneMode::Workers);
    }
    refresh(&mut s, "w1", BlockState::Done);
    assert_eq!(
        s.pane_mode,
        PaneMode::Workers,
        "manual selection outlives settlement"
    );
    s.cycle_mode(); // WORKERS -> LEDGER
    refresh(&mut s, "w2", BlockState::Running);
    assert_eq!(s.pane_mode, PaneMode::Workers);
    refresh(&mut s, "w2", BlockState::Done);
    assert_eq!(s.pane_mode, PaneMode::Ledger, "auto-selection settles");
    s.cycle_mode(); // LEDGER -> WORKERS
    for _ in 0..4 {
        refresh(&mut s, "w1", BlockState::Done);
        assert_eq!(s.pane_mode, PaneMode::Workers);
    }
    s.sync_workers(vec![]);
    assert_eq!(s.pane_mode, PaneMode::Workers);
    assert!(s.available_modes().contains(&PaneMode::Workers));
    s.cycle_mode(); // WORKERS -> LEDGER
    s.cycle_mode(); // LEDGER -> WORKERS
    s.sync_workers(vec![]);
    assert_eq!(s.pane_mode, PaneMode::Workers);
}

#[test]
fn review_attention_self_pins_once_and_new_review_reclaims_attention() {
    let mut s = Screen::new(false);
    refresh(&mut s, "w1", BlockState::NeedsReview);
    assert_eq!(s.pane_mode, PaneMode::Workers);
    assert!(s.pinned);
    s.cycle_mode(); // deliberate LEDGER, even though review remains parked
    for _ in 0..3 {
        refresh(&mut s, "w1", BlockState::NeedsReview);
        assert_eq!(s.pane_mode, PaneMode::Ledger);
        assert!(s.pinned);
    }
    s.sync_workers(vec![
        worker("w1", BlockState::NeedsReview),
        worker("w2", BlockState::NeedsReview),
    ]);
    assert_eq!(s.pane_mode, PaneMode::Workers);
    assert!(s.pinned);
    s.sync_workers(vec![
        worker("w1", BlockState::Done),
        worker("w2", BlockState::Done),
    ]);
    assert_eq!(
        s.pane_mode,
        PaneMode::Workers,
        "review pin survives settlement"
    );
    s.toggle_pin();
    refresh(&mut s, "w1", BlockState::Done);
    assert_eq!(
        s.pane_mode,
        PaneMode::Ledger,
        "auto mode falls back on unpin"
    );
}

#[test]
fn pin_and_width_are_operator_controls_not_poll_side_effects() {
    let mut s = Screen::new(false);
    s.pane_width = PaneWidth::Off;
    s.toggle_pin();
    refresh(&mut s, "w1", BlockState::Running);
    assert_eq!(
        (s.pane_mode, s.pane_width),
        (PaneMode::Ledger, PaneWidth::Off)
    );
    s.toggle_pin();
    refresh(&mut s, "w1", BlockState::Running);
    assert_eq!(
        (s.pane_mode, s.pane_width),
        (PaneMode::Ledger, PaneWidth::Off)
    );

    // A fresh live worker can open a hidden, unpinned pane; finishing restores
    // the pre-promotion width unless the operator explicitly changed it.
    refresh(&mut s, "w2", BlockState::Running);
    assert_eq!(
        (s.pane_mode, s.pane_width),
        (PaneMode::Workers, PaneWidth::Wide)
    );
    s.cycle_mode();
    refresh(&mut s, "w2", BlockState::Done);
    assert_eq!(
        (s.pane_mode, s.pane_width),
        (PaneMode::Ledger, PaneWidth::Off)
    );

    refresh(&mut s, "w3", BlockState::Running);
    for _ in 0..4 {
        s.cycle_width(); // Wide -> Split -> Off -> Narrow -> Wide
    }
    refresh(&mut s, "w3", BlockState::Done);
    assert_eq!(
        (s.pane_mode, s.pane_width),
        (PaneMode::Ledger, PaneWidth::Wide)
    );
    s.cycle_width(); // Split, another explicit width
    refresh(&mut s, "w4", BlockState::Running);
    s.toggle_pin();
    refresh(&mut s, "w4", BlockState::Done);
    assert_eq!(
        (s.pane_mode, s.pane_width),
        (PaneMode::Workers, PaneWidth::Split)
    );

    let mut output_screen = Screen::new(false);
    output_screen.pane_width = PaneWidth::Off;
    refresh(&mut output_screen, "w1", BlockState::Running);
    output_screen.open_output(output());
    refresh(&mut output_screen, "w1", BlockState::Done);
    assert_eq!(
        (output_screen.pane_mode, output_screen.pane_width),
        (PaneMode::Output, PaneWidth::Wide),
        "opening tool output explicitly keeps it visible after settlement"
    );
}
