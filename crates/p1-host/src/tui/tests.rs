//! Driver tests: no TTY, no terminal — the driver over plain method calls.

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A minimal real agent, for its inbox handle and (in the loop tests) for the
/// `&mut Agent` the run loop drives. Its provider never answers, so a turn is
/// never run through it: a test that runs one to its end builds its own agent
/// with [`agent_with`] (and scripts a provider that does answer).
fn test_agent() -> Agent {
    agent_with(Arc::new(p1_testkit::ScriptedProvider::new(vec![])))
}

/// An agent over `provider`: no tools, no authorization prompt, in-memory
/// journal. Its inbox handle is what the driver uses. The handle stays with the
/// caller, so a test can see how many turns the fake was asked for.
fn agent_with(provider: Arc<p1_testkit::ScriptedProvider>) -> Agent {
    Agent::new(p1_core::AgentParts {
        provider,
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
    .expect("agent builds")
}

/// One scripted turn that RESOLVES on its first poll: a stream that ends with no
/// events is a transport failure (SPEC §3d), which ends the turn immediately —
/// the provider is asked nothing further, so a later turn needs another step.
fn resolving_turn() -> p1_testkit::Step {
    p1_testkit::Step::Events(vec![])
}

/// A driver with an `--ask`-off policy and no wiring behind it.
fn driver() -> (Driver, mpsc::UnboundedReceiver<AuthRequest>) {
    driver_with(false)
}

/// A driver whose policy is `--ask` (`true`) or full access: the idle-loop tests
/// park a real authorization on the ask one (handoff §7.5).
fn driver_with(ask: bool) -> (Driver, mpsc::UnboundedReceiver<AuthRequest>) {
    let (policy, auth) = TuiPolicy::new(ask, CancellationToken::new());
    // A minimal real agent, for its inbox handle.
    let agent = test_agent();
    (
        Driver {
            screen: Screen::new(true),
            env: "claude".into(),
            route: "claude".into(),
            model: "sonnet-4.5".into(),
            workspace: std::env::current_dir().unwrap(),
            sandbox: "off".into(),
            ask,
            policy: Arc::new(policy),
            pending_auth: VecDeque::new(),
            pinned_by_approval: false,
            follow_ups: VecDeque::new(),
            submit_pending: None,
            worker_rows: Arc::new(Mutex::new(Vec::new())),
            worker_stops: None,
            run_cancels: None,
            worker_usage: HashMap::new(),
            worker_windows: Arc::new(Mutex::new(HashMap::new())),
            pending_calls: HashMap::new(),
            task_files: HashSet::new(),
            exit: None,
            inbox: agent.inbox(),
            branch: Arc::new(Mutex::new(None)),
            tools: Arc::new(Vec::new()),
            context_window: None,
            context_warn_at: None,
            pending_worker_starts: HashMap::new(),
            worker_details: Arc::new(Mutex::new(HashMap::new())),
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
fn streamed_tool_input_prepares_a_named_call_row() {
    let (mut d, _auth) = driver();
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 1,
        worker: None,
        event: p1_contracts::AgentEvent::TurnStarted,
    }));
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 1,
        worker: None,
        event: p1_contracts::AgentEvent::ToolInputDelta {
            call_id: "c-patch".into(),
            name: "apply_patch".into(),
            text: "*** Begin Patch".into(),
        },
    }));
    let Some(p1_tui::transcript::Block::Call(row)) = d.screen.transcript.blocks.first() else {
        panic!("the streamed arguments should create a preparing block");
    };
    assert_eq!(row.name, "apply_patch");
    assert_eq!(row.summary, "*** Begin Patch");
    assert!(
        row.call.is_none(),
        "the block remains preparing until ToolStarted"
    );
    let rendered = p1_tui::render::block::lines(row, 76, false, 0, true);
    assert!(rendered[0].to_string().contains("apply_patch"));
}

#[test]
fn interactive_summary_notice_is_added_and_removed_by_host_control_events() {
    let (mut d, _auth) = driver();
    let notice = |text: &str| {
        UiEvent::Agent(p1_tui::runtime::Stamped {
            at_ms: 1,
            worker: None,
            event: p1_contracts::AgentEvent::ProviderNotice { text: text.into() },
        })
    };
    d.on_ui_event(notice("\0p1-idle-summary-count:6"));
    assert!(d.screen.transcript.blocks.iter().any(|block| matches!(
        block,
        p1_tui::transcript::Block::Meta { text }
            if text == "· 6 context summaries since the last change"
    )));
    d.on_ui_event(notice("\0p1-idle-summary-count:0"));
    assert!(!d.screen.transcript.blocks.iter().any(|block| matches!(
        block,
        p1_tui::transcript::Block::Meta { text }
            if text.starts_with("· ") && text.contains("context summaries since the last change")
    )));
}

#[test]
fn home_prelude_is_attached_to_the_driver_screen_with_workspace_and_current_model() {
    let (mut d, _auth) = driver();
    let workspace = tempfile::tempdir().unwrap();
    d.screen.home = Some(home_prelude(
        workspace.path(),
        Some("task/test".into()),
        "claude",
        "sonnet-4.5",
        false,
    ));
    let home = d.screen.home.as_ref().unwrap();
    assert_eq!(home.path, workspace.path().to_string_lossy());
    assert_eq!(
        display_workspace_path(
            std::path::Path::new("/tmp/fake-home/project"),
            Some(std::ffi::OsStr::new("/tmp/fake-home")),
        ),
        "~/project"
    );
    assert_eq!(home.branch.as_deref(), Some("task/test"));
    assert!(
        home.items
            .iter()
            .any(|(command, text)| command == "/env" && text == "claude · sonnet-4.5")
    );
    assert_eq!(
        home.items[0],
        ("/resume".into(), "reopen a previous session".into())
    );
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
    assert_eq!(d.screen.session.as_ref().unwrap().effort, "high");
    assert_eq!(d.screen.session.as_ref().unwrap().model, "gpt/5.1:high");
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
    d.screen.context = Some(p1_tui::render::ledger::ContextView {
        used: None,
        window: 120_000,
        summarize_at: 90_000,
        parts: vec![],
    });
    assert_eq!(d.screen.context.as_ref().unwrap().used, None);
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
    assert_eq!(d.screen.context.as_ref().unwrap().used, Some(60_000));
    assert_eq!(d.screen.context.as_ref().unwrap().window, 120_000);
    assert!(d.screen.context.as_ref().unwrap().parts.is_empty());
}

