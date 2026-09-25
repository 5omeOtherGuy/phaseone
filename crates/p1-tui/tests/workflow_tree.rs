//! The WORKERS pane's workflow tree (issue #198, ADR-0075): runs → phases → steps → the
//! step's worker, navigable like the flat list, from plain events the host fills.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use p1_contracts::{AgentEvent, ToolCall, ToolInput};
use p1_tui::input::{self, Action, Command, ViewCommand};
use p1_tui::render::workers::{self, BlockState, WorkerBlock};
use p1_tui::state::{PaneMode, PaneWidth, Screen};
use p1_tui::workflow::{RunStarted, StepEnded, StepStarted, WorkflowEvent};

fn worker(id: &str, state: BlockState, tokens: Option<u64>) -> WorkerBlock {
    WorkerBlock {
        id: id.into(),
        task: format!("task {id}"),
        route: "claude/opus".into(),
        model: None,
        state,
        elapsed: Some("0m03s".into()),
        cost_micro_usd: None,
        tokens,
        context_window: tokens.map(|_| 128_000),
        grants: String::new(),
        activity: String::new(),
    }
}

/// The fixtures' calls are `c<N>`, the N-th `agent()` call of the run.
fn ordinal(call: &str) -> u32 {
    call[1..].parse().expect("a c<N> call")
}

fn started(call: &str, label: &str, worker: &str) -> WorkflowEvent {
    WorkflowEvent::StepStarted(StepStarted {
        run: "wf1".into(),
        ordinal: ordinal(call),
        call: call.into(),
        label: Some(label.into()),
        phase: None,
        role: "reviewer".into(),
        model: "claude/opus:high".into(),
        worker_id: Some(worker.into()),
        attempt: 1,
        prompt: format!("prompt of {label}"),
    })
}

fn ended(call: &str, status: &str, attempts: u32, error: Option<&str>) -> WorkflowEvent {
    WorkflowEvent::StepEnded(StepEnded {
        run: "wf1".into(),
        ordinal: ordinal(call),
        call: call.into(),
        label: None,
        model: "claude/opus:high".into(),
        status: status.into(),
        attempts,
        replayed: false,
        error: error.map(str::to_string),
        worker_id: None,
    })
}

/// One run: `Plan` ended (two done steps), `Review` running with a running step and its
/// worker, a done step, a failed step on its second attempt and a replayed step; one job
/// still queued. A direct worker `w9` no step references.
fn screen() -> Screen {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    let run = |run: &str| run.to_string();
    let events = [
        (
            0,
            WorkflowEvent::RunStarted(RunStarted {
                id: run("wf1"),
                resumed_from: Some(run("wf0")),
            }),
        ),
        (
            10,
            WorkflowEvent::Phase {
                run: run("wf1"),
                name: "Plan".into(),
            },
        ),
        (20, started("c1", "plan", "w1")),
        (30, started("c2", "sketch", "w2")),
        (1_000, ended("c1", "done", 1, None)),
        (2_000, ended("c2", "done", 1, None)),
        (
            3_000,
            WorkflowEvent::JobsQueued {
                run: run("wf1"),
                count: 5,
            },
        ),
        (
            3_000,
            WorkflowEvent::Phase {
                run: run("wf1"),
                name: "Review".into(),
            },
        ),
        (3_100, started("c3", "review:bugs", "w3")),
        (3_200, started("c4", "review:docs", "w4")),
        (5_000, ended("c4", "done", 1, None)),
        (5_100, started("c5", "review:tests", "w5")),
        (
            6_000,
            ended("c5", "failed", 2, Some("invalid_output: bad\nsecond line")),
        ),
        (
            6_100,
            WorkflowEvent::StepEnded(StepEnded {
                run: run("wf1"),
                ordinal: 6,
                call: "c6".into(),
                label: Some("review:style".into()),
                model: "claude/opus:high".into(),
                status: "done".into(),
                attempts: 1,
                replayed: true,
                error: None,
                worker_id: None,
            }),
        ),
        (
            6_200,
            WorkflowEvent::Log {
                run: run("wf1"),
                text: "reviewing 5 files".into(),
            },
        ),
    ];
    for (at_ms, event) in events {
        screen.apply_workflow(event, at_ms);
    }
    screen.sync_workers(vec![
        worker("w1", BlockState::Done, Some(1_000)),
        worker("w2", BlockState::Done, None),
        worker("w3", BlockState::Running, Some(48_213)),
        worker("w4", BlockState::Done, Some(2_000)),
        worker("w5", BlockState::Done, Some(3_000)),
        worker("w9", BlockState::Running, None),
    ]);
    screen.apply_worker(
        "w3",
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "t1".into(),
                name: "read".into(),
                input: ToolInput::Json(r#"{"file_path":"src/lib.rs"}"#.into()),
            },
        },
        6_500,
    );
    screen.tick(7_000);
    screen
}

