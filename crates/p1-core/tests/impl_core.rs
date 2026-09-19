//! Implementer's own focused tests. These fill gaps the frozen suites leave:
//! multiple tool rounds in one turn, large inbox bursts, a commit failure in the
//! middle of an inbox batch, environment ordering on an inbox-only turn, and the
//! cancellation boundary between tool rounds. Same determinism rules as the frozen
//! suites: paused time, explicit timeouts, no sleeps, no network, no filesystem.

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    AgentEvent, CancellationToken, Effect, InboxKind, InterruptionReason, Item, JournalRecord,
    ModelOptions, RecordBody, StopReason, Tool, ToolOutcome, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, Step, json_call, text_response, tool_call_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

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