/// A driver plus the agent whose inbox handle it holds.
fn driver_with_agent() -> (Driver, Agent) {
    let (mut d, _auth) = driver();
    let agent = test_agent();
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

#[cfg(feature = "delegation")]
#[test]
fn worker_start_event_supplies_task_and_grants_to_the_worker_snapshot() {
    let (mut d, _auth) = driver();
    let call = p1_contracts::ToolCall {
        call_id: "start-1".into(),
        name: "worker_start".into(),
        input: p1_contracts::ToolInput::Json(
            r#"{"task":"Inspect the parser\nsecond line","tools":["read","edit"]}"#.into(),
        ),
    };
    d.track_task(&p1_contracts::AgentEvent::ToolStarted { call });
    d.track_task(&p1_contracts::AgentEvent::ToolFinished {
        result: p1_contracts::ToolResultItem {
            call_id: "start-1".into(),
            name: "worker_start".into(),
            status: p1_contracts::ToolStatus::Ok,
            content: "Started worker w1 on deepseek/v4.1-flash with tools: read, edit, finish. You will be notified when it finishes.".into(),
        },
    });
    assert_eq!(
        d.worker_details.lock().unwrap().get("w1"),
        Some(&("Inspect the parser".into(), "read, edit, finish".into()))
    );
}

#[test]
fn edit_approval_uses_tool_preview_and_patch_keeps_permission_form() {
    let dir = tempfile::tempdir().unwrap();
    let workspace = p1_workspace::Workspace::new(dir.path()).unwrap();
    let observed = p1_workspace::ObservedFiles::new();
    std::fs::write(dir.path().join("a.txt"), "old").unwrap();
    let edit_tool = p1_tool_edit::EditTool::new(workspace.clone(), observed.clone());
    let edit_call = ToolCall {
        call_id: "edit".into(),
        name: "edit".into(),
        input: p1_contracts::ToolInput::Json(
            r#"{"file_path":"a.txt","old_string":"old","new_string":"new"}"#.into(),
        ),
    };
    let edit_description = edit_tool.describe(&edit_call);
    assert!(matches!(
        approval_view(
            &edit_call,
            Effect::WritesFiles,
            &edit_description,
            dir.path(),
            "off"
        ),
        Approval::Diff(_)
    ));

    let patch_tool = p1_tool_patch::PatchTool::new(workspace, observed);
    let patch_call = ToolCall {
        call_id: "patch".into(),
        name: "apply_patch".into(),
        input: p1_contracts::ToolInput::Text(
            "*** Begin Patch\n*** Update File: a.txt\n@@\n-old\n+new\n*** End Patch\n".into(),
        ),
    };
    let patch_description = patch_tool.describe(&patch_call);
    assert_eq!(patch_description.edit, None);
    assert!(matches!(
        approval_view(
            &patch_call,
            Effect::WritesFiles,
            &patch_description,
            dir.path(),
            "off"
        ),
        Approval::Permission(_)
    ));
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
        model: None,
        state,
        elapsed: None,
        cost_micro_usd: None,
        tokens: None,
        context_window: None,
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

/// ADR-0057: the WORKSPACE section tracks the files a session writes from the
/// tool that owns each call (`effect` + `describe`), so a RENAMED edit face and
/// the freeform `apply_patch` (no `file_path` key at all) both count.
#[test]
fn a_renamed_edit_face_and_an_apply_patch_call_track_both_files() {
    use p1_contracts::{AgentEvent, ToolInput, ToolResultItem, ToolStatus};

    let (mut d, _auth) = driver();
    let dir = tempfile::tempdir().unwrap();
    let workspace = p1_workspace::Workspace::new(dir.path()).unwrap();
    let observed = p1_workspace::ObservedFiles::new();
    let edit = Arc::new(
        p1_tool_edit::EditTool::new(workspace.clone(), observed.clone())
            .with_face(p1_tool_edit::ToolFace::new("EditFile", "renamed"), "gpt"),
    ) as Arc<dyn Tool>;
    let patch = Arc::new(p1_tool_patch::PatchTool::new(workspace, observed)) as Arc<dyn Tool>;
    d.tools = Arc::new(vec![edit, patch]);

    let started = |call_id: &str, name: &str, input: ToolInput| AgentEvent::ToolStarted {
        call: ToolCall {
            call_id: call_id.into(),
            name: name.into(),
            input,
        },
    };
    let finished = |call_id: &str, name: &str| AgentEvent::ToolFinished {
        result: ToolResultItem {
            call_id: call_id.into(),
            name: name.into(),
            status: ToolStatus::Ok,
            content: String::new(),
        },
    };

    d.track_task(&started(
        "c1",
        "EditFile",
        ToolInput::Json(r#"{"file_path":"src/a.rs","old_string":"a","new_string":"b"}"#.into()),
    ));
    d.track_task(&finished("c1", "EditFile"));
    assert!(d.task_files.contains("src/a.rs"));

    d.track_task(&started(
        "c2",
        "apply_patch",
        ToolInput::Text(
            "*** Begin Patch\n*** Update File: src/b.rs\n@@\n-a\n+b\n*** End Patch\n".into(),
        ),
    ));
    d.track_task(&finished("c2", "apply_patch"));

    assert!(d.task_files.contains("src/b.rs"));
    assert_eq!(d.task_files.len(), 2);
    assert_eq!(
        d.screen.workspace.as_ref().and_then(|w| w.files),
        Some(2),
        "the WORKSPACE section counts both files"
    );
}

// ------------------------------------------------------ the idle redraw policy

/// One WORKERS row for the redraw tests: running, with a fixed elapsed string
/// (the real refresher's own field) so nothing in the row moves on its own.
fn worker_row() -> p1_tui::render::workers::WorkerBlock {
    use p1_tui::render::workers::{BlockState, WorkerBlock};
    WorkerBlock {
        id: "w1".into(),
        task: "inspect the parser".into(),
        route: "deepseek/v4.1-flash".into(),
        model: None,
        state: BlockState::Running,
        elapsed: Some("0m00s".into()),
        cost_micro_usd: None,
        tokens: None,
        context_window: None,
        grants: "read, edit".into(),
        activity: String::new(),
    }
}

fn worker_response(id: &str, model: &str, usage: Option<p1_contracts::Usage>) -> UiEvent {
    UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 0,
        worker: Some(id.into()),
        event: p1_contracts::AgentEvent::ResponseCompleted {
            model: model.into(),
            stop: p1_contracts::StopReason::EndTurn,
            usage,
        },
    })
}

fn worker_test_row(id: &str) -> p1_tui::render::workers::WorkerBlock {
    p1_tui::render::workers::WorkerBlock {
        id: id.into(),
        task: String::new(),
        route: "deepseek/v4.1-flash".into(),
        model: None,
        state: p1_tui::render::workers::BlockState::Running,
        elapsed: None,
        cost_micro_usd: None,
        tokens: None,
        context_window: None,
        grants: String::new(),
        activity: String::new(),
    }
}

/// A key stream over a channel, so a script can deliver an input at a chosen
/// simulated instant — the shape `run` builds from crossterm's events.
fn key_stream(
    rx: mpsc::UnboundedReceiver<Input>,
) -> std::pin::Pin<Box<dyn futures_util::Stream<Item = Input>>> {
    Box::pin(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|input| (input, rx))
    }))
}

/// The handles a test script uses to poke the running loop.
#[derive(Clone)]
struct Wires {
    keys: mpsc::UnboundedSender<Input>,
    /// The loop's UI event channel (the tests stamp their own events).
    events: mpsc::UnboundedSender<UiEvent>,
    rows: Arc<Mutex<Vec<p1_tui::render::workers::WorkerBlock>>>,
    /// The driver's policy: an `authorize` call on it parks a request on the
    /// screen when the driver is under `--ask`.
    policy: Arc<TuiPolicy>,
    cancel: CancellationToken,
    draws: DrawCounter,
}