fn text(screen: &Screen, width: usize, height: Option<usize>) -> Vec<String> {
    workers::render_in(
        &screen.workers,
        width,
        width < 56,
        screen.stop_pending.as_deref(),
        height,
    )
    .iter()
    .map(ToString::to_string)
    .collect()
}

fn find<'a>(lines: &'a [String], needle: &str) -> &'a str {
    lines
        .iter()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no row with {needle:?}:\n{}", lines.join("\n")))
}

fn position(lines: &[String], needle: &str) -> usize {
    lines
        .iter()
        .position(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("no row with {needle:?}:\n{}", lines.join("\n")))
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

fn focus(screen: &mut Screen, key: &str) {
    if !screen.pane_focused {
        screen.apply_view(ViewCommand::TogglePaneFocus);
    }
    screen.workers.focused = Some(key.into());
}

#[test]
fn the_run_header_shows_its_phase_resume_and_all_five_counts() {
    let lines = text(&screen(), 60, None);
    let header = find(&lines, "wf1 · Review");
    assert!(header.contains("↺ wf0"), "{header}");
    assert!(header.contains("7 steps · 0m07s"), "{header}");
    let counts = find(&lines, "running ·");
    assert!(
        counts.contains("1 running · 4 done · 1 failed · 1 queued"),
        "{counts}"
    );
    // w1 1000 + w3 48213 + w4 2000 + w5 3000; w2's tokens are unknown.
    let sums = find(&lines, "tokens ");
    assert!(sums.contains("54.2k+?"), "{sums}");
    assert!(sums.contains("1+? calls"), "{sums}");
    assert!(find(&lines, "reviewing 5 files").starts_with("      reviewing"));
}

#[test]
fn a_phase_row_shows_done_of_known_and_the_current_phase_counts_the_queue() {
    let lines = text(&screen(), 60, None);
    // Review: c4 done and c6 replayed done, of four steps and one queued job.
    assert!(find(&lines, "▾ Review").contains("▾ Review 2/5"));
    assert!(find(&lines, "▾ Plan").contains("▾ Plan 2/2"));
}

#[test]
fn a_running_step_shows_its_tool_call_and_its_worker_block_beneath() {
    let lines = text(&screen(), 60, None);
    let step = position(&lines, "review:bugs");
    assert!(
        lines[step].contains("▪ review:bugs · claude/opus:high"),
        "{}",
        lines[step]
    );
    assert!(
        lines[step + 1].contains("↳ read src/lib.rs"),
        "{}",
        lines[step + 1]
    );
    assert!(
        lines[step + 1].contains("48.2k/128k · 1 calls"),
        "{}",
        lines[step + 1]
    );
    // The worker's own block, indented one level, right under the step.
    assert!(
        lines[step + 2].starts_with("      ▪ w3"),
        "{}",
        lines[step + 2]
    );
    // An ended step's worker folds into the step row: no block for w4.
    assert!(
        !lines.iter().any(|line| line.contains(" w4 ")),
        "{lines:#?}"
    );
}

#[test]
fn a_failed_step_names_its_attempts_and_error_and_a_replayed_step_says_so() {
    let lines = text(&screen(), 60, None);
    let failed = position(&lines, "review:tests");
    assert!(
        lines[failed].contains("✗ review:tests"),
        "{}",
        lines[failed]
    );
    assert!(lines[failed].contains("×2 · 0m00s"), "{}", lines[failed]);
    assert!(
        lines[failed + 1].contains("↳ invalid_output: bad")
            && !lines[failed + 1].contains("second"),
        "{}",
        lines[failed + 1]
    );
    let replayed = position(&lines, "review:style");
    assert!(
        lines[replayed].contains("↺ review:style"),
        "{}",
        lines[replayed]
    );
    assert!(
        lines[replayed].contains('—'),
        "unknown elapsed: {}",
        lines[replayed]
    );
    assert!(
        lines[replayed + 1].contains("replayed · done"),
        "{}",
        lines[replayed + 1]
    );
}

#[test]
fn a_direct_worker_stays_in_a_flat_group_below_the_runs() {
    let lines = text(&screen(), 60, None);
    let group = position(&lines, "    workers");
    assert!(group > position(&lines, "review:style"));
    assert!(position(&lines, "w9") > group);
}

#[test]
fn an_ended_phase_collapses_when_the_pane_is_short_but_never_the_running_one() {
    let screen = screen();
    let full = text(&screen, 48, None);
    assert!(find(&full, "Plan").contains("▾ Plan 2/2"));
    let short = text(&screen, 48, Some(full.len() - 1));
    assert!(find(&short, "Plan").contains("▸ Plan 2/2"), "{short:#?}");
    assert!(
        !short.iter().any(|line| line.contains("sketch")),
        "{short:#?}"
    );
    assert!(find(&short, "▾ Review").contains("▾ Review 2/5"));
    assert!(short.iter().any(|line| line.contains("review:bugs")));

    // The selection's phase stays open.
    let mut selected = screen;
    focus(&mut selected, "wf1/2");
    let short = text(&selected, 48, Some(full.len() - 1));
    assert!(find(&short, "Plan").contains("▾ Plan"), "{short:#?}");
}

#[test]
fn both_widths_fill_every_row_and_keep_unknowns_explicit() {
    let screen = screen();
    for width in [30, 48] {
        let lines = workers::render_in(&screen.workers, width, true, None, None);
        for line in &lines {
            assert_eq!(p1_tui::wrap::cell_width(&line.to_string()), width, "{line}");
        }
        let lines: Vec<String> = lines.iter().map(ToString::to_string).collect();
        let counts = find(&lines, "✓4");
        assert!(counts.contains("▪1 ✓4 ✗1 ·1 of 7"), "{width}: {counts}");
        assert!(lines.last().unwrap().contains("^F select   ⏎ open"));
    }
    // Below 48 a step is one row with the activity as its suffix (cut to fit).
    let narrow = text(&screen, 30, None);
    let step = position(&narrow, "▪ review:");
    assert!(narrow[step + 1].contains("▪ w3"), "{}", narrow[step + 1]);
    let narrow = text(&screen, 46, None);
    let step = position(&narrow, "review:bugs");
    assert!(narrow[step].contains("· read src/"), "{}", narrow[step]);
    assert!(!narrow[step + 1].contains('↳'), "{}", narrow[step + 1]);
    let wide = text(&screen, 48, None);
    let step = position(&wide, "review:bugs");
    assert!(wide[step + 1].contains("↳ read src/"), "{}", wide[step + 1]);
}

#[test]
fn selection_moves_across_run_headers_steps_and_workers() {
    let mut screen = screen();
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    screen.apply_view(ViewCommand::TogglePaneFocus);
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1"));
    let mut seen = vec!["wf1".to_string()];
    for _ in 0..10 {
        screen.pane_step(1);
        seen.push(screen.workers.focused.clone().unwrap());
    }
    assert_eq!(
        seen,
        [
            "wf1", "wf1/1", "wf1/2", "wf1/3", "w3", "wf1/4", "wf1/5", "wf1/6", "w9", "w9", "w9"
        ]
    );
    // The selected step row takes the focus fill with the Menu glyph.
    screen.pane_step(-5);
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1/3"));
    let lines = text(&screen, 60, None);
    assert!(find(&lines, "review:bugs").contains("▸ review:bugs"));
    // A sync that no longer lists a worker keeps a step selected.
    screen.sync_workers(vec![]);
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1/3"));
}

#[test]
fn a_attaches_a_steps_worker_and_enter_opens_the_step_with_its_prompt() {
    let mut screen = screen();
    focus(&mut screen, "wf1/3");
    assert_eq!(
        input::decide(&screen, key(KeyCode::Char('a'))),
        Some(Action::View(ViewCommand::AttachWorker))
    );
    screen.apply_view(ViewCommand::AttachWorker);
    let attached = screen.attached.as_ref().expect("attached");
    assert_eq!(attached.id, "w3");
    assert!(attached.step.is_none());
    screen.apply_view(ViewCommand::DetachWorker);

    // A run header attaches nothing.
    focus(&mut screen, "wf1");
    screen.apply_view(ViewCommand::AttachWorker);
    assert!(screen.attached.is_none());

    // ⏎ on a step opens it: a stats band, the prompt folded to three lines.
    let prompt = "one\ntwo\nthree\nfour\nfive";
    screen.apply_workflow(
        WorkflowEvent::StepStarted(StepStarted {
            prompt: prompt.into(),
            ..match started("c7", "fix", "w7") {
                WorkflowEvent::StepStarted(step) => step,
                _ => unreachable!(),
            }
        }),
        6_600,
    );
    focus(&mut screen, "wf1/7");
    assert_eq!(
        input::decide(&screen, key(KeyCode::Enter)),
        Some(Action::View(ViewCommand::OpenStep))
    );
    screen.apply_view(ViewCommand::OpenStep);
    let opened = screen.attached.as_ref().expect("opened");
    assert_eq!(opened.id, "w7");
    let step = opened.step.clone().expect("the step is open");
    let band: Vec<String> = workers::step_band(&screen.workers, &step.key, false, 100)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(
        band[0]
            .contains("fix · running · claude/opus:high · Review · ×1 · 0m00s · — tok · — calls"),
        "{band:#?}"
    );
    assert_eq!(band.len(), 5, "{band:#?}");
    assert!(band[3].contains("three") && band[4].contains("… 2 more lines · p expands"));

    // `p` expands the whole prompt.
    assert_eq!(
        input::decide(&screen, key(KeyCode::Char('p'))),
        Some(Action::View(ViewCommand::TogglePrompt))
    );
    screen.apply_view(ViewCommand::TogglePrompt);
    let step = screen.attached.as_ref().unwrap().step.clone().unwrap();
    assert!(step.prompt_expanded);
    let band: Vec<String> = workers::step_band(&screen.workers, &step.key, true, 100)
        .iter()
        .map(ToString::to_string)
        .collect();
    assert!(band.iter().any(|line| line.contains("five")), "{band:#?}");

    // A step with no worker yet opens nothing.
    screen.apply_view(ViewCommand::DetachWorker);
    focus(&mut screen, "wf1/6");
    screen.apply_view(ViewCommand::OpenStep);
    assert!(screen.attached.is_none());
}

#[test]
fn x_on_a_step_stops_its_worker_and_x_on_a_run_header_cancels_the_run() {
    let mut screen = screen();
    focus(&mut screen, "wf1/3");
    assert_eq!(
        input::decide(&screen, key(KeyCode::Char('x'))),
        Some(Action::View(ViewCommand::AskStopWorker))
    );
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending.as_deref(), Some("w3"));
    assert_eq!(
        input::decide(&screen, key(KeyCode::Char('y'))),
        Some(Action::Command(Command::StopWorker("w3".into())))
    );
    screen.apply_view(ViewCommand::KeepWorker);

    // An ended step has nothing to stop.
    focus(&mut screen, "wf1/4");
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending, None);

    focus(&mut screen, "wf1");
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending.as_deref(), Some("wf1"));
    let lines = text(&screen, 60, None);
    assert!(
        lines
            .last()
            .unwrap()
            .contains("cancel wf1?   y cancel   n keep")
    );
    // A worker refresh does not drop a pending run cancel.
    screen.sync_workers(vec![worker("w3", BlockState::Running, None)]);
    assert_eq!(screen.stop_pending.as_deref(), Some("wf1"));
    assert_eq!(
        input::decide(&screen, key(KeyCode::Char('y'))),
        Some(Action::Command(Command::CancelRun("wf1".into())))
    );
}

