//! Driver tests: no TTY, no terminal — the driver over plain method calls.

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A driver with an `--ask`-off policy and no wiring behind it.
fn driver() -> (Driver, mpsc::UnboundedReceiver<AuthRequest>) {
    let (policy, auth) = TuiPolicy::new(false, CancellationToken::new());
    // A minimal real agent, for its inbox handle.
    let agent = Agent::new(p1_core::AgentParts {
        provider: Arc::new(p1_testkit::ScriptedProvider::new(vec![])),
        tools: vec![],
        system_prompt: String::new(),
        options: p1_contracts::ModelOptions::default(),
        context: Arc::new(crate::run::DefaultContext),
        authorization: Arc::new(crate::policy::HostPolicy::new(
            false,
            false,
            Arc::new(crate::StdinLines::new()),
            Arc::new(std::sync::Mutex::new(Box::new(std::io::sink()))),
            CancellationToken::new(),
        )),
        journal: Arc::new(p1_journal::MemoryJournal::new()),
        events: Arc::new(p1_tui::runtime::TuiSink::new().0),
    })
    .expect("agent builds");
    (
        Driver {
            screen: Screen::new(true),
            env: "claude".into(),
            route: "claude".into(),
            model: "sonnet-4.5".into(),
            workspace: std::env::current_dir().unwrap(),
            sandbox: "off".into(),
            policy: Arc::new(policy),
            pending_auth: VecDeque::new(),
            pinned_by_approval: false,
            follow_ups: VecDeque::new(),
            submit_pending: None,
            worker_rows: Arc::new(Mutex::new(Vec::new())),
            pending_calls: HashMap::new(),
            task_files: HashSet::new(),
            task_added: 0,
            task_removed: 0,
            exit: None,
            inbox: agent.inbox(),
            _workers: None,
            now_ms: 0,
            last_cancel_ms: None,
            filter_before: String::new(),
            scroll_before: 0,
            unknown_confirm: None,
            term_out: Box::new(std::io::sink()),
            ask: false,
            pending_decision: None,
            last_turn_end_ms: None,
            last_decision_ms: None,
            inbox_hold: false,
            exit_after_turn: false,
            ui_tx: None,
            reviewed: HashMap::new(),
        },
        auth,
    )
}

#[test]
fn typing_and_enter_submits_a_prompt() {
    let (mut d, _auth) = driver();
    for c in "fix it".chars() {
        d.on_key(key(KeyCode::Char(c)));
    }
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.submit_pending.as_deref(), Some("fix it"));
    assert_eq!(d.screen.composer.text, "");
    // The operator line is in the transcript.
    assert!(matches!(
        d.screen.transcript.blocks[0],
        p1_tui::transcript::Block::Operator { .. }
    ));
}

#[test]
fn slash_exit_quits_and_focus_toggles() {
    let (mut d, _auth) = driver();
    for c in "/focus".chars() {
        d.on_key(key(KeyCode::Char(c)));
    }
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.screen.focus_explicit, Some(true));
    assert_eq!(d.submit_pending, None);
    for c in "/exit".chars() {
        d.on_key(key(KeyCode::Char(c)));
    }
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.exit, Some(0));
}

#[test]
fn enter_while_working_queues_steering_to_the_inbox() {
    let (mut d, agent) = driver_with_agent();
    d.screen.working = Some(p1_tui::state::Working {
        label: "shell".into(),
        started_ms: 0,
    });
    for c in "use vecdeque".chars() {
        d.on_key(key(KeyCode::Char(c)));
    }
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.submit_pending, None, "steering never starts a turn");
    assert_eq!(d.screen.queued.len(), 1);
    assert_eq!(d.screen.queued[0].text, "use vecdeque");
    // The steering actually reached the agent's inbox (the earlier version of
    // this test dropped the agent and never noticed the send failing).
    assert!(agent.has_pending_inbox());
}