/// The real idle loop (`drive_loop`) over a test backend: a test runs a script
/// beside it under paused tokio time (`#[tokio::test(start_paused = true)]`), so
/// every `sleep` in a script is the runtime's own frozen clock — no real time
/// passes, and no assertion here is a timing measurement.
struct IdleLoop<B: Backend> {
    terminal: ratatui::Terminal<B>,
    driver: Driver,
    agent: Agent,
    keys: mpsc::UnboundedReceiver<Input>,
    events: mpsc::UnboundedReceiver<UiEvent>,
    auth: mpsc::UnboundedReceiver<AuthRequest>,
    sink: TuiSink,
    cancel: CancellationToken,
    redraws: Redraws,
    wires: Wires,
}

impl IdleLoop<ratatui::backend::TestBackend> {
    fn new() -> Self {
        Self::with_ask(false)
    }

    /// With `--ask` (`true`), an authorization parks on the screen — a frame
    /// input of its own.
    fn with_ask(ask: bool) -> Self {
        Self::with_backend(ratatui::backend::TestBackend::new(96, 24), ask)
    }
}

impl<B: Backend> IdleLoop<B> {
    fn with_backend(backend: B, ask: bool) -> Self {
        Self::with_agent(backend, ask, test_agent())
    }

    /// A loop over a test's own agent. `test_agent`'s provider never answers, so
    /// only a test that runs a turn to its end needs this.
    fn with_agent(backend: B, ask: bool, agent: Agent) -> Self {
        let (mut driver, auth) = driver_with(ask);
        driver.inbox = agent.inbox();
        // The sink is the loop's clock — the production wiring — while the
        // events the tests inject go over their own channel, so a test stamps
        // its own `at_ms` (`run` gets both from the one sink).
        let (sink, _unused) = TuiSink::new();
        let (events, event_rx) = mpsc::unbounded_channel();
        let (keys, key_rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let draws = DrawCounter::default();
        let wires = Wires {
            keys,
            events,
            rows: driver.worker_rows.clone(),
            policy: driver.policy.clone(),
            cancel: cancel.clone(),
            draws: draws.clone(),
        };
        Self {
            terminal: ratatui::Terminal::new(backend).expect("the test terminal builds"),
            driver,
            agent,
            keys: key_rx,
            events: event_rx,
            auth,
            sink,
            cancel,
            redraws: Redraws::new(draws),
            wires,
        }
    }

    /// Run the loop beside `script` until the script finishes (its own `cancel`
    /// normally ends the loop first). What the loop drew is the test's to assert
    /// on, through its clone of the counter.
    async fn run<S: std::future::Future<Output = ()>>(self, script: S) {
        tokio::select! {
            _ = self.run_to_end() => {}
            () = script => {}
        }
    }

    /// Run the loop until it returns — its exit code.
    async fn run_to_end(self) -> i32 {
        self.run_to_end_with_driver().await.0
    }

    /// Run the loop until it returns, handing the driver back as well: a test
    /// that needs to see the state the loop left behind (which prompts it
    /// started, what it queued) reads it here.
    async fn run_to_end_with_driver(self) -> (i32, Driver) {
        let IdleLoop {
            mut terminal,
            mut driver,
            mut agent,
            keys,
            events,
            auth,
            sink,
            cancel,
            redraws,
            ..
        } = self;
        let keys = key_stream(keys);
        let code = drive_loop(
            &mut terminal,
            &mut driver,
            &mut agent,
            keys,
            events,
            auth,
            &cancel,
            &sink,
            ColorMode::TrueColor,
            redraws,
        )
        .await;
        (code, driver)
    }
}

/// The operator's lines, in order: the prompts the loop actually started. The
/// transcript is the loop's own record — a queued follow-up lands here when the
/// turn boundary takes it, not when it is queued.
fn operator_prompts(driver: &Driver) -> Vec<&str> {
    driver
        .screen
        .transcript
        .blocks
        .iter()
        .filter_map(|block| match block {
            p1_tui::transcript::Block::Operator { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

/// Everything a drawn frame left on the terminal — the tests' way to read what
/// actually reached the screen (the backend's own cell buffer).
fn frame_text<B: ScreenText>(terminal: &ratatui::Terminal<B>) -> String {
    terminal.backend().screen_text()
}

/// A test backend whose drawn frame a test can read back row by row.
trait ScreenText: Backend {
    fn screen_text(&self) -> String;
}

/// The cell grid of a `TestBackend`'s buffer, row by row.
fn buffer_text(buffer: &ratatui::buffer::Buffer) -> String {
    let area = buffer.area;
    (0..area.height)
        .map(|y| {
            (0..area.width)
                .map(|x| buffer[(x, y)].symbol())
                .collect::<String>()
        })
        .collect::<Vec<_>>()
        .join("\n")
}

impl ScreenText for ratatui::backend::TestBackend {
    fn screen_text(&self) -> String {
        buffer_text(self.buffer())
    }
}

/// A backend whose `draw` fails until its budget runs out, then hands over to
/// the `TestBackend` underneath (issue #141 review: a refused frame must not be
/// recorded as drawn). Every other call is the inner backend's, whose own
/// error type is `Infallible`.
struct FailingBackend {
    inner: ratatui::backend::TestBackend,
    /// How many more `draw` calls refuse the frame.
    failures: Arc<std::sync::atomic::AtomicUsize>,
}

impl FailingBackend {
    /// The next `failures` frames are refused; the one after them is drawn.
    fn refusing(failures: usize) -> Self {
        Self {
            inner: ratatui::backend::TestBackend::new(96, 24),
            failures: Arc::new(std::sync::atomic::AtomicUsize::new(failures)),
        }
    }

    /// Take one refusal from the budget; `true` when this frame is refused.
    fn refuse(&self) -> bool {
        self.failures
            .fetch_update(
                std::sync::atomic::Ordering::Relaxed,
                std::sync::atomic::Ordering::Relaxed,
                |left| (left > 0).then(|| left - 1),
            )
            .is_ok()
    }
}

impl ScreenText for FailingBackend {
    fn screen_text(&self) -> String {
        buffer_text(self.inner.buffer())
    }
}

/// The `TestBackend` never fails; its `Infallible` becomes the `io::Error` this
/// wrapper must be able to raise from `draw`.
fn infallible<T>(result: Result<T, std::convert::Infallible>) -> std::io::Result<T> {
    result.map_err(|never| match never {})
}

/// The refusal the terminal reports in the failure tests.
fn terminal_error() -> std::io::Error {
    std::io::Error::other("the terminal went away")
}

impl Backend for FailingBackend {
    type Error = std::io::Error;

    fn draw<'a, I>(&mut self, content: I) -> std::io::Result<()>
    where
        I: Iterator<Item = (u16, u16, &'a ratatui::buffer::Cell)>,
    {
        if self.refuse() {
            return Err(terminal_error());
        }
        infallible(self.inner.draw(content))
    }

    fn hide_cursor(&mut self) -> std::io::Result<()> {
        infallible(self.inner.hide_cursor())
    }

    fn show_cursor(&mut self) -> std::io::Result<()> {
        infallible(self.inner.show_cursor())
    }

    fn get_cursor_position(&mut self) -> std::io::Result<ratatui::layout::Position> {
        infallible(self.inner.get_cursor_position())
    }

    fn set_cursor_position<P: Into<ratatui::layout::Position>>(
        &mut self,
        position: P,
    ) -> std::io::Result<()> {
        infallible(self.inner.set_cursor_position(position))
    }

    fn clear(&mut self) -> std::io::Result<()> {
        infallible(self.inner.clear())
    }

    fn clear_region(&mut self, clear_type: ratatui::backend::ClearType) -> std::io::Result<()> {
        infallible(self.inner.clear_region(clear_type))
    }

    fn size(&self) -> std::io::Result<ratatui::layout::Size> {
        infallible(self.inner.size())
    }

    fn window_size(&mut self) -> std::io::Result<ratatui::backend::WindowSize> {
        infallible(self.inner.window_size())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        infallible(self.inner.flush())
    }
}

/// Issue #141: an idle TUI with no events draws almost nothing. Ten simulated
/// seconds draw the first frame and nothing else, where the old loop drew 20 a
/// second; the requirement's bound is one a second.
#[tokio::test(start_paused = true)]
async fn an_idle_loop_draws_at_most_once_a_second() {
    let harness = IdleLoop::new();
    let wires = harness.wires.clone();
    let draws = wires.draws.clone();
    let script = async move {
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
        wires.cancel.cancel();
    };
    harness.run(script).await;
    let frames = draws.count();
    assert!(
        frames >= 1,
        "the first frame must be drawn before any input"
    );
    assert!(
        frames <= 10,
        "an idle TUI drew {frames} frames in ten simulated seconds"
    );
}

/// Issue #141: worker rows are polled, but only a change draws a frame — and
/// only one: the polls after it see the rows they already drew.
#[tokio::test(start_paused = true)]
async fn a_changed_worker_row_draws_exactly_once() {
    let harness = IdleLoop::new();
    let wires = harness.wires.clone();
    let script = async move {
        // Let the first frame and a couple of polls pass.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        let before = wires.draws.count();
        wires.rows.lock().unwrap().push(worker_row());
        // Two polls later than the change needs: the first one drew, the second
        // saw the same rows.
        tokio::time::sleep(std::time::Duration::from_millis(600)).await;
        wires.cancel.cancel();
        assert_eq!(
            wires.draws.count() - before,
            1,
            "a changed worker row draws exactly one frame"
        );
    };
    harness.run(script).await;
}

/// Issue #141: a key is a frame input of its own — it draws at once, without
/// waiting for the heartbeat (a simulated second away here).
#[tokio::test(start_paused = true)]
async fn a_key_draws_immediately() {
    let harness = IdleLoop::new();
    let wires = harness.wires.clone();
    let script = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1550)).await;
        let before = wires.draws.count();
        wires
            .keys
            .send(Input::Key(key(KeyCode::Char('x'))))
            .expect("the loop reads keys");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        wires.cancel.cancel();
        assert_eq!(
            wires.draws.count() - before,
            1,
            "a key draws at once, not at the next heartbeat"
        );
    };
    harness.run(script).await;
}

/// Issue #141: while a turn runs, the `▪▪▪` pulse is the one frame input with
/// no text to compare, so the heartbeat draws it — at the spinner's 5 Hz, not
/// the old 20 Hz, and it keeps drawing while the turn is live.
#[tokio::test(start_paused = true)]
async fn a_running_turns_pulse_draws_at_the_spinner_heartbeat() {
    let (mut d, mut auth) = driver();
    d.screen.reduced_motion = false;
    d.screen.working = Some(p1_tui::state::Working {
        label: "shell".into(),
        started_ms: 0,
    });
    let (sink, mut events) = TuiSink::new();
    let (keys_tx, keys) = mpsc::unbounded_channel();
    let _keys_tx = keys_tx;
    let mut keys = key_stream(keys);
    let turn_cancel = CancellationToken::new();
    let mut redraws = Redraws::new(DrawCounter::default());
    let draws = redraws.draws.clone();
    let mut terminal = ratatui::Terminal::new(ratatui::backend::TestBackend::new(96, 24)).unwrap();
    let mut running = Box::pin(pump(
        &mut terminal,
        &mut d,
        &mut keys,
        &mut events,
        &mut auth,
        &sink,
        &turn_cancel,
        ColorMode::TrueColor,
        &mut redraws,
        Box::pin(std::future::pending::<TurnEnd>()),
    ));
    let script = async {
        // Start after the first pulse frame, so the window is exactly one
        // simulated second of a live turn.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let before = draws.count();
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        draws.count() - before
    };
    let frames = tokio::select! {
        _ = &mut running => panic!("the turn future never resolves"),
        frames = script => frames,
    };
    assert!(frames > 0, "the pulse keeps drawing while the turn runs");
    assert!(
        frames <= 5,
        "a running turn drew {frames} pulse frames in one simulated second"
    );
}

/// A parent event that fails a tool call: `Screen::apply` raises the §5 PEEK —
/// the three-second banner the loop must keep counting down (issue #141 review).
fn failed_tool_event(at_ms: u64) -> UiEvent {
    UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms,
        worker: None,
        event: p1_contracts::AgentEvent::ToolFinished {
            result: p1_contracts::ToolResultItem {
                call_id: "c1".into(),
                name: "shell".into(),
                status: p1_contracts::ToolStatus::Error,
                content: "exit 1: no such file".into(),
            },
        },
    })
}

/// Issue #141 review: a PEEK is a frame input that moves on its own — its
/// countdown redraws at the 1 Hz heartbeat while the banner is visible, and
/// exactly one frame erases it once `Screen::tick` expires it, then the loop is
/// idle again. The simulated instants avoid the 1 s tick boundaries, so the
/// counts are exact rather than timing races.
#[tokio::test(start_paused = true)]
async fn a_peek_countdown_draws_at_the_idle_heartbeat_and_expires() {
    let harness = IdleLoop::new();
    let wires = harness.wires.clone();
    let draws = wires.draws.clone();
    let script = async move {
        // The failed call raises the peek at simulated t=0, which is also the
        // loop's own clock (`at_ms` is stamped by the run's sink).
        wires
            .events
            .send(failed_tool_event(0))
            .expect("the loop reads events");
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        let peeked = draws.count();
        assert_eq!(
            peeked, 2,
            "the first frame, then one for the peek: the event draws at once"
        );

        // t=1000 and t=2000: the countdown moves, so the heartbeat draws it —
        // one frame a second, not twenty.
        tokio::time::sleep(std::time::Duration::from_millis(2_200)).await;
        let counted = draws.count() - peeked;
        assert_eq!(
            counted, 2,
            "the visible countdown drew {counted} frames in 2.2 simulated seconds"
        );

        // t=3000 expires it: exactly one frame erases the banner.
        tokio::time::sleep(std::time::Duration::from_millis(1_000)).await;
        assert_eq!(
            draws.count() - peeked - counted,
            1,
            "one frame after expiry takes the banner away"
        );

        // Back to idle: nothing moves, so nothing draws.
        let expired = draws.count();
        tokio::time::sleep(std::time::Duration::from_secs(5)).await;
        assert_eq!(
            draws.count() - expired,
            0,
            "an idle screen draws nothing after the peek is gone"
        );
        wires.cancel.cancel();
    };
    harness.run(script).await;
}

/// Issue #141 review: a resize is a frame input of its own — the frame on screen
/// is stale at the new size — so it draws at once, without waiting for the
/// heartbeat a second away.
#[tokio::test(start_paused = true)]
async fn a_resize_draws_immediately() {
    let harness = IdleLoop::new();
    let wires = harness.wires.clone();
    let script = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1_550)).await;
        let before = wires.draws.count();
        wires.keys.send(Input::Resize).expect("the loop reads keys");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        wires.cancel.cancel();
        assert_eq!(
            wires.draws.count() - before,
            1,
            "a resize draws at once, not at the next heartbeat"
        );
    };
    harness.run(script).await;
}

