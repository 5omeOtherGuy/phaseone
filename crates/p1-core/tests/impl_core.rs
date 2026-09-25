//! Implementer's own focused tests. These fill gaps the frozen suites leave:
//! multiple tool rounds in one turn, large inbox bursts, a commit failure in the
//! middle of an inbox batch, environment ordering on an inbox-only turn, and the
//! cancellation boundary between tool rounds. Same determinism rules as the frozen
//! suites: paused time, explicit timeouts, no sleeps, no network, no filesystem.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{
    AgentEvent, BoxFuture, CancellationToken, CommitError, CommitSink, Effect, InboxKind,
    InterruptionReason, Item, JournalRecord, ModelOptions, RecordBody, StopReason, Tool,
    ToolOutcome, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, Step, json_call, text_response, tool_call_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);
const UNKNOWN_OUTCOME: &str = "Interrupted: this call was started before the session stopped and its outcome is unknown. Check the current state before retrying.";

struct Fixture {
    provider: Arc<ScriptedProvider>,
    journal: Arc<RecordingJournal>,
    events: Arc<RecordingEvents>,
}

fn parts(
    provider: Arc<ScriptedProvider>,
    tools: Vec<Arc<dyn Tool>>,
    journal: Arc<RecordingJournal>,
) -> (AgentParts, Fixture) {
    let events = Arc::new(RecordingEvents::new());
    let authorization = Arc::new(ScriptedAuthorization::permit_all());
    (
        AgentParts {
            provider: provider.clone(),
            tools,
            system_prompt: "impl prompt".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization,
            journal: journal.clone(),
            events: events.clone(),
        },
        Fixture {
            provider,
            journal,
            events,
        },
    )
}

fn agent_with(script: Vec<Step>) -> (Agent, Fixture) {
    agent_with_tools(script, vec![], RecordingJournal::new())
}

fn agent_with_tools(
    script: Vec<Step>,
    tools: Vec<Arc<dyn Tool>>,
    journal: RecordingJournal,
) -> (Agent, Fixture) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let journal = Arc::new(journal);
    let (parts, fixture) = parts(provider, tools, journal);
    (Agent::new(parts).expect("agent builds"), fixture)
}

async fn run(agent: &mut Agent, input: &str, cancel: CancellationToken) -> TurnEnd {
    timeout(LIMIT, agent.run_turn(input.into(), cancel))
        .await
        .expect("run_turn hung")
}

async fn run_inbox(agent: &mut Agent, cancel: CancellationToken) -> Option<TurnEnd> {
    timeout(LIMIT, agent.run_inbox_turn(cancel))
        .await
        .expect("run_inbox_turn hung")
}

fn body(record: &JournalRecord) -> RecordBody {
    record.body.clone()
}

fn inbox_texts(journal: &RecordingJournal) -> Vec<String> {
    journal
        .records()
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::Inbox { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

// A turn with two tool rounds: every request must see a history where each
// assistant tool call has its result already paired (invariant 5d).
#[tokio::test(start_paused = true)]
async fn two_tool_rounds_keep_every_request_history_paired() {
    let tool = Arc::new(FakeTool::new("alpha"));
    let (mut agent, fixture) = agent_with_tools(
        vec![
            tool_call_response(vec![json_call("c1", "alpha", "{}")]),
            tool_call_response(vec![json_call("c2", "alpha", "{}")]),
            text_response("done"),
        ],
        vec![tool.clone()],
        RecordingJournal::new(),
    );

    let end = run(&mut agent, "go", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(tool.calls().len(), 2);

    let requests = fixture.provider.requests();
    assert_eq!(requests.len(), 3);
    for request in &requests {
        let calls: Vec<&str> = request
            .history
            .iter()
            .filter_map(|item| match item {
                Item::Assistant(assistant) => {
                    Some(assistant.tool_calls().map(|call| call.call_id.as_str()))
                }
                _ => None,
            })
            .flatten()
            .collect();
        let results: Vec<&str> = request
            .history
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult(result) => Some(result.call_id.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(calls, results, "unpaired tool call in a request history");
    }

    let paired: Vec<(&str, &str)> = agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::Assistant(assistant) => Some(assistant.tool_calls()),
            _ => None,
        })
        .flatten()
        .map(|call| call.call_id.as_str())
        .zip(agent.history().iter().filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        }))
        .collect();
    assert_eq!(paired, vec![("c1", "c1"), ("c2", "c2")]);
}