#[test]
fn a_run_start_promotes_an_unpinned_pane_and_its_end_demotes_it() {
    let run_started = || {
        WorkflowEvent::RunStarted(RunStarted {
            id: "wf1".into(),
            resumed_from: None,
        })
    };
    let run_ended = || {
        WorkflowEvent::RunEnded(p1_tui::workflow::RunEnded {
            id: "wf1".into(),
            outcome: "completed".into(),
            error: None,
        })
    };
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Off;
    assert_eq!(screen.pane_mode, PaneMode::Ledger);
    screen.apply_workflow(run_started(), 0);
    assert_eq!(screen.pane_mode, PaneMode::Workers);
    assert!(screen.worker_mode_auto);
    // The width force, as for a worker: a hidden pane opens and is given back.
    assert_eq!(screen.pane_width, PaneWidth::Wide);
    screen.apply_workflow(run_ended(), 1_000);
    assert_eq!(screen.pane_mode, PaneMode::Ledger);
    assert!(!screen.worker_mode_auto);
    assert_eq!(screen.pane_width, PaneWidth::Off);

    // A pinned pane stays where the operator pinned it.
    let mut pinned = Screen::new(true);
    pinned.pinned = true;
    pinned.apply_workflow(run_started(), 0);
    assert_eq!(pinned.pane_mode, PaneMode::Ledger);
    assert!(!pinned.worker_mode_auto);

    // The first step of a run the tree never saw start promotes too.
    let mut late = Screen::new(true);
    late.apply_workflow(started("c1", "plan", "w1"), 0);
    assert_eq!(late.pane_mode, PaneMode::Workers);
}