/// A driver plus the agent whose inbox handle it holds.
fn driver_with_agent() -> (Driver, Agent) {
    let (mut d, _auth) = driver();
    let agent = Agent::new(p1_core::AgentParts {
        provider: Arc::new(p1_testkit::ScriptedProvider::new(vec![])),
        tools: vec![],
        system_prompt: String::new(),
        options: p1_contracts::ModelOptions::default(),
        context: Arc::new(crate::run::DefaultContext),
        authorization: Arc::new(crate::policy::HostPolicy::new(
            false,
            false,
            Arc::new(crate::StdinLines::new()),
            Arc::new(std::sync::Mutex::new(Box::new(std::io::sink()))),
            CancellationToken::new(),
        )),
        journal: Arc::new(p1_journal::MemoryJournal::new()),
        events: Arc::new(p1_tui::runtime::TuiSink::new().0),
    })
    .expect("agent builds");
    d.inbox = agent.inbox();
    (d, agent)
}

#[tokio::test]
async fn a_cancelled_turn_denies_its_parked_approval() {
    let (policy, mut auth_rx) = TuiPolicy::new(true, CancellationToken::new());
    let turn = CancellationToken::new();
    policy.set_turn(Some(turn.clone()));
    let call = p1_contracts::ToolCall {
        call_id: "c1".into(),
        name: "shell".into(),
        input: p1_contracts::ToolInput::Json("{}".into()),
    };
    let identity = p1_contracts::ToolIdentity {
        implementation: "shell".into(),
        variant: String::new(),
    };
    let pending = tokio::spawn(async move {
        policy
            .authorize(p1_contracts::AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect: p1_contracts::Effect::Executes,
            })
            .await
    });
    let _request = auth_rx.recv().await.unwrap();
    turn.cancel();
    assert_eq!(
        pending.await.unwrap(),
        Decision::Deny {
            reason: p1_tui::runtime::CANCEL_DENY.into()
        }
    );
}

#[test]
fn a_turn_end_offers_the_oldest_follow_up() {
    let (mut d, _auth) = driver();
    d.follow_ups.push_back("next step".into());
    d.screen.queue(true, "next step".into());
    d.note_turn_end(&TurnEnd::Completed {
        stop: p1_contracts::StopReason::EndTurn,
    });
    assert_eq!(d.take_follow_up().as_deref(), Some("next step"));
    assert!(d.follow_ups.is_empty());
    assert!(d.screen.queued.is_empty());
}

#[test]
fn cancel_clears_the_follow_up_queue() {
    let (mut d, _auth) = driver();
    d.follow_ups.push_back("next step".into());
    d.screen.queue(true, "next step".into());
    d.note_turn_end(&TurnEnd::Cancelled);
    assert_eq!(d.take_follow_up(), None);
    assert!(d.screen.queued.is_empty());
}

#[tokio::test]
async fn an_auth_request_becomes_the_approval_view_and_answers() {
    let (policy, mut auth_rx) = TuiPolicy::new(true, CancellationToken::new());
    let (mut d, _auth) = driver();
    d.policy = Arc::new(policy);
    let call = p1_contracts::ToolCall {
        call_id: "c1".into(),
        name: "shell".into(),
        input: p1_contracts::ToolInput::Json("{\"command\":\"cargo test\"}".into()),
    };
    let identity = p1_contracts::ToolIdentity {
        implementation: "shell".into(),
        variant: String::new(),
    };
    let pending = tokio::spawn({
        let policy = d.policy.clone();
        let call = call.clone();
        let identity = identity.clone();
        async move {
            policy
                .authorize(p1_contracts::AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect: p1_contracts::Effect::Executes,
                })
                .await
        }
    });
    let request = auth_rx.recv().await.unwrap();
    d.on_auth(request);
    assert!(matches!(d.screen.approval, Some(Approval::Permission(_))));
    assert!(d.screen.pinned);
    // A decision needs the approval to have been on screen a moment, and it
    // commits after a beat without another key.
    d.now_ms = 1_000;
    d.on_key(key(KeyCode::Char('y')));
    assert!(d.screen.approval.is_some(), "held for its beat");
    assert!(
        d.screen
            .flash
            .as_ref()
            .unwrap()
            .0
            .contains("allow once in a moment")
    );
    d.now_ms = 1_450;
    d.commit_decision();
    assert_eq!(pending.await.unwrap(), Decision::Permit);
    assert!(d.screen.approval.is_none());
    assert!(!d.screen.pinned);
}