/// Issue #141 review: one parent agent event draws one frame, at once.
#[tokio::test(start_paused = true)]
async fn a_parent_event_draws_immediately() {
    let harness = IdleLoop::new();
    let wires = harness.wires.clone();
    let script = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1_550)).await;
        let before = wires.draws.count();
        wires
            .events
            .send(UiEvent::Agent(p1_tui::runtime::Stamped {
                at_ms: 1_550,
                worker: None,
                event: p1_contracts::AgentEvent::ProviderNotice {
                    text: "transport: retrying over HTTP".into(),
                },
            }))
            .expect("the loop reads events");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        wires.cancel.cancel();
        assert_eq!(
            wires.draws.count() - before,
            1,
            "a parent event draws one frame, at once"
        );
    };
    harness.run(script).await;
}

/// Issue #141 review: an attached worker's own event (#147) is a frame input
/// too — its transcript is what the screen is showing.
#[tokio::test(start_paused = true)]
async fn an_attached_workers_event_draws_immediately() {
    let mut harness = IdleLoop::new();
    // Attach w1 before the loop starts. Its pane row comes from the snapshot the
    // loop's own `sync_workers` reads, so the attach survives the refreshes.
    harness
        .driver
        .worker_rows
        .lock()
        .unwrap()
        .push(worker_row());
    harness.driver.sync_workers();
    harness.driver.screen.pane_mode = p1_tui::state::PaneMode::Workers;
    harness.driver.screen.workers.focused = Some("w1".into());
    harness.driver.screen.attach_selected();
    assert_eq!(
        harness
            .driver
            .screen
            .attached
            .as_ref()
            .map(|row| row.id.as_str()),
        Some("w1"),
        "the loop starts attached to w1"
    );
    let wires = harness.wires.clone();
    let script = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1_550)).await;
        let before = wires.draws.count();
        wires
            .events
            .send(UiEvent::Agent(p1_tui::runtime::Stamped {
                at_ms: 1_550,
                worker: Some("w1".into()),
                event: p1_contracts::AgentEvent::TextDelta {
                    text: "worker prose".into(),
                },
            }))
            .expect("the loop reads events");
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        wires.cancel.cancel();
        assert_eq!(
            wires.draws.count() - before,
            1,
            "an attached worker's event draws one frame, at once"
        );
    };
    harness.run(script).await;
}