#[test]
fn a_moved_past_links_worker_stays_under_its_step_never_in_the_flat_group() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    let link = |worker: &str, model: &str| {
        WorkflowEvent::StepStarted(StepStarted {
            run: "wf1".into(),
            ordinal: 1,
            call: "c1".into(),
            label: Some("failover".into()),
            phase: None,
            role: "worker".into(),
            model: model.into(),
            worker_id: Some(worker.into()),
            attempt: 1,
            prompt: "fail over".into(),
        })
    };
    screen.apply_workflow(
        WorkflowEvent::RunStarted(RunStarted {
            id: "wf1".into(),
            resumed_from: None,
        }),
        0,
    );
    screen.apply_workflow(link("w2", "ds2/flash"), 10);
    // The engine reports the first link's start after its turn: the same worker.
    screen.apply_workflow(link("w2", "ds2/flash"), 20);
    screen.apply_workflow(link("w3", "cline2/flash"), 30);
    screen.apply_workflow(ended("c1", "done", 2, None), 3_000);
    screen.sync_workers(vec![
        worker("w2", BlockState::Failed, None),
        worker("w3", BlockState::Done, Some(1_400)),
    ]);
    screen.tick(3_000);

    let lines = text(&screen, 60, None);
    let step = position(&lines, "failover");
    assert!(
        lines[step].contains("failover · cline2/flash"),
        "{}",
        lines[step]
    );
    assert!(lines[step + 1].contains("↳ done"), "{}", lines[step + 1]);
    let moved = &lines[step + 2];
    assert!(
        moved.contains("↳ moved on · ds2/flash · route_failed") && moved.contains("w2"),
        "{moved}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("    workers")),
        "{lines:#?}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("✗ w2")),
        "{lines:#?}"
    );

    // Selectable and openable like any worker.
    screen.apply_view(ViewCommand::TogglePaneFocus);
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1/1"));
    screen.pane_step(1);
    assert_eq!(screen.workers.focused.as_deref(), Some("w2"));
    let lines = text(&screen, 60, None);
    assert!(find(&lines, "moved on").contains("▸ moved on"));
    screen.apply_view(ViewCommand::AttachWorker);
    assert_eq!(screen.attached.as_ref().map(|w| w.id.as_str()), Some("w2"));
}

