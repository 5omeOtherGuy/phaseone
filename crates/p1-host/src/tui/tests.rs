//! Driver tests: no TTY, no terminal — the driver core over channels, with a
//! fake agent task that answers turns immediately.

use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use p1_contracts::{AuthorizationPolicy, StopReason};

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}

/// A driver wired to a real agent task over a scripted (empty) provider;
/// each turn ends immediately with the scripted provider's exhaustion.
fn driver() -> Driver {
    let (policy, _auth) = TuiPolicy::new(false, CancellationToken::new());
    let (_sink, _events) = TuiSink::new();
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
        events: Arc::new(_sink),
    })
    .expect("agent builds");
    let inbox = agent.inbox();
    let (agent_tx, agent_rx) = mpsc::unbounded_channel();
    tokio::spawn(agent_task(agent, agent_rx));
    Driver {
        screen: Screen::new(true),
        options: TuiOptions {
            env: "claude".into(),
            route: "claude".into(),
            model: "sonnet-4.5".into(),
            ask: false,
            workspace: std::env::current_dir().unwrap(),
        },
        agent_tx,
        turn: None,
        inbox,
        policy: Arc::new(policy),
        pending_auth: None,
        follow_ups: VecDeque::new(),
        exit: None,
    }
}

#[tokio::test]
async fn typing_and_enter_submits_a_prompt() {
    let mut d = driver();
    for c in "fix it".chars() {
        d.on_key(key(KeyCode::Char(c)), 0);
    }
    d.on_key(key(KeyCode::Enter), 0);
    assert!(d.turn.is_some(), "a turn started");
    assert_eq!(d.screen.composer.text, "");
}

#[tokio::test]
async fn slash_exit_quits_and_focus_toggles() {
    let mut d = driver();
    for c in "/focus".chars() {
        d.on_key(key(KeyCode::Char(c)), 0);
    }
    d.on_key(key(KeyCode::Enter), 0);
    assert!(d.screen.focus);
    for c in "/exit".chars() {
        d.on_key(key(KeyCode::Char(c)), 0);
    }
    d.on_key(key(KeyCode::Enter), 0);
    assert_eq!(d.exit, Some(0));
}

#[tokio::test]
async fn enter_while_working_queues_steering_to_the_inbox() {
    let mut d = driver();
    d.screen.working = Some(p1_tui::state::Working {
        label: "shell".into(),
        started_ms: 0,
    });
    for c in "use vecdeque".chars() {
        d.on_key(key(KeyCode::Char(c)), 0);
    }
    d.on_key(key(KeyCode::Enter), 0);
    assert_eq!(d.screen.queued.len(), 1);
    assert_eq!(d.screen.queued[0].text, "use vecdeque");
}

#[tokio::test]
async fn a_turn_end_runs_the_oldest_follow_up() {
    let mut d = driver();
    d.follow_ups.push_back("next step".into());
    d.screen.queue(true, "next step".into());
    d.on_turn_end(TurnEnd::Completed {
        stop: StopReason::EndTurn,
    });
    assert!(d.turn.is_some(), "the follow-up started a turn");
    assert!(d.follow_ups.is_empty());
    assert!(d.screen.queued.is_empty());
}

#[tokio::test]
async fn cancel_clears_the_follow_up_queue() {
    let mut d = driver();
    d.follow_ups.push_back("next step".into());
    d.on_turn_end(TurnEnd::Cancelled);
    assert!(d.follow_ups.is_empty());
    assert!(d.turn.is_none());
}

#[tokio::test]
async fn an_auth_request_becomes_the_approval_view_and_answers() {
    let (policy, mut auth_rx) = TuiPolicy::new(true, CancellationToken::new());
    let mut d = driver();
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
    // Park a request from the policy side, as the core would.
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
    // `y` answers it.
    d.on_key(key(KeyCode::Char('y')), 0);
    assert_eq!(pending.await.unwrap(), Decision::Permit);
    assert!(d.screen.approval.is_none());
    assert!(!d.screen.pinned);
}
