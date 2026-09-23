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
            ask: false,
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
            branch: Arc::new(Mutex::new(None)),
            describer: Arc::new(HostDescriber::new(
                std::env::current_dir().unwrap(),
                "off".into(),
            )),
            context_window: None,
            context_warn_at: None,
            model_switch: None,
            environment_dirs: Vec::new(),
            route_label: Arc::new(Mutex::new(String::new())),
            pending_switch: None,
            _workers: None,
        },
        auth,
    )
}

#[test]
fn typing_and_enter_submits_a_prompt() {
    let (mut d, _auth) = driver();
    for c in "fix it".chars() {
        d.on_key(key(KeyCode::Char(c)), None);
    }
    d.on_key(key(KeyCode::Enter), None);
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
        d.on_key(key(KeyCode::Char(c)), None);
    }
    d.on_key(key(KeyCode::Enter), None);
    assert_eq!(d.screen.focus_explicit, Some(true));
    assert_eq!(d.submit_pending, None);
    for c in "/exit".chars() {
        d.on_key(key(KeyCode::Char(c)), None);
    }
    d.on_key(key(KeyCode::Enter), None);
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
        d.on_key(key(KeyCode::Char(c)), None);
    }
    d.on_key(key(KeyCode::Enter), None);
    assert_eq!(d.submit_pending, None, "steering never starts a turn");
    assert_eq!(d.screen.queued.len(), 1);
    assert_eq!(d.screen.queued[0].text, "use vecdeque");
    // The steering actually reached the agent's inbox (the earlier version of
    // this test dropped the agent and never noticed the send failing).
    assert!(agent.has_pending_inbox());
    // §8.2: delivery renders the queued text as a tagged `OperatorTurn`, not
    // just the meta row — the driver also calls `transcript.queue_steering`.
    d.screen
        .transcript
        .apply(&p1_contracts::AgentEvent::InboxDelivered { count: 1 }, None);
    assert!(matches!(
        d.screen.transcript.blocks.last(),
        Some(p1_tui::transcript::Block::Operator { text, steering: true }) if text == "use vecdeque"
    ));
}

// -------------------------------------------------------------- slash commands

#[test]
fn slash_help_lists_commands_as_command_output() {
    let (mut d, _auth) = driver();
    d.slash("help", None);
    let p1_tui::transcript::Block::CommandOutput(output) = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a command output block, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(output.command, "/help");
    assert!(output.body.iter().any(|row| matches!(
        row,
        p1_tui::transcript::CommandRow::Entry { key, .. } if key == "/model [REF]"
    )));
}

#[test]
fn slash_status_reports_the_facts_the_retired_overlay_showed() {
    let (mut d, _auth) = driver();
    d.slash("status", None);
    let p1_tui::transcript::Block::CommandOutput(output) = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a command output block, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(output.command, "/status");
    let keys: Vec<&str> = output
        .body
        .iter()
        .filter_map(|row| match row {
            p1_tui::transcript::CommandRow::Entry { key, .. } => Some(key.as_str()),
            p1_tui::transcript::CommandRow::Head(_) => None,
        })
        .collect();
    assert_eq!(
        keys,
        [
            "environment",
            "route",
            "model",
            "effort",
            "access",
            "sandbox",
            "spend"
        ]
    );
    // The overlay is retired: `/status` never sets it any more.
    assert!(d.screen.status.is_none());
}

#[test]
fn slash_access_names_the_policy_facts() {
    let (mut d, _auth) = driver();
    d.slash("access", None);
    let p1_tui::transcript::Block::CommandOutput(output) = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a command output block, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(output.command, "/access");
    assert!(output.body.iter().any(|row| matches!(
        row,
        p1_tui::transcript::CommandRow::Entry { key, text }
            if key == "mode" && text.starts_with("full")
    )));
}

#[test]
fn slash_models_lists_the_empty_search_path_as_zero_models() {
    let (mut d, _auth) = driver();
    d.slash("models", None);
    let p1_tui::transcript::Block::CommandOutput(output) = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a command output block, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(output.command, "/models");
    assert_eq!(output.facts, "0 models");
}

#[test]
fn slash_env_is_an_alias_of_bare_model() {
    let (mut d, _auth) = driver();
    d.slash("env", None);
    assert!(matches!(
        &d.screen.transcript.blocks[0],
        p1_tui::transcript::Block::CommandOutput(output) if output.command == "/models"
    ));
}

#[test]
fn an_unknown_slash_command_is_one_meta_row() {
    let (mut d, _auth) = driver();
    d.slash("bogus", None);
    assert_eq!(d.screen.transcript.blocks.len(), 1);
    let p1_tui::transcript::Block::Meta { text } = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a meta row, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(text, "· /bogus is not a command · /help");
}

#[test]
fn slash_effort_without_a_level_asks_for_one() {
    let (mut d, _auth) = driver();
    d.slash("effort", None);
    let p1_tui::transcript::Block::Meta { text } = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a meta row, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(text, "· /effort needs a level · low medium high max");
}