/// Park one authorization on the screen (handoff §7.5): the task resolves when
/// the operator answers, exactly as the running turn's own call would.
fn park_authorization(policy: &Arc<TuiPolicy>) -> tokio::task::JoinHandle<Decision> {
    let call = p1_contracts::ToolCall {
        call_id: "c1".into(),
        name: "shell".into(),
        input: p1_contracts::ToolInput::Json("{\"command\":\"cargo check\"}".into()),
    };
    let identity = p1_contracts::ToolIdentity {
        implementation: "shell".into(),
        variant: String::new(),
    };
    let policy = policy.clone();
    tokio::spawn(async move {
        policy
            .authorize(p1_contracts::AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect: p1_contracts::Effect::Executes,
            })
            .await
    })
}

/// Issue #141 review: an authorization arriving at idle is a frame input — the
/// operator must see what is being asked at once, not at the next heartbeat.
#[tokio::test(start_paused = true)]
async fn an_authorization_arrival_draws_immediately() {
    let harness = IdleLoop::with_ask(true);
    let wires = harness.wires.clone();
    let script = async move {
        tokio::time::sleep(std::time::Duration::from_millis(1_550)).await;
        let before = wires.draws.count();
        let parked = park_authorization(&wires.policy);
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(
            wires.draws.count() - before,
            1,
            "an authorization draws one frame, at once"
        );
        // The loop's own key handling answers it (the driver tests' `n`); a
        // simulated timeout turns a missing answer into a failure, not a hang.
        wires
            .keys
            .send(Input::Key(key(KeyCode::Char('n'))))
            .expect("the loop reads keys");
        let decision = tokio::time::timeout(std::time::Duration::from_secs(5), parked)
            .await
            .expect("the loop answers the parked request")
            .expect("the parked task answers");
        assert_eq!(
            decision,
            Decision::Deny {
                reason: p1_tui::runtime::USER_DENY.into()
            }
        );
        wires.cancel.cancel();
    };
    harness.run(script).await;
}

/// Issue #141 review: a frame the terminal refuses is not recorded as drawn, so
/// the next wake retries it — and the frame that finally reaches the screen
/// carries the state the input acted on.
#[test]
fn a_refused_frame_stays_due_and_the_retry_carries_the_state() {
    let (mut d, _auth) = driver();
    let draws = DrawCounter::default();
    let mut redraws = Redraws::new(draws.clone());
    let mut terminal =
        ratatui::Terminal::new(FailingBackend::refusing(1)).expect("the test terminal builds");

    // The operator types; the frame that must show it is refused.
    for c in "qzj".chars() {
        d.on_key(key(KeyCode::Char(c)), None);
    }
    redraws.dirty = true;
    let error = frame_if_due(&mut redraws, &mut terminal, &mut d, 0, ColorMode::TrueColor)
        .expect_err("the terminal refuses the first frame");
    assert!(error.contains("the terminal went away"), "{error}");
    assert_eq!(
        draws.count(),
        1,
        "the refused frame still went through the draw path"
    );
    assert!(
        !frame_text(&terminal).contains("qzj"),
        "a refused frame never reached the screen"
    );

    // The next wake retries it, and this frame does reach the terminal.
    let period = frame_if_due(&mut redraws, &mut terminal, &mut d, 0, ColorMode::TrueColor)
        .expect("the retry reaches the terminal");
    assert_eq!(draws.count(), 2);
    assert!(
        frame_text(&terminal).contains("qzj"),
        "the retry carries the state the keys acted on"
    );
    assert_eq!(
        period, IDLE_HEARTBEAT,
        "the retry waits at the heartbeat the loop is already armed for"
    );
}