// Cancellation landing exactly on the boundary after the last tool result: the
// running call was awaited and paired, and the turn ends Cancelled before the
// next provider request.
#[tokio::test(start_paused = true)]
async fn cancellation_after_a_tool_round_pairs_the_call_and_stops_before_requester() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let journal = RecordingJournal::new().with_commit_hook(move |record| {
        if matches!(record.body, RecordBody::ToolFinished { .. }) {
            trigger.cancel();
        }
    });
    let tool = Arc::new(FakeTool::new("alpha"));
    let (mut agent, fixture) = agent_with_tools(
        vec![tool_call_response(vec![json_call("c1", "alpha", "{}")])],
        vec![tool.clone()],
        journal,
    );

    let end = run(&mut agent, "go", cancel).await;
    assert_eq!(end, TurnEnd::Cancelled);
    assert_eq!(fixture.provider.requests().len(), 1);
    assert_eq!(tool.calls().len(), 1);
    let results: Vec<_> = agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some((result.call_id.as_str(), result.status)),
            _ => None,
        })
        .collect();
    assert_eq!(results, vec![("c1", ToolStatus::Ok)]);
}

// An inbox-only turn still commits Environment before the Inbox record (§2), and
// consumes the message exactly once.
#[tokio::test(start_paused = true)]
async fn inbox_only_turn_commits_environment_before_inbox() {
    let (mut agent, fixture) = agent_with(vec![text_response("ok")]);
    assert!(agent.inbox().send(InboxKind::Notification, "n"));

    let end = run_inbox(&mut agent, CancellationToken::new()).await;
    assert_eq!(
        end,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );
    let seqs: Vec<u64> = fixture.journal.records().iter().map(|r| r.seq).collect();
    assert_eq!(seqs, vec![0, 1, 2]);
    assert!(matches!(
        body(&fixture.journal.records()[0]),
        RecordBody::Environment { .. }
    ));
    assert!(matches!(
        body(&fixture.journal.records()[1]),
        RecordBody::Inbox { .. }
    ));
    assert!(!agent.has_pending_inbox());
}

// A cancel takes back queued steering (ADR-0047): only undelivered messages of
// the named kind leave the inbox, in send order; notifications stay.
#[tokio::test(start_paused = true)]
async fn withdraw_takes_back_only_undelivered_messages_of_one_kind() {
    let (mut agent, _fixture) = agent_with(vec![text_response("ok")]);
    let inbox = agent.inbox();
    assert!(inbox.send(InboxKind::Steering, "first"));
    assert!(inbox.send(InboxKind::Notification, "worker done"));
    assert!(inbox.send(InboxKind::Steering, "second"));
    assert_eq!(
        inbox.withdraw(InboxKind::Steering),
        vec!["first".to_string(), "second".to_string()]
    );
    assert!(inbox.withdraw(InboxKind::Steering).is_empty());
    assert!(agent.has_pending_inbox(), "the notification stays queued");
    run_inbox(&mut agent, CancellationToken::new()).await;
    assert!(!agent.has_pending_inbox());
    assert!(inbox.withdraw(InboxKind::Notification).is_empty());
}

// A pre-cancelled inbox turn races cancellation before the provider is ever
// asked and records the interruption without touching history.
#[tokio::test(start_paused = true)]
async fn precancelled_inbox_turn_never_calls_the_provider() {
    let (mut agent, fixture) = agent_with(vec![text_response("never")]);
    assert!(agent.inbox().send(InboxKind::Steering, "stop"));
    let cancel = CancellationToken::new();
    cancel.cancel();

    let end = run_inbox(&mut agent, cancel).await;
    assert_eq!(end, Some(TurnEnd::Cancelled));
    assert!(fixture.provider.requests().is_empty());
    assert_eq!(
        body(&fixture.journal.records()[2]),
        RecordBody::AssistantInterrupted {
            reason: InterruptionReason::Cancelled,
            partial_text: String::new(),
            error: None,
        }
    );
    let inbox_items = agent
        .history()
        .iter()
        .filter(|item| matches!(item, Item::Inbox { .. }))
        .count();
    assert_eq!(inbox_items, 1);
}