#[test]
fn worker_events_stay_out_of_the_parent_transcript_but_mark_start_and_end() {
    let (mut d, _auth) = driver();
    d.on_ui_event(UiEvent::WorkerStarted("w1".into()));
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 0,
        worker: Some("w1".into()),
        event: p1_contracts::AgentEvent::TextDelta {
            text: "worker prose".into(),
        },
    }));
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 1,
        worker: Some("w1".into()),
        event: p1_contracts::AgentEvent::TurnFinished {
            end: TurnEnd::Completed {
                stop: p1_contracts::StopReason::EndTurn,
            },
        },
    }));
    let texts: Vec<String> = d
        .screen
        .transcript
        .blocks
        .iter()
        .filter_map(|b| match b {
            p1_tui::transcript::Block::Meta { text } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(texts, vec!["↳ w1 started", "↳ w1 finished"]);
    // No prose leaked into the parent's transcript.
    assert!(
        !d.screen
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, p1_tui::transcript::Block::Prose { .. }))
    );
}

#[test]
fn a_live_worker_promotes_the_pane_and_a_finished_one_releases_it() {
    use p1_tui::render::workers::{WorkerRow, WorkerState};
    use p1_tui::state::{PaneMode, PaneWidth};
    let (mut d, _auth) = driver();
    let row = |state: WorkerState| WorkerRow {
        id: "w1".into(),
        summary: "w1".into(),
        route: "deepseek/v4.1-flash".into(),
        state,
        elapsed: None,
        cost_micro_usd: None,
        details: vec![],
    };
    d.screen.pane_width = PaneWidth::Off;
    d.screen.sync_workers(vec![row(WorkerState::Running)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Workers);
    assert_eq!(d.screen.pane_width, PaneWidth::Ch56);
    // All done: an unpinned WORKERS pane falls back to the ledger.
    d.screen.sync_workers(vec![row(WorkerState::Done)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Ledger);
    // …unless it is pinned.
    d.screen.pane_mode = PaneMode::Workers;
    d.screen.pinned = true;
    d.screen.sync_workers(vec![row(WorkerState::Running)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Workers);
    d.screen.sync_workers(vec![row(WorkerState::Done)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Workers, "pinning always wins");
}

#[test]
fn block_shift_enter_and_atomic_paste_never_submit() {
    let (mut d, _) = driver();
    d.on_input(UiInput::Paste("line one\r\nline two".into()));
    assert_eq!(d.screen.composer.text, "line one\nline two");
    assert!(d.submit_pending.is_none());
    d.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::SHIFT));
    assert_eq!(d.screen.composer.text, "line one\nline two\n");
    assert!(d.submit_pending.is_none());
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.submit_pending.as_deref(), Some("line one\nline two\n"));
    assert_eq!(d.screen.input_history.len(), 1);
}

#[test]
fn pasted_approval_keys_are_data_never_authorization() {
    let (mut d, _) = driver();
    d.screen.approval = Some(Approval::Permission(PermissionView {
        tool: "shell".into(),
        command: "fixture".into(),
        rows: vec![],
        grantable: true,
    }));
    d.on_input(UiInput::Paste("y\na\np\nn\n".into()));
    // Still pending: the paste decided nothing. It is kept as draft text, not dropped.
    assert!(d.screen.approval.is_some());
    assert_eq!(d.pending_auth.len(), 0);
    assert_eq!(d.screen.composer.text, "y\na\np\nn\n");
}

#[test]
fn output_handles_open_close_and_preserve_the_composer_draft() {
    use p1_contracts::{AgentEvent, ToolCall, ToolInput, ToolResultItem, ToolStatus};
    let (mut d, _) = driver();
    d.screen.transcript.apply(
        &AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: "test".into(),
                name: "shell".into(),
                input: ToolInput::Json("{}".into()),
            },
        },
        None,
    );
    d.screen.transcript.apply(
        &AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id: "test".into(),
                name: "shell".into(),
                status: ToolStatus::Ok,
                content: "one\ntwo\n[exit code: 0]".into(),
            },
        },
        Some(1),
    );
    d.screen.composer.insert_text("unfinished draft");
    d.on_key(KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL));
    assert!(d.screen.output_focus);
    d.on_key(key(KeyCode::Down));
    assert_eq!(d.screen.output.as_ref().unwrap().scroll, 1);
    d.on_key(key(KeyCode::Esc));
    assert!(!d.screen.output_focus);
    assert_eq!(d.screen.composer.text, "unfinished draft");
    // A two-line output is shown whole inline: it has a handle, but it is
    // not the "latest fold" ^O and `newer` point at.
    assert!(d.screen.transcript.latest_fold.is_none());
    let id = match d.screen.transcript.blocks.last() {
        Some(p1_tui::transcript::Block::Call(row)) => row.output_id.clone().unwrap().to_string(),
        _ => panic!("the call's block"),
    };
    d.slash("open", &id);
    assert!(d.screen.output_focus);
    assert_eq!(d.screen.output.as_ref().unwrap().lines[0], "one");
}