/// Issue #141 review: a terminal that keeps refusing frames ends the loop the
/// way `run` reports a terminal it cannot use — after three attempts, one per
/// wake, never a spin.
#[tokio::test(start_paused = true)]
async fn a_persistent_draw_error_ends_the_loop_like_a_missing_terminal() {
    let harness = IdleLoop::with_backend(FailingBackend::refusing(usize::MAX), false);
    let draws = harness.wires.draws.clone();
    let code = harness.run_to_end().await;
    assert_eq!(code, 1, "the loop reports the terminal it cannot use");
    assert_eq!(
        draws.count(),
        DRAW_FAILURES_BEFORE_EXIT as usize,
        "one refused frame per attempt, then the loop gives up"
    );
}

/// Issue #141 review round 2: the retry budget spans turns, not one `pump`. A
/// backend that refuses every frame cannot outlast the loop by ending turns
/// between refusals — the count lives in `Redraws` — so the idle frame, the
/// prompt turn's first frame and the follow-up turn's first frame add up to the
/// three-failure exit. The follow-up IS taken (the assertions below prove it);
/// what it cannot show is a provider request, because the loop gives up on the
/// second pump's first frame, before that pump's turn future is ever polled.
#[tokio::test(start_paused = true)]
async fn a_persistent_draw_error_ends_the_loop_across_turn_boundaries() {
    // Two scripted turns; only the first is reached (a stream that ends with no
    // events is a transport failure, SPEC §3d, so its turn resolves on the
    // pump's first poll and the loop takes the follow-up at the boundary).
    let provider = Arc::new(p1_testkit::ScriptedProvider::new(vec![
        resolving_turn(),
        resolving_turn(),
    ]));
    let agent = agent_with(provider.clone());
    let mut harness = IdleLoop::with_agent(FailingBackend::refusing(usize::MAX), false, agent);
    // The prompt the operator sent, and a follow-up queued behind it. One key,
    // so the frame count below is: the idle first frame, then one frame per pump.
    harness.driver.screen.composer.insert('g');
    harness.driver.follow_ups.push_back("next".into());
    harness
        .wires
        .keys
        .clone()
        .send(Input::Key(key(KeyCode::Enter)))
        .expect("the loop reads keys");
    let draws = harness.wires.draws.clone();

    let (code, driver) = harness.run_to_end_with_driver().await;

    assert_eq!(code, 1, "the loop reports the terminal it cannot use");
    assert_eq!(
        draws.count(),
        DRAW_FAILURES_BEFORE_EXIT as usize,
        "the idle frame and one refused frame per pump share one budget: a count \
         reset per pump would let the loop spend this pump's budget, take the \
         follow-up and only give up three frames later (five in all)"
    );
    assert_eq!(
        operator_prompts(&driver),
        ["g", "next"],
        "the loop took the queued follow-up, so the third refusal came from the \
         second pump's first frame"
    );
    assert_eq!(
        provider.requests().len(),
        1,
        "the second pump refuses its first frame before it polls the turn future, \
         so the follow-up's turn never reaches the provider"
    );
}

#[test]
fn worker_rows_get_latest_context_and_accumulated_cost_without_touching_the_parent() {
    use p1_contracts::Usage;

    let (mut d, _auth) = driver();
    let parent_context = d.screen.context.clone();
    let parent_ctx = d.screen.statusbar.ctx.clone();
    let parent_spend = d.screen.spend;

    d.on_ui_event(worker_response(
        "w1",
        "deepseek-v4.1-flash",
        Some(Usage {
            input_uncached: Some(10_000),
            cost_micro_usd: Some(10),
            ..Usage::default()
        }),
    ));
    d.on_ui_event(worker_response(
        "w1",
        "deepseek-v4.1-flash",
        Some(Usage {
            input_uncached: Some(48_213),
            cost_micro_usd: Some(5),
            ..Usage::default()
        }),
    ));
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1")];
    d.sync_workers();

    let row = &d.screen.workers.workers[0];
    assert_eq!(row.model.as_deref(), Some("deepseek-v4.1-flash"));
    assert_eq!(row.tokens, Some(48_213));
    assert_eq!(row.cost_micro_usd, Some(15));
    assert_eq!(row.context_window, None);
    assert_eq!(d.screen.context, parent_context);
    assert_eq!(d.screen.statusbar.ctx, parent_ctx);
    assert_eq!(d.screen.spend, parent_spend);
}

#[test]
fn missing_worker_usage_poisons_its_latest_tokens_and_accumulated_cost() {
    use p1_contracts::Usage;

    let (mut d, _auth) = driver();
    d.on_ui_event(worker_response(
        "w1",
        "deepseek-v4.1-flash",
        Some(Usage {
            input_uncached: Some(48_213),
            cost_micro_usd: Some(5),
            ..Usage::default()
        }),
    ));
    d.on_ui_event(worker_response("w1", "deepseek-v4.1-flash", None));
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1")];
    d.sync_workers();

    let usage = &d.worker_usage["w1"];
    assert_eq!(usage.tokens, None);
    assert_eq!(usage.cost_micro_usd, None);
    let row = &d.screen.workers.workers[0];
    assert_eq!(row.tokens, None);
    assert_eq!(row.cost_micro_usd, None);
}

#[test]
fn a_worker_row_without_response_events_keeps_its_metrics_unknown() {
    let (mut d, _auth) = driver();
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w2")];
    d.sync_workers();

    let row = &d.screen.workers.workers[0];
    assert_eq!(row.model, None);
    assert_eq!(row.tokens, None);
    assert_eq!(row.cost_micro_usd, None);
}

#[test]
fn worker_clocks_restart_after_continue_and_freeze_once_settled() {
    let t = std::time::Instant::now();
    let mut clocks = WorkerClocks::default();

    assert_eq!(clocks.observe("w1", true, t).as_deref(), Some("0m00s"));
    assert_eq!(
        clocks
            .observe("w1", true, t + std::time::Duration::from_secs(252))
            .as_deref(),
        Some("4m12s")
    );
    assert_eq!(
        clocks
            .observe("w1", false, t + std::time::Duration::from_secs(300))
            .as_deref(),
        Some("5m00s")
    );
    assert_eq!(
        clocks
            .observe("w1", false, t + std::time::Duration::from_secs(9_999))
            .as_deref(),
        Some("5m00s")
    );
    assert_eq!(
        clocks
            .observe("w1", true, t + std::time::Duration::from_secs(10_000))
            .as_deref(),
        Some("0m00s")
    );
    assert_eq!(
        clocks
            .observe("w1", true, t + std::time::Duration::from_secs(10_030))
            .as_deref(),
        Some("0m30s")
    );
    assert_eq!(
        clocks
            .observe("w1", false, t + std::time::Duration::from_secs(10_060))
            .as_deref(),
        Some("1m00s")
    );
    assert_eq!(
        clocks
            .observe("w1", false, t + std::time::Duration::from_secs(20_000))
            .as_deref(),
        Some("1m00s")
    );
    assert_eq!(
        clocks.observe("w2", false, t + std::time::Duration::from_secs(5)),
        None
    );
    assert_eq!(
        clocks.observe("w2", false, t + std::time::Duration::from_secs(50)),
        None
    );
}

// ------------------------------------------------- attached worker detail (§9.5)