#[test]
fn a_model_switch_typed_mid_turn_queues_for_the_next_boundary() {
    let (mut d, _auth) = driver();
    d.request_switch(PendingSwitch::Effort("high".into()), None);
    assert!(matches!(d.pending_switch, Some(PendingSwitch::Effort(ref level)) if level == "high"));
    let p1_tui::transcript::Block::Meta { text } = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a meta row, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(text, "· model switch queued · applies at the next boundary");
}

#[test]
fn slash_model_with_no_switch_seam_refuses_cleanly() {
    let (mut d, mut agent) = driver_with_agent();
    d.slash("model claude/opus", Some(&mut agent));
    let p1_tui::transcript::Block::Meta { text } = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a meta row, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(
        text,
        "· switch refused · no model switch is available this run"
    );
}

#[test]
fn report_switch_ok_renders_the_transition_and_moves_the_driver_state() {
    let (mut d, _auth) = driver();
    d.report_switch("claude/sonnet".into(), Ok("gpt/5.1:high".into()));
    assert_eq!(d.env, "gpt");
    assert_eq!(d.model, "gpt/5.1:high");
    assert_eq!(d.screen.statusbar.effort.as_deref(), Some("high"));
    let p1_tui::transcript::Block::Meta { text } = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a meta row, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(
        text,
        "· model claude/sonnet → gpt/5.1:high · from the next turn"
    );
}

#[test]
fn report_switch_err_keeps_state_and_names_what_stayed() {
    let (mut d, _auth) = driver();
    let before = d.model.clone();
    d.report_switch(before.clone(), Err("unknown model \"x\"".into()));
    assert_eq!(d.model, before);
    let p1_tui::transcript::Block::Meta { text } = &d.screen.transcript.blocks[0] else {
        panic!(
            "expected a meta row, got {:?}",
            d.screen.transcript.blocks[0]
        );
    };
    assert_eq!(
        text,
        &format!("✗ switch refused · unknown model \"x\" · kept still on {before}")
    );
}

// ------------------------------------------------------------------- context

#[test]
fn response_completed_computes_ctx_from_the_configured_window() {
    let (mut d, _auth) = driver();
    d.context_window = Some(120_000);
    d.context_warn_at = Some(90_000);
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        worker: None,
        at_ms: 1_000,
        event: p1_contracts::AgentEvent::ResponseCompleted {
            model: "m".into(),
            stop: p1_contracts::StopReason::EndTurn,
            usage: Some(p1_contracts::Usage {
                input_uncached: Some(60_000),
                ..p1_contracts::Usage::default()
            }),
        },
    }));
    assert_eq!(d.screen.statusbar.ctx.as_deref(), Some("50%"));
    assert!(!d.screen.statusbar.ctx_warn);
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
    // The inline Block names the tool it asks about (handoff §7.5); nothing waits behind it.
    assert_eq!(d.screen.approval_tool, "shell");
    assert_eq!(d.screen.approvals_waiting, 0);
    assert!(d.screen.pinned);
    d.on_key(key(KeyCode::Char('y')), None);
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

/// ADR-0048: a provider notice is one quiet transcript note and nothing else — no
/// prose block, no turn state moved.
#[test]
fn a_provider_notice_becomes_one_transcript_note() {
    let (mut d, _auth) = driver();
    let text = "transport: WebSocket unavailable (HTTP 500) — using HTTP (SSE) for the rest of \
                this session";
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 0,
        worker: None,
        event: p1_contracts::AgentEvent::ProviderNotice { text: text.into() },
    }));
    assert_eq!(
        d.screen.transcript.blocks,
        vec![p1_tui::transcript::Block::Meta {
            text: format!("· {text}")
        }]
    );
    assert!(d.screen.working.is_none());
}

#[test]
fn a_live_worker_promotes_the_pane_and_a_finished_one_releases_it() {
    use p1_tui::render::workers::{BlockState, WorkerBlock};
    use p1_tui::state::{PaneMode, PaneWidth};
    let (mut d, _auth) = driver();
    let row = |state: BlockState| WorkerBlock {
        id: "w1".into(),
        task: String::new(),
        route: "deepseek/v4.1-flash".into(),
        state,
        elapsed: None,
        cost_micro_usd: None,
        grants: String::new(),
        activity: String::new(),
    };
    d.screen.pane_width = PaneWidth::Off;
    d.screen.sync_workers(vec![row(BlockState::Running)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Workers);
    assert_eq!(d.screen.pane_width, PaneWidth::Wide);
    // All done: an unpinned WORKERS pane falls back to the ledger.
    d.screen.sync_workers(vec![row(BlockState::Done)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Ledger);
    // …unless it is pinned.
    d.screen.pane_mode = PaneMode::Workers;
    d.screen.pinned = true;
    d.screen.sync_workers(vec![row(BlockState::Running)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Workers);
    d.screen.sync_workers(vec![row(BlockState::Done)]);
    assert_eq!(d.screen.pane_mode, PaneMode::Workers, "pinning always wins");
}