#[test]
fn filter_navigation_clamps_and_ledger_overlay_dismisses() {
    let (mut d, _) = driver();
    d.screen.open_output(p1_tui::render::output::OutputView {
        id: p1_tui::fold::FoldId("h-1234".into()),
        lines: vec!["a matching row".into(), "other".into()],
        scroll: 0,
    });
    d.on_key(key(KeyCode::Char('/')));
    for c in "matching".chars() {
        d.on_key(key(KeyCode::Char(c)));
    }
    assert!(d.screen.output_search);
    assert_eq!(d.screen.output_filter, "matching");
    d.on_key(key(KeyCode::Enter));
    d.on_key(key(KeyCode::Down));
    assert_eq!(d.screen.output.as_ref().unwrap().scroll, 0);
    // Esc peels one layer at a time: the kept filter, then the pane.
    d.on_key(key(KeyCode::Esc));
    assert!(d.screen.output_filter.is_empty() && d.screen.output_focus);
    d.on_key(key(KeyCode::Esc));
    assert!(!d.screen.output_focus);
    d.on_key(KeyEvent::new(KeyCode::Char('l'), KeyModifiers::CONTROL));
    assert!(d.screen.ledger_overlay);
    d.on_key(key(KeyCode::Esc));
    assert!(!d.screen.ledger_overlay);
}

fn ctrl_key(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}

fn typed(d: &mut Driver, text: &str) {
    for c in text.chars() {
        d.on_key(key(KeyCode::Char(c)));
    }
}

fn working(d: &mut Driver) {
    d.screen.working = Some(p1_tui::state::Working {
        label: "writing".into(),
        started_ms: 0,
    });
}

#[test]
fn ctrl_c_at_idle_closes_layers_then_clears_the_draft_then_quits() {
    let (mut d, _) = driver();
    typed(&mut d, "/help");
    d.on_key(key(KeyCode::Enter));
    assert!(d.screen.status.is_some());
    d.on_key(ctrl_key('c'));
    assert!(d.screen.status.is_none(), "the overlay closes first");
    assert_eq!(d.exit, None);
    typed(&mut d, "a careful draft");
    d.on_key(ctrl_key('c'));
    assert_eq!(
        d.exit, None,
        "a draft is cleared, not lost with the process"
    );
    assert!(d.screen.composer.text.is_empty());
    d.on_key(key(KeyCode::Up));
    assert_eq!(
        d.screen.composer.text, "a careful draft",
        "Up brings it back"
    );
    d.screen.composer.take();
    d.on_key(ctrl_key('c'));
    assert_eq!(d.exit, Some(0));
}