/// A worker's own stream is buffered for the attach view, never drawn into the
/// parent's transcript (handoff §9.5): attaching later shows detail the parent
/// stream never carried.
#[test]
fn a_worker_stream_is_buffered_for_attach_and_stays_out_of_the_parent() {
    let (mut d, _auth) = driver();
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 7,
        worker: Some("w1".into()),
        event: p1_contracts::AgentEvent::TextDelta {
            text: "worker prose".into(),
        },
    }));

    assert!(
        !d.screen
            .transcript
            .blocks
            .iter()
            .any(|b| matches!(b, p1_tui::transcript::Block::Prose { .. })),
        "the parent never shows a worker's prose"
    );
    let buffered = d
        .screen
        .worker_transcripts
        .get("w1")
        .expect("w1's stream is buffered while it is detached");
    assert_eq!(
        buffered.blocks,
        vec![p1_tui::transcript::Block::Prose {
            lines: vec!["worker prose".into()],
        }]
    );
}

/// The approval owns the transcript area (handoff §7.5), so parking one while
/// attached to a worker detaches it; the buffer is kept, not dropped.
#[tokio::test]
async fn an_approval_while_attached_detaches_the_worker() {
    use p1_tui::state::PaneMode;

    let (policy, mut auth_rx) = TuiPolicy::new(true, CancellationToken::new());
    let (mut d, _auth) = driver();
    d.policy = Arc::new(policy);
    d.on_ui_event(UiEvent::Agent(p1_tui::runtime::Stamped {
        at_ms: 4,
        worker: Some("w1".into()),
        event: p1_contracts::AgentEvent::TextDelta {
            text: "worker prose".into(),
        },
    }));
    d.screen.pane_mode = PaneMode::Workers;
    d.screen.workers.workers = vec![worker_test_row("w1")];
    d.screen.workers.focused = Some("w1".into());
    d.screen.attach_selected();
    assert_eq!(
        d.screen.attached.as_ref().map(|worker| worker.id.as_str()),
        Some("w1")
    );

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
    assert!(
        d.screen.attached.is_none(),
        "the approval needs the transcript area"
    );
    d.on_key(key(KeyCode::Char('n')), None);
    assert_eq!(
        pending.await.unwrap(),
        Decision::Deny {
            reason: p1_tui::runtime::USER_DENY.into()
        }
    );
    assert_eq!(
        d.screen
            .worker_transcripts
            .get("w1")
            .map(|transcript| transcript.blocks.len()),
        Some(1),
        "detaching keeps the worker's buffered transcript"
    );
}

/// An open OUTPUT fold scrolls on the bare arrows only while OUTPUT is the
/// pane's mode; another mode showing keeps them out of the hidden pane.
#[test]
fn only_output_mode_takes_the_bare_arrows() {
    use p1_tui::render::output::OutputView;
    use p1_tui::state::PaneMode;

    let (mut d, _auth) = driver();
    d.screen.output = Some(OutputView {
        id: p1_tui::fold::FoldId::of("folded output"),
        lines: vec!["a".into(), "b".into(), "c".into()],
        scroll: 0,
    });
    d.screen.pane_mode = PaneMode::Ledger;
    d.on_key(key(KeyCode::Down), None);
    assert_eq!(
        d.screen.output.as_ref().map(|output| output.scroll),
        Some(0),
        "LEDGER leaves the hidden OUTPUT pane alone"
    );

    d.screen.pane_mode = PaneMode::Output;
    d.on_key(key(KeyCode::Down), None);
    assert_eq!(
        d.screen.output.as_ref().map(|output| output.scroll),
        Some(1),
        "OUTPUT mode scrolls it"
    );
}

#[test]
fn ctrl_f_off_detaches_and_returns_keys_to_the_composer() {
    use p1_tui::state::PaneWidth;

    let (mut d, _auth) = driver();
    d.screen.pane_width = PaneWidth::Wide;
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1"), worker_test_row("w2")];
    d.sync_workers();
    d.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        None,
    );
    d.on_key(key(KeyCode::Down), None);
    d.on_key(key(KeyCode::Enter), None);
    assert_eq!(
        d.screen.attached.as_ref().map(|worker| worker.id.as_str()),
        Some("w2")
    );

    d.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        None,
    );
    assert!(d.screen.attached.is_none());
    d.on_key(key(KeyCode::Char('h')), None);
    d.on_key(key(KeyCode::Char('i')), None);
    assert_eq!(d.screen.composer.text, "hi");
    d.on_key(key(KeyCode::Enter), None);
    assert!(d.screen.attached.is_none());
}

#[test]
fn ctrl_n_detaches_an_attached_worker() {
    use p1_tui::state::PaneWidth;

    let (mut d, _auth) = driver();
    d.screen.pane_width = PaneWidth::Wide;
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1"), worker_test_row("w2")];
    d.sync_workers();
    d.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        None,
    );
    d.on_key(key(KeyCode::Enter), None);
    assert!(d.screen.attached.is_some());

    d.on_key(
        KeyEvent::new(KeyCode::Char('n'), KeyModifiers::CONTROL),
        None,
    );
    assert!(d.screen.attached.is_none());
}

#[test]
fn worker_stop_confirmation_keeps_or_dispatches_through_the_stop_channel() {
    use p1_tui::state::PaneWidth;

    let (mut d, _auth) = driver();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    d.worker_stops = Some(tx);
    d.screen.pane_width = PaneWidth::Wide;
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1"), worker_test_row("w2")];
    d.sync_workers();
    d.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        None,
    );
    d.on_key(key(KeyCode::Down), None);
    d.on_key(key(KeyCode::Char('x')), None);
    assert_eq!(d.screen.stop_pending.as_deref(), Some("w2"));

    d.on_key(key(KeyCode::Char('n')), None);
    assert_eq!(d.screen.stop_pending, None);
    assert!(rx.try_recv().is_err());

    d.on_key(key(KeyCode::Char('x')), None);
    d.on_key(key(KeyCode::Char('y')), None);
    assert_eq!(d.screen.stop_pending, None);
    assert_eq!(rx.try_recv(), Ok("w2".to_string()));
    assert!(d.screen.transcript.blocks.iter().any(|block| matches!(
        block,
        p1_tui::transcript::Block::Meta { text } if text == "↳ w2 stop requested"
    )));
}

#[test]
fn worker_stop_without_the_host_channel_reports_that_it_cannot_stop() {
    use p1_tui::state::PaneWidth;

    let (mut d, _auth) = driver();
    d.screen.pane_width = PaneWidth::Wide;
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1"), worker_test_row("w2")];
    d.sync_workers();
    d.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        None,
    );
    d.on_key(key(KeyCode::Down), None);
    d.on_key(key(KeyCode::Char('x')), None);
    d.on_key(key(KeyCode::Char('y')), None);

    assert_eq!(d.screen.stop_pending, None);
    assert!(d.screen.transcript.blocks.iter().any(|block| matches!(
        block,
        p1_tui::transcript::Block::Meta { text }
            if text == "↳ w2 cannot be stopped here"
    )));
}