// A large burst queued while idle is delivered once, in arrival order, at the
// next request boundary.
#[tokio::test(start_paused = true)]
async fn large_inbox_burst_is_delivered_once_and_in_order() {
    let (mut agent, fixture) = agent_with(vec![text_response("ok"), text_response("again")]);
    let inbox = agent.inbox();
    for index in 0..64u32 {
        assert!(inbox.send(InboxKind::Steering, format!("m{index}")));
    }

    let first = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        first,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    let delivered = inbox_texts(&fixture.journal);
    let expected: Vec<String> = (0..64u32).map(|index| format!("m{index}")).collect();
    assert_eq!(delivered, expected);
    assert!(!agent.has_pending_inbox());

    let request = &fixture.provider.requests()[0];
    let history_inbox: Vec<String> = request
        .history
        .iter()
        .filter_map(|item| match item {
            Item::Inbox { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(history_inbox, expected);

    // A second turn delivers nothing new.
    run(&mut agent, "q2", CancellationToken::new()).await;
    assert_eq!(inbox_texts(&fixture.journal), expected);
}

// A commit failure part-way through an inbox batch stops the turn, and the
// undelivered messages stay pending so a later turn delivers each exactly once.
#[tokio::test(start_paused = true)]
async fn failed_inbox_commit_requeues_undelivered_messages() {
    let journal = RecordingJournal::new().failing_once_at(2);
    let (mut agent, fixture) = agent_with_tools(vec![text_response("done")], vec![], journal);
    let inbox = agent.inbox();
    assert!(inbox.send(InboxKind::Notification, "a"));
    assert!(inbox.send(InboxKind::Notification, "b"));

    let first = run(&mut agent, "one", CancellationToken::new()).await;
    assert!(matches!(first, TurnEnd::CommitFailed { .. }), "{first:?}");
    assert!(fixture.provider.requests().is_empty());
    assert!(agent.has_pending_inbox());
    assert!(inbox_texts(&fixture.journal).is_empty());

    let second = run(&mut agent, "two", CancellationToken::new()).await;
    assert_eq!(
        second,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(inbox_texts(&fixture.journal), vec!["a", "b"]);
    assert_eq!(
        fixture
            .journal
            .records()
            .iter()
            .map(|record| record.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5]
    );
    assert!(!agent.has_pending_inbox());
}

// R2 for an inbox record: the next inbox-only turn reuses the failed seq and
// never loses the message.
#[tokio::test(start_paused = true)]
async fn inbox_turn_after_a_commit_failure_reuses_the_sequence_number() {
    let journal = RecordingJournal::new().failing_once_at(1);
    let (mut agent, fixture) = agent_with_tools(vec![text_response("done")], vec![], journal);
    assert!(agent.inbox().send(InboxKind::Steering, "retry"));

    let first = run_inbox(&mut agent, CancellationToken::new()).await;
    assert!(
        matches!(first, Some(TurnEnd::CommitFailed { .. })),
        "{first:?}"
    );
    assert!(agent.has_pending_inbox());

    let second = run_inbox(&mut agent, CancellationToken::new()).await;
    assert_eq!(
        second,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );
    assert_eq!(
        fixture
            .journal
            .records()
            .iter()
            .map(|record| record.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2]
    );
    assert_eq!(inbox_texts(&fixture.journal), vec!["retry"]);
}

// The core passes a tool's status and content through verbatim, including an
// `Unknown` status it did not invent.
#[tokio::test(start_paused = true)]
async fn tool_result_status_and_content_are_verbatim() {
    let tool = Arc::new(FakeTool::new("odd").returning(ToolOutcome {
        status: ToolStatus::Unknown,
        content: "unreconciled".into(),
    }));
    let (mut agent, fixture) = agent_with_tools(
        vec![
            tool_call_response(vec![json_call("u1", "odd", "{}")]),
            text_response("done"),
        ],
        vec![tool.clone()],
        RecordingJournal::new(),
    );

    run(&mut agent, "go", CancellationToken::new()).await;
    let finished = fixture
        .journal
        .records()
        .into_iter()
        .find_map(|record| match record.body {
            RecordBody::ToolFinished { result } => Some(result),
            _ => None,
        })
        .expect("a ToolFinished record");
    assert_eq!(finished.status, ToolStatus::Unknown);
    assert_eq!(finished.content, "unreconciled");
    assert!(fixture.events.events().contains(&AgentEvent::ToolFinished {
        result: finished.clone()
    }));
    assert!(agent.history().contains(&Item::ToolResult(finished)));
}

// The authorization policy sees the tool's declared effect, and only for a tool
// that actually exists.
#[tokio::test(start_paused = true)]
async fn authorization_sees_the_effect_for_existing_tools_only() {
    let tool = Arc::new(FakeTool::new("writer").with_effect(Effect::WritesFiles));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("w", "writer", "{}"),
            json_call("g", "ghost", "{}"),
        ]),
        text_response("done"),
    ]));
    let authorization = Arc::new(ScriptedAuthorization::permit_all());
    let journal = Arc::new(RecordingJournal::new());
    let parts = AgentParts {
        provider,
        tools: vec![tool],
        system_prompt: "impl prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: authorization.clone(),
        journal,
        events: Arc::new(RecordingEvents::new()),
    };
    let mut agent = Agent::new(parts).expect("agent builds");

    run(&mut agent, "go", CancellationToken::new()).await;
    assert_eq!(
        authorization.seen(),
        vec![("w".into(), Effect::WritesFiles)]
    );
}