#[test]
fn a_ctrl_c_right_after_a_cancel_needs_a_second_press_to_quit() {
    let (mut d, _) = driver();
    d.now_ms = 1_000;
    d.note_turn_end(&TurnEnd::Cancelled);
    d.now_ms = 1_400;
    d.on_key(ctrl_key('c'));
    assert_eq!(d.exit, None);
    assert!(d.screen.quit_armed);
    d.on_key(ctrl_key('c'));
    assert_eq!(d.exit, Some(0));
}

#[test]
fn local_commands_run_mid_turn_and_never_reach_the_model() {
    let (mut d, agent) = driver_with_agent();
    working(&mut d);
    typed(&mut d, "/status");
    d.on_key(key(KeyCode::Enter));
    assert!(d.screen.status.is_some(), "the overlay opens while working");
    assert!(!agent.has_pending_inbox(), "nothing was sent as steering");
    assert!(d.screen.queued.is_empty());
    // Plain text while working is still steering.
    d.on_key(key(KeyCode::Esc));
    typed(&mut d, "be brief");
    d.on_key(key(KeyCode::Enter));
    assert!(agent.has_pending_inbox());
}

#[test]
fn a_cancel_returns_queued_steering_to_the_composer() {
    let (mut d, agent) = driver_with_agent();
    working(&mut d);
    typed(&mut d, "also run the tests");
    d.on_key(key(KeyCode::Enter));
    typed(&mut d, "then summarise");
    d.on_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT));
    assert!(agent.has_pending_inbox());
    typed(&mut d, "draft");
    d.note_turn_end(&TurnEnd::Cancelled);
    assert!(
        !agent.has_pending_inbox(),
        "no turn starts from withdrawn steering"
    );
    assert!(d.follow_ups.is_empty() && d.screen.queued.is_empty());
    // In the order typed, each its own paragraph.
    assert_eq!(
        d.screen.composer.text,
        "also run the tests\n\nthen summarise\n\ndraft"
    );
    // Queued text is recallable too.
    assert!(
        d.screen
            .input_history
            .contains(&"also run the tests".to_string())
    );
}

#[test]
fn prompts_that_start_with_a_slash_are_prompts() {
    let (mut d, _) = driver();
    typed(&mut d, "/usr/bin/env python3 fails, please look");
    d.on_key(key(KeyCode::Enter));
    assert_eq!(
        d.submit_pending.as_deref(),
        Some("/usr/bin/env python3 fails, please look")
    );
    d.submit_pending = None;
    typed(&mut d, "//help");
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.submit_pending.as_deref(), Some("/help"));
    d.submit_pending = None;
    // An unknown /word is noted and kept; Enter again sends it.
    typed(&mut d, "/deploy");
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.submit_pending, None);
    assert_eq!(d.screen.composer.text, "/deploy");
    d.on_key(key(KeyCode::Enter));
    assert_eq!(d.submit_pending.as_deref(), Some("/deploy"));
}

#[test]
fn edit_goal_keeps_the_draft_and_prefills_the_goal() {
    let (mut d, _) = driver();
    d.screen.goal = Some("ship it".into());
    typed(&mut d, "my careful draft");
    d.on_key(ctrl_key('g'));
    assert_eq!(d.screen.composer.text, "/goal ship it");
    d.screen.composer.take();
    d.on_key(key(KeyCode::Up));
    assert_eq!(d.screen.composer.text, "my careful draft");
}

