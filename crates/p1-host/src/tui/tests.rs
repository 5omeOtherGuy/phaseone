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
            branch: Arc::new(Mutex::new(None)),
            describer: Arc::new(HostDescriber::new(
                std::env::current_dir().unwrap(),
                "off".into(),
            )),
            _workers: None,
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
    d.on_key(key(KeyCode::Char('y')));
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