// R5: reconciliation also runs at the start of an inbox-only turn, before its
// Inbox record, and never re-executes the unresolved call.
#[tokio::test(start_paused = true)]
async fn reconciliation_runs_at_the_start_of_an_inbox_only_turn() {
    let tool = Arc::new(FakeTool::new("work"));
    // seq 0 env, 1 user, 2 assistant, 3 ToolStarted, 4 = ToolFinished fails once.
    let journal = RecordingJournal::new().failing_once_at(4);
    let (mut agent, fixture) = agent_with_tools(
        vec![
            tool_call_response(vec![json_call("c1", "work", "{}")]),
            text_response("done"),
        ],
        vec![tool.clone()],
        journal,
    );

    let first = run(&mut agent, "one", CancellationToken::new()).await;
    assert!(matches!(first, TurnEnd::CommitFailed { .. }), "{first:?}");
    assert_eq!(tool.calls().len(), 1);
    assert!(agent.inbox().send(InboxKind::Notification, "ping"));

    let second = run_inbox(&mut agent, CancellationToken::new()).await;
    assert_eq!(
        second,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );
    assert_eq!(
        tool.calls().len(),
        1,
        "reconciliation never re-executes the call"
    );

    let kinds: Vec<&str> = fixture
        .journal
        .records()
        .iter()
        .map(|record| match record.body {
            RecordBody::Environment { .. } => "environment",
            RecordBody::UserInput { .. } => "user_input",
            RecordBody::Inbox { .. } => "inbox",
            RecordBody::AssistantCompleted { .. } => "assistant_completed",
            RecordBody::AssistantInterrupted { .. } => "assistant_interrupted",
            RecordBody::ToolStarted { .. } => "tool_started",
            RecordBody::ToolFinished { .. } => "tool_finished",
            RecordBody::ContextReplaced { .. } => "context_replaced",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "environment",
            "user_input",
            "assistant_completed",
            "tool_started",
            "tool_finished",
            "inbox",
            "assistant_completed",
        ]
    );
    assert_eq!(
        fixture
            .journal
            .records()
            .iter()
            .map(|record| record.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5, 6]
    );
    let result = agent
        .history()
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("a reconciled result");
    assert_eq!(result.status, ToolStatus::Unknown);
    assert_eq!(result.content, UNKNOWN_OUTCOME);
}

/// A commit sink that rejects the first two `ToolFinished` commits, so a failure
/// can be injected *inside* reconciliation rather than in the turn that leaves the
/// call unresolved.
#[derive(Clone)]
struct FailFirstTwoToolFinished {
    inner: RecordingJournal,
    attempts: Arc<Mutex<u32>>,
}

impl CommitSink for FailFirstTwoToolFinished {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        Box::pin(async move {
            if matches!(&record.body, RecordBody::ToolFinished { .. }) {
                let mut attempts = self.attempts.lock().unwrap();
                if *attempts < 2 {
                    *attempts += 1;
                    return Err(CommitError("reconciliation failed".into()));
                }
            }
            self.inner.commit(record).await
        })
    }
}