#[test]
fn typing_and_paste_in_the_output_pane_land_in_the_right_place() {
    let (mut d, _) = driver();
    d.screen.open_output(p1_tui::render::output::OutputView {
        id: p1_tui::fold::FoldId("h-1234".into()),
        lines: vec!["alpha row".into(), "beta row".into()],
        scroll: 0,
    });
    d.on_key(key(KeyCode::Char('/')));
    d.on_input(UiInput::Paste("beta".into()));
    assert_eq!(
        d.screen.output_filter, "beta",
        "a paste while filtering filters"
    );
    assert_eq!(d.screen.output_matches.as_deref(), Some(&[1][..]));
    d.on_key(key(KeyCode::Esc));
    assert_eq!(d.screen.output_filter, "", "esc cancels the filter edit");
    // Typing goes back to the draft, and the pane stays open.
    d.on_key(key(KeyCode::Char('h')));
    d.on_key(key(KeyCode::Char('i')));
    assert!(!d.screen.output_focus);
    assert_eq!(d.screen.composer.text, "hi");
    d.on_key(ctrl_key('o'));
    assert!(d.screen.output_focus, "^O refocuses the open pane");
    d.on_input(UiInput::Paste(" there".into()));
    assert_eq!(d.screen.composer.text, "hi there");
}

#[test]
fn delivered_steering_becomes_operator_rows() {
    let (mut d, _) = driver_with_agent();
    working(&mut d);
    typed(&mut d, "be brief");
    d.on_key(key(KeyCode::Enter));
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 5,
        worker: None,
        event: p1_contracts::AgentEvent::InboxDelivered { count: 1 },
    }));
    assert!(d.screen.queued.is_empty());
    assert!(matches!(
        d.screen.transcript.blocks.last(),
        Some(p1_tui::transcript::Block::Operator { text }) if text == "be brief"
    ));
}

#[tokio::test]
async fn a_word_typed_across_an_approval_never_decides_it() {
    let (policy, mut auth_rx) = TuiPolicy::new(true, CancellationToken::new());
    let (mut d, _auth) = driver();
    d.policy = Arc::new(policy);
    let call = p1_contracts::ToolCall {
        call_id: "w1".into(),
        name: "shell".into(),
        input: p1_contracts::ToolInput::Json("{\"command\":\"make\"}".into()),
    };
    let identity = p1_contracts::ToolIdentity {
        implementation: "shell".into(),
        variant: String::new(),
    };
    let pending = tokio::spawn({
        let policy = d.policy.clone();
        let (call, identity) = (call.clone(), identity.clone());
        async move {
            policy
                .authorize(p1_contracts::AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect: p1_contracts::Effect::Executes,
                })
                .await
        }
    });
    d.on_auth(auth_rx.recv().await.unwrap());
    // A pause, then a word that starts with a decision key.
    d.now_ms = 2_000;
    for (i, c) in "also".chars().enumerate() {
        d.now_ms = 2_000 + 60 * i as u64;
        d.on_key(key(KeyCode::Char(c)));
        d.commit_decision();
    }
    d.now_ms = 3_000;
    d.commit_decision();
    assert!(d.screen.approval.is_some(), "no decision was taken");
    assert_eq!(d.screen.composer.text, "also");
    // A lone key after a pause decides once its beat passes.
    d.screen.composer.take();
    d.now_ms = 5_000;
    d.on_key(key(KeyCode::Char('n')));
    d.now_ms = 5_500;
    d.commit_decision();
    assert!(d.screen.approval.is_none());
    assert!(matches!(pending.await.unwrap(), Decision::Deny { .. }));
    // The denial shows at once in the transcript.
    assert!(d.screen.transcript.blocks.iter().any(|b| matches!(b,
        p1_tui::transcript::Block::Call(r) if r.status == p1_tui::transcript::RowStatus::Settled(p1_contracts::ToolStatus::Denied))));
}