#[test]
fn worker_rows_show_only_their_configured_context_window_without_touching_the_parent() {
    let (mut d, _auth) = driver();
    let parent_context = d.screen.context.clone();
    let parent_ctx = d.screen.statusbar.ctx.clone();
    d.worker_windows
        .lock()
        .unwrap()
        .insert("w1".into(), 1_048_576);
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1"), worker_test_row("w2")];

    d.sync_workers();

    assert_eq!(d.screen.workers.workers[0].context_window, Some(1_048_576));
    assert_eq!(d.screen.workers.workers[1].context_window, None);
    assert_eq!(d.screen.context, parent_context);
    assert_eq!(d.screen.statusbar.ctx, parent_ctx);
}

#[test]
fn worker_context_window_sync_keeps_filling_usage_from_worker_responses() {
    use p1_contracts::Usage;

    let (mut d, _auth) = driver();
    d.worker_windows
        .lock()
        .unwrap()
        .insert("w1".into(), 1_048_576);
    d.on_ui_event(worker_response(
        "w1",
        "deepseek-v4.1-flash",
        Some(Usage {
            input_uncached: Some(48_213),
            cost_micro_usd: Some(5),
            ..Usage::default()
        }),
    ));
    *d.worker_rows.lock().unwrap() = vec![worker_test_row("w1")];

    d.sync_workers();

    let row = &d.screen.workers.workers[0];
    assert_eq!(row.context_window, Some(1_048_576));
    assert_eq!(row.model.as_deref(), Some("deepseek-v4.1-flash"));
    assert_eq!(row.tokens, Some(48_213));
    assert_eq!(row.cost_micro_usd, Some(5));
}

/// ADR-0075: the front end's workflow calls reach the screen's tree through the sink's
/// channel, and `x`/`y` on a run header sends the run to the host's canceller.
#[cfg(feature = "workflows")]
#[test]
fn workflow_calls_build_the_tree_and_a_run_header_cancels_through_the_run_channel() {
    use p1_tui::state::{PaneMode, PaneWidth};

    let front_end = TuiFrontEnd::new(
        TuiOptions {
            env: "claude".into(),
            ask: false,
            workspace: std::path::PathBuf::from("/workspace"),
            sandbox: "off".into(),
            effort: None,
        },
        CancellationToken::new(),
    );
    let mut events = front_end.events.lock().unwrap().take().unwrap();
    front_end.workflow_run_started(&crate::frontend::WorkflowRunStarted {
        id: "wf1".into(),
        resumed_from: None,
    });
    front_end.workflow_phase("wf1", "Review");
    front_end.workflow_line("workflow wf1 phase: Review");

    let (mut d, _auth) = driver();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    d.run_cancels = Some(tx);
    let mut forwarded = 0;
    while let Ok(event) = events.try_recv() {
        assert!(matches!(event, UiEvent::Workflow { .. }), "{event:?}");
        d.on_ui_event(event);
        forwarded += 1;
    }
    assert_eq!(forwarded, 2, "the ledger line is not a tree event");
    let run = d
        .screen
        .workers
        .tree
        .run("wf1")
        .expect("the run is in the tree");
    assert_eq!(
        run.current_phase().map(|phase| phase.name.as_str()),
        Some("Review")
    );

    d.screen.pane_width = PaneWidth::Wide;
    d.screen.pane_mode = PaneMode::Workers;
    d.on_key(
        KeyEvent::new(KeyCode::Char('f'), KeyModifiers::CONTROL),
        None,
    );
    assert_eq!(d.screen.workers.focused.as_deref(), Some("wf1"));
    d.on_key(key(KeyCode::Char('x')), None);
    assert_eq!(d.screen.stop_pending.as_deref(), Some("wf1"));
    d.on_key(key(KeyCode::Char('y')), None);
    assert_eq!(d.screen.stop_pending, None);
    assert_eq!(rx.try_recv(), Ok("wf1".to_string()));
    assert!(d.screen.transcript.blocks.iter().any(|block| matches!(
        block,
        p1_tui::transcript::Block::Meta { text } if text == "↳ wf1 cancel requested"
    )));
}

/// ADR-0075 / #141's idle rule: a live run's tree redraws on the heartbeat only while the
/// pane shows it — with the pane off, a running workflow draws no periodic frames.
#[cfg(feature = "workflows")]
#[tokio::test(start_paused = true)]
async fn a_live_run_draws_on_the_heartbeat_only_while_its_tree_is_visible() {
    use p1_tui::state::{PaneMode, PaneWidth};

    async fn frames_in_five_seconds(pane_width: PaneWidth, setup: fn(&mut Screen)) -> usize {
        let mut harness =
            IdleLoop::with_backend(ratatui::backend::TestBackend::new(120, 40), false);
        // Pinned, so the run's start does not promote (and open) the pane.
        harness.driver.screen.pinned = true;
        harness.driver.screen.pane_mode = PaneMode::Workers;
        harness.driver.screen.pane_width = pane_width;
        setup(&mut harness.driver.screen);
        let wires = harness.wires.clone();
        let script = async move {
            wires
                .events
                .send(UiEvent::Workflow {
                    at_ms: 0,
                    event: p1_tui::workflow::WorkflowEvent::RunStarted(
                        p1_tui::workflow::RunStarted {
                            id: "wf1".into(),
                            resumed_from: None,
                        },
                    ),
                })
                .expect("the loop reads events");
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            let before = wires.draws.count();
            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            wires.cancel.cancel();
            wires.draws.count() - before
        };
        let frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = frames.clone();
        harness
            .run(async move {
                seen.store(script.await, std::sync::atomic::Ordering::SeqCst);
            })
            .await;
        frames.load(std::sync::atomic::Ordering::SeqCst)
    }

    assert_eq!(
        frames_in_five_seconds(PaneWidth::Off, |_| {}).await,
        0,
        "a hidden tree draws nothing on its own"
    );
    let shown = frames_in_five_seconds(PaneWidth::Wide, |_| {}).await;
    assert!(
        (3..=5).contains(&shown),
        "a visible live tree redraws once a second: {shown}"
    );
    // A review flag left open by an earlier diff decision covers nothing once that
    // decision is gone: the tree keeps its refresh.
    let stale = frames_in_five_seconds(PaneWidth::Wide, |screen| {
        screen.review.open = true;
        screen.approval = None;
    })
    .await;
    assert!(
        (3..=5).contains(&stale),
        "a stale review flag must not stop the tree: {stale}"
    );
    // A diff taller than the transcript opens the full review by itself (`review.open`
    // stays false): it covers the pane, so the tree draws nothing on its own.
    assert_eq!(
        frames_in_five_seconds(PaneWidth::Wide, |screen| {
            let old: String = (0..80).map(|n| format!("old {n}\n")).collect();
            let new: String = (0..80).map(|n| format!("new {n}\n")).collect();
            screen.approval = Some(p1_tui::state::Approval::Diff(
                p1_tui::render::diff::DiffView::from_edit(
                    "edit",
                    "src/lib.rs",
                    &old,
                    &new,
                    Some(&old),
                    (1, 1),
                ),
            ));
            assert!(!screen.review.open);
        })
        .await,
        0,
        "an auto-opened review covers the pane"
    );
}