// R5: a commit failure *during* reconciliation ends that turn like any other, the
// in-memory started set is not lost, and a third turn finishes the reconciliation.
#[tokio::test(start_paused = true)]
async fn a_commit_failure_during_reconciliation_is_retried_by_the_next_turn() {
    let tool = Arc::new(FakeTool::new("work"));
    let inner = RecordingJournal::new();
    let journal = FailFirstTwoToolFinished {
        inner: inner.clone(),
        attempts: Arc::new(Mutex::new(0)),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "work", "{}")]),
        text_response("done"),
    ]));
    let parts = AgentParts {
        provider: provider.clone(),
        tools: vec![tool.clone()],
        system_prompt: "impl prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(journal),
        events: Arc::new(RecordingEvents::new()),
    };
    let mut agent = Agent::new(parts).expect("agent builds");

    // Turn 1: ToolFinished of the real execution fails, leaving c1 unresolved.
    let first = run(&mut agent, "one", CancellationToken::new()).await;
    assert!(matches!(first, TurnEnd::CommitFailed { .. }), "{first:?}");
    assert_eq!(tool.calls().len(), 1);

    // Turn 2: reconciliation itself fails.
    let second = run(&mut agent, "two", CancellationToken::new()).await;
    assert!(matches!(second, TurnEnd::CommitFailed { .. }), "{second:?}");
    assert_eq!(
        inner.records().len(),
        4,
        "env, user, assistant, tool_started"
    );
    assert_eq!(tool.calls().len(), 1, "no re-execution on retry");
    assert_eq!(
        provider.requests().len(),
        1,
        "no provider request in a failed turn"
    );

    // Turn 3: reconciliation finally commits, then the turn proceeds.
    let third = run(&mut agent, "three", CancellationToken::new()).await;
    assert_eq!(
        third,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(tool.calls().len(), 1);
    assert_eq!(
        inner
            .records()
            .iter()
            .map(|record| record.seq)
            .collect::<Vec<_>>(),
        vec![0, 1, 2, 3, 4, 5, 6]
    );
    let kinds: Vec<&str> = inner
        .records()
        .iter()
        .map(|record| match record.body {
            RecordBody::Environment { .. } => "environment",
            RecordBody::UserInput { .. } => "user_input",
            RecordBody::Inbox { .. } => "inbox",
            RecordBody::AssistantCompleted { .. } => "assistant_completed",
            RecordBody::AssistantInterrupted { .. } => "assistant_interrupted",
            RecordBody::ToolStarted { .. } => "tool_started",
            RecordBody::ToolFinished { .. } => "tool_finished",
            RecordBody::ContextReplaced { .. } => "context_replaced",
        })
        .collect();
    assert_eq!(
        kinds,
        vec![
            "environment",
            "user_input",
            "assistant_completed",
            "tool_started",
            "tool_finished",
            "user_input",
            "assistant_completed",
        ]
    );
    let result = agent
        .history()
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.clone()),
            _ => None,
        })
        .expect("a reconciled result");
    assert_eq!(result.status, ToolStatus::Unknown);
    assert_eq!(result.content, UNKNOWN_OUTCOME);
}

// R5: reconciliation resolves leftover calls without asking authorization or
// re-executing anything. Turn 1 legitimately asked once, for c1, BEFORE its
// ToolStarted commit failed; that single entry is the expected full history.
#[tokio::test(start_paused = true)]
async fn reconciliation_asks_no_authorization_and_never_re_executes() {
    let tool = Arc::new(FakeTool::new("work"));
    let authorization = Arc::new(ScriptedAuthorization::permit_all());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("c1", "work", "{}"),
            json_call("c2", "work", "{}"),
        ]),
        text_response("recovered"),
    ]));
    let parts = AgentParts {
        provider,
        tools: vec![tool.clone()],
        system_prompt: "impl prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: authorization.clone(),
        // seq 3 = ToolStarted(c1) fails once, leaving both calls unresolved.
        journal: Arc::new(RecordingJournal::new().failing_once_at(3)),
        events: Arc::new(RecordingEvents::new()),
    };
    let mut agent = Agent::new(parts).expect("agent builds");

    let first = run(&mut agent, "one", CancellationToken::new()).await;
    assert!(matches!(first, TurnEnd::CommitFailed { .. }), "{first:?}");
    assert_eq!(authorization.seen(), vec![("c1".into(), Effect::ReadOnly)]);

    let second = run(&mut agent, "two", CancellationToken::new()).await;
    assert_eq!(
        second,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(
        authorization.seen(),
        vec![("c1".into(), Effect::ReadOnly)],
        "reconciliation asks no new authorization"
    );
    assert_eq!(
        tool.calls().len(),
        0,
        "nothing is executed by reconciliation"
    );
}