/// Park one shell call under `--ask` and put its approval on screen.
async fn parked(d: &mut Driver, call_id: &str) -> tokio::task::JoinHandle<Decision> {
    let (policy, mut auth_rx) = TuiPolicy::new(true, CancellationToken::new());
    d.policy = Arc::new(policy);
    let call = p1_contracts::ToolCall {
        call_id: call_id.into(),
        name: "shell".into(),
        input: p1_contracts::ToolInput::Json("{\"command\":\"make\"}".into()),
    };
    let identity = p1_contracts::ToolIdentity {
        implementation: "shell".into(),
        variant: String::new(),
    };
    let pending = tokio::spawn({
        let policy = d.policy.clone();
        async move {
            policy
                .authorize(p1_contracts::AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect: p1_contracts::Effect::Executes,
                })
                .await
        }
    });
    d.on_auth(auth_rx.recv().await.unwrap());
    pending
}

#[tokio::test]
async fn nothing_opens_over_a_pending_approval_and_it_is_decided_only_in_view() {
    let (mut d, _auth) = driver();
    d.screen.transcript.apply(
        &p1_contracts::AgentEvent::ToolFinished {
            result: p1_contracts::ToolResultItem {
                call_id: "earlier".into(),
                name: "shell".into(),
                status: p1_contracts::ToolStatus::Ok,
                content: (0..80).map(|n| format!("row {n}\n")).collect(),
            },
        },
        Some(1),
    );
    let pending = parked(&mut d, "a1").await;
    d.now_ms = 2_000;
    for layer in [
        ctrl_key('o'),
        ctrl_key('l'),
        KeyEvent::new(KeyCode::F(6), KeyModifiers::NONE),
    ] {
        d.on_key(layer);
        assert!(!d.screen.output_focus && !d.screen.ledger_overlay);
        assert!(
            d.screen
                .flash
                .as_ref()
                .unwrap()
                .0
                .contains("answer the approval first")
        );
    }
    for command in ["/help", "/status", "/outputs", "/open"] {
        typed(&mut d, command);
        d.on_key(key(KeyCode::Enter));
        assert!(d.screen.status.is_none() && d.screen.picker.is_none() && !d.screen.output_focus);
        // Refused, the command waits in the draft for afterwards.
        assert_eq!(d.screen.composer.text, command);
        d.screen.composer.take();
    }
    assert!(d.screen.approval.is_some());
    // `y⏎`, the [y/N] habit: Enter confirms the held key, nothing is steered.
    d.now_ms = 3_000;
    d.on_key(key(KeyCode::Char('y')));
    d.now_ms = 3_030;
    d.on_key(key(KeyCode::Enter));
    assert!(d.screen.approval.is_none());
    assert!(matches!(pending.await.unwrap(), Decision::Permit));
    assert!(d.screen.queued.is_empty() && d.screen.composer.text.is_empty());
}

#[tokio::test]
async fn esc_takes_a_held_decision_back_and_a_held_key_never_decides_a_later_call() {
    let (mut d, _auth) = driver();
    let _first = parked(&mut d, "a1").await;
    d.now_ms = 2_000;
    d.on_key(key(KeyCode::Char('n')));
    d.on_key(key(KeyCode::Esc));
    d.now_ms = 2_500;
    d.commit_decision();
    assert!(d.screen.approval.is_some(), "Esc dropped the held key");
    assert!(d.screen.composer.text.is_empty());
    // A key held for one call, whose request is then dropped (a cancel),
    // never answers the next one.
    d.on_key(key(KeyCode::Char('a')));
    d.drop_pending_auth();
    let second = parked(&mut d, "a2").await;
    d.now_ms = 3_000;
    d.commit_decision();
    assert!(d.screen.approval.is_some(), "the new call still waits");
    drop(second);
}