/// The operator's exact keys (issue #198 live check): `^F`, `↓` to the first step, `⏎`,
/// `p`, `esc` — each through `input::decide`, as the host's driver applies them. The
/// opened step heads the TRANSCRIPT area (not the side pane).
#[test]
fn the_operators_keys_open_a_step_expand_its_prompt_and_esc_closes_it() {
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn press(screen: &mut Screen, key: KeyEvent) {
        match input::decide(screen, key) {
            Some(Action::View(view)) => screen.apply_view(view),
            Some(Action::Command(Command::PaneDown)) => screen.pane_step(1),
            Some(Action::Command(Command::PaneUp)) => screen.pane_step(-1),
            other => panic!("{key:?} decided {other:?}"),
        }
    }

    fn transcript_rows(screen: &mut Screen, w: u16, h: u16) -> (u16, Vec<String>) {
        let mut terminal = Terminal::new(TestBackend::new(w, h)).unwrap();
        terminal
            .draw(|frame| {
                p1_tui::render::screen::draw(screen, frame.area(), frame.buffer_mut(), 7_000)
            })
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let area = p1_tui::geometry::layout(w, h, screen.pane_width, false, 1).transcript;
        let rows = (area.y..area.y + 6)
            .map(|y| {
                (area.x..area.right())
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect();
        (area.y, rows)
    }

    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    screen.apply_workflow(
        WorkflowEvent::RunStarted(RunStarted {
            id: "wf1".into(),
            resumed_from: None,
        }),
        0,
    );
    screen.apply_workflow(
        WorkflowEvent::Phase {
            run: "wf1".into(),
            name: "Review".into(),
        },
        0,
    );
    screen.apply_workflow(
        WorkflowEvent::StepStarted(StepStarted {
            prompt: "one\ntwo\nthree\nfour\nfive".into(),
            ..match started("c1", "review:bugs", "w3") {
                WorkflowEvent::StepStarted(step) => step,
                _ => unreachable!(),
            }
        }),
        100,
    );
    screen.sync_workers(vec![worker("w3", BlockState::Running, Some(2_000))]);
    screen.tick(7_000);

    press(
        &mut screen,
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
    );
    // Focus starts on the run header.
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1"));
    press(&mut screen, key(KeyCode::Down));
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1/1"));
    press(&mut screen, key(KeyCode::Enter));
    let opened = screen.attached.as_ref().expect("the step is open");
    assert_eq!(opened.id, "w3");
    assert_eq!(
        opened.step.as_ref().map(|step| step.key.as_str()),
        Some("wf1/1")
    );

    // The band and the folded prompt head the transcript area, at both widths.
    for (w, h) in [(175, 42), (60, 30)] {
        let (top, rows) = transcript_rows(&mut screen, w, h);
        assert_eq!(top, 1, "{w}");
        assert!(rows[0].contains("attached w3"), "{w}: {rows:#?}");
        assert!(
            rows[1].contains("review:bugs · running · claude/opus:high · Review"),
            "{w}: {rows:#?}"
        );
        assert!(
            rows[2].contains("one") && rows[4].contains("three"),
            "{w}: {rows:#?}"
        );
        assert!(
            rows[5].contains("… 2 more lines · p expands"),
            "{w}: {rows:#?}"
        );
    }

    press(&mut screen, key(KeyCode::Char('p')));
    assert!(
        screen
            .attached
            .as_ref()
            .unwrap()
            .step
            .as_ref()
            .unwrap()
            .prompt_expanded
    );
    let (_, rows) = transcript_rows(&mut screen, 175, 42);
    assert!(rows[5].contains("four"), "{rows:#?}");

    press(&mut screen, key(KeyCode::Esc));
    assert!(screen.attached.is_none());

    // `⏎` on the run header opens its running step; on the step's worker block too.
    press(&mut screen, key(KeyCode::Up));
    assert_eq!(screen.workers.focused.as_deref(), Some("wf1"));
    press(&mut screen, key(KeyCode::Enter));
    let opened = screen.attached.as_ref().expect("opened from the header");
    assert_eq!(
        opened.step.as_ref().map(|step| step.key.as_str()),
        Some("wf1/1")
    );
    press(&mut screen, key(KeyCode::Esc));
    press(&mut screen, key(KeyCode::Down));
    press(&mut screen, key(KeyCode::Down));
    assert_eq!(screen.workers.focused.as_deref(), Some("w3"));
    press(&mut screen, key(KeyCode::Enter));
    assert!(screen.attached.as_ref().unwrap().step.is_some());
}

/// ADR-0075: two concurrent steps with one call id (same label, prompt and options) are
/// two rows, keyed by their ordinals — each with its own worker, and a replay of the same
/// call is a third row.
#[test]
fn two_steps_sharing_a_call_id_are_two_rows_with_their_own_workers() {
    let mut screen = Screen::new(true);
    screen.pane_width = PaneWidth::Wide;
    let vote = |ordinal: u32, worker: &str| {
        WorkflowEvent::StepStarted(StepStarted {
            run: "wf1".into(),
            ordinal,
            call: "vote".into(),
            label: None,
            phase: None,
            role: "worker".into(),
            model: "claude/opus".into(),
            worker_id: Some(worker.into()),
            attempt: 1,
            prompt: "vote".into(),
        })
    };
    let end = |ordinal: u32, replayed: bool| {
        WorkflowEvent::StepEnded(StepEnded {
            run: "wf1".into(),
            ordinal,
            call: "vote".into(),
            label: None,
            model: "claude/opus".into(),
            status: "done".into(),
            attempts: u32::from(!replayed),
            replayed,
            error: None,
            worker_id: None,
        })
    };
    screen.apply_workflow(
        WorkflowEvent::RunStarted(RunStarted {
            id: "wf1".into(),
            resumed_from: None,
        }),
        0,
    );
    screen.apply_workflow(
        WorkflowEvent::JobsQueued {
            run: "wf1".into(),
            count: 2,
        },
        0,
    );
    screen.apply_workflow(vote(1, "w1"), 10);
    screen.apply_workflow(vote(2, "w2"), 20);
    // The engine's own report of the first start, after its turn: an update.
    screen.apply_workflow(vote(1, "w1"), 30);
    screen.sync_workers(vec![
        worker("w1", BlockState::Running, None),
        worker("w2", BlockState::Running, None),
    ]);
    screen.tick(1_000);

    let lines = text(&screen, 60, None);
    assert!(
        find(&lines, "running ·").contains("2 running · 0 done · 0 failed · 0 queued"),
        "{lines:#?}"
    );
    let rows: Vec<usize> = lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.contains("▪ vote"))
        .map(|(index, _)| index)
        .collect();
    assert_eq!(rows.len(), 2, "{lines:#?}");
    let (w1, w2) = (position(&lines, "▪ w1"), position(&lines, "▪ w2"));
    assert!(rows[0] < w1 && w1 < rows[1] && rows[1] < w2, "{lines:#?}");
    assert!(
        !lines.iter().any(|line| line.contains("    workers")),
        "{lines:#?}"
    );

    screen.apply_workflow(end(2, false), 2_000);
    screen.apply_workflow(end(1, false), 2_100);
    let lines = text(&screen, 60, None);
    assert_eq!(
        lines.iter().filter(|line| line.contains("✓ vote")).count(),
        2,
        "{lines:#?}"
    );
    assert!(find(&lines, "running ·").contains("0 running · 2 done"));

    // A replay of the same call is its own step, by its own ordinal.
    screen.apply_workflow(end(3, true), 2_200);
    let run = screen.workers.tree.run("wf1").unwrap();
    let ordinals: Vec<u32> = run.steps().map(|step| step.ordinal).collect();
    assert_eq!(ordinals, [1, 2, 3]);
    assert!(run.steps().nth(2).unwrap().replayed);
}

/// ADR-0075: with a worker attached, `x` on a focused run header asks to cancel the run.
#[test]
fn x_on_a_run_header_cancels_the_run_even_with_a_worker_attached() {
    let mut screen = screen();
    focus(&mut screen, "wf1/3");
    screen.apply_view(ViewCommand::AttachWorker);
    assert_eq!(screen.attached.as_ref().map(|w| w.id.as_str()), Some("w3"));
    focus(&mut screen, "wf1");
    assert_eq!(
        input::decide(&screen, key(KeyCode::Char('x'))),
        Some(Action::View(ViewCommand::AskStopWorker))
    );
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending.as_deref(), Some("wf1"));
    let lines = text(&screen, 60, None);
    assert!(
        lines
            .last()
            .unwrap()
            .contains("cancel wf1?   y cancel   n keep")
    );

    // On a worker row the attached worker is still the one `x` stops.
    screen.apply_view(ViewCommand::KeepWorker);
    focus(&mut screen, "w9");
    screen.apply_view(ViewCommand::AskStopWorker);
    assert_eq!(screen.stop_pending.as_deref(), Some("w3"));
}