#[tokio::test]
async fn enter_after_a_held_key_finishes_a_word_and_a_lone_letter_never_steers() {
    let (mut d, _auth) = driver();
    working(&mut d);
    let pending = parked(&mut d, "a1").await;
    // `retr` + pause + `y⏎`: the operator finished a word; it is steering.
    d.now_ms = 2_000;
    typed(&mut d, "retr");
    d.now_ms = 2_600;
    d.on_key(key(KeyCode::Char('y')));
    d.now_ms = 2_630;
    d.on_key(key(KeyCode::Enter));
    assert!(
        d.screen.approval.is_some(),
        "no decision from a finished word"
    );
    assert!(d.screen.queued.iter().any(|q| q.text == "retry"));
    // A lone `y` typed too early is held back, not sent; once armed, Enter
    // with just that letter decides.
    d.screen.approval_shown_ms = 5_000;
    d.now_ms = 5_050;
    typed(&mut d, "y");
    d.on_key(key(KeyCode::Enter));
    assert!(d.screen.approval.is_some());
    assert_eq!(d.screen.composer.text, "y");
    assert!(!d.screen.queued.iter().any(|q| q.text == "y"));
    d.now_ms = 5_600;
    d.on_key(key(KeyCode::Enter));
    assert!(d.screen.approval.is_none());
    assert!(matches!(pending.await.unwrap(), Decision::Permit));
    assert!(d.screen.composer.text.is_empty());
}

#[test]
fn the_resume_command_keeps_every_flag_the_session_started_with() {
    let args = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let journal = std::path::Path::new("/tmp/s/session.jsonl");
    assert_eq!(
        resume_command(
            args(&[
                "p1",
                "--tui",
                "--env",
                "fixture",
                "--workspace",
                "/tmp/my ws",
                "--ask"
            ]),
            journal
        )
        .unwrap(),
        "p1 --resume --tui --env fixture --workspace '/tmp/my ws' --ask --session /tmp/s/session.jsonl"
    );
    // Already resumed, with its own --session: nothing doubles.
    assert_eq!(
        resume_command(
            args(&[
                "p1",
                "--tui",
                "--resume",
                "--session",
                "/tmp/s/session.jsonl"
            ]),
            journal
        )
        .unwrap(),
        "p1 --resume --tui --session /tmp/s/session.jsonl"
    );
    assert!(resume_command(args(&["tui_fixture", "/tmp/s"]), journal).is_none());
}

#[tokio::test]
async fn letters_typed_into_a_message_never_decide_and_a_retried_y_is_no_message() {
    let (mut d, _auth) = driver();
    working(&mut d);
    let pending = parked(&mut d, "a1").await;
    // `use ` … pause … `a` … pause: a one-letter word of a sentence is text.
    d.now_ms = 2_000;
    typed(&mut d, "use ");
    d.now_ms = 2_600;
    d.on_key(key(KeyCode::Char('a')));
    d.now_ms = 3_500;
    d.commit_decision();
    assert!(d.screen.approval.is_some(), "no grant from a word");
    assert_eq!(d.screen.composer.text, "use a");
    d.screen.composer.take();
    // `y⏎` typed twice for one approval (the reflex, retried): one decision,
    // nothing sent to the model.
    d.now_ms = 4_000;
    typed(&mut d, "yy");
    d.on_key(key(KeyCode::Enter));
    assert!(d.screen.approval.is_none());
    assert!(matches!(pending.await.unwrap(), Decision::Permit));
    assert!(d.screen.queued.is_empty());
}

#[tokio::test]
async fn a_session_grant_is_noted_where_it_was_made() {
    let (mut d, _auth) = driver();
    let _pending = parked(&mut d, "g1").await;
    d.now_ms = 2_000;
    d.on_key(key(KeyCode::Char('a')));
    d.now_ms = 2_500;
    d.commit_decision();
    assert!(d.screen.transcript.blocks.iter().any(|b| matches!(b,
        p1_tui::transcript::Block::Meta { text } if text.contains("allowed for the rest of this session"))));
}