/// ADR-0075 item 4: a run cancelled mid-fan-out keeps the jobs it never started counted.
#[test]
fn a_run_cancelled_with_jobs_queued_keeps_them_as_never_run() {
    let mut screen = Screen::new(true);
    screen.apply_workflow(
        WorkflowEvent::RunStarted(RunStarted {
            id: "wf1".into(),
            resumed_from: None,
        }),
        0,
    );
    screen.apply_workflow(
        WorkflowEvent::JobsQueued {
            run: "wf1".into(),
            count: 5,
        },
        0,
    );
    screen.apply_workflow(started("c1", "first", "w1"), 10);
    screen.apply_workflow(ended("c1", "cancelled", 1, None), 500);
    screen.apply_workflow(
        WorkflowEvent::RunEnded(p1_tui::workflow::RunEnded {
            id: "wf1".into(),
            outcome: "cancelled".into(),
            error: Some("cancelled".into()),
        }),
        600,
    );
    let run = screen.workers.tree.run("wf1").unwrap();
    assert_eq!((run.queued, run.never_run, run.total()), (0, 4, 5));
    let lines = text(&screen, 60, None);
    assert!(find(&lines, "wf1 ·").contains("5 steps"), "{lines:#?}");
    assert!(find(&lines, "running ·").contains("0 queued"), "{lines:#?}");
    assert!(
        find(&lines, "never run").contains("4 never run"),
        "{lines:#?}"
    );
}

/// Unknown stays `—`: a step's tool calls are unknown until its worker's stream reached
/// the TUI, and a sum over such a step says so.
#[test]
fn a_steps_calls_are_unknown_until_its_workers_stream_arrives() {
    let mut screen = Screen::new(true);
    screen.apply_workflow(started("c1", "first", "w1"), 10);
    screen.sync_workers(vec![worker("w1", BlockState::Running, Some(1_000))]);
    let lines = text(&screen, 60, None);
    let step = position(&lines, "▪ first");
    assert!(
        lines[step + 1].contains("1.0k/128k · — calls"),
        "{}",
        lines[step + 1]
    );
    assert!(find(&lines, "tokens ").contains("— calls"), "{lines:#?}");
    screen.apply_worker("w1", &AgentEvent::TurnStarted, 20);
    let lines = text(&screen, 60, None);
    assert!(
        lines[step + 1].contains("1.0k/128k · 0 calls"),
        "{}",
        lines[step + 1]
    );
}
