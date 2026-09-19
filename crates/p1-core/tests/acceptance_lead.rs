//! Lead acceptance tests: the boundaries the independent authors could not reach with
//! the first test kit (docs/design/core.md rulings R1–R4, §4 row 1 for the FIRST call).
//! FROZEN for implementers.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{
    AgentEvent, CancellationToken, InboxKind, InterruptionReason, Item, ModelOptions, RecordBody,
    StopReason, StreamEvent, Tool, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts, Inbox};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, Step, completed, json_call, text_block, text_response, tool_call_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

struct Rig {
    provider: ScriptedProvider,
    journal: RecordingJournal,
    events: RecordingEvents,
    authorization: ScriptedAuthorization,
}

fn build(script: Vec<Step>, tools: Vec<Arc<dyn Tool>>, journal: RecordingJournal) -> (Agent, Rig) {
    let provider = ScriptedProvider::new(script);
    let events = RecordingEvents::new();
    let authorization = ScriptedAuthorization::permit_all();
    let agent = Agent::new(AgentParts {
        provider: Arc::new(provider.clone()),
        tools,
        system_prompt: "lead prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(authorization.clone()),
        journal: Arc::new(journal.clone()),
        events: Arc::new(events.clone()),
    })
    .expect("agent builds");
    (
        agent,
        Rig {
            provider,
            journal,
            events,
            authorization,
        },
    )
}

async fn turn(agent: &mut Agent, input: &str, cancel: CancellationToken) -> TurnEnd {
    timeout(LIMIT, agent.run_turn(input.into(), cancel))
        .await
        .expect("run_turn hung")
}

fn kinds(journal: &RecordingJournal) -> Vec<&'static str> {
    journal
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
        .collect()
}

fn seqs(journal: &RecordingJournal) -> Vec<u64> {
    journal.records().iter().map(|record| record.seq).collect()
}

// R3 / §1: validate is called exactly once, with the empty-history first request.
#[tokio::test(start_paused = true)]
async fn construction_validates_once_with_the_empty_history_request() {
    let first = Arc::new(FakeTool::new("first"));
    let second = Arc::new(FakeTool::new("second"));
    let (_agent, rig) = build(
        vec![],
        vec![first.clone(), second.clone()],
        RecordingJournal::new(),
    );

    let validated = rig.provider.validated();
    assert_eq!(validated.len(), 1);
    assert_eq!(validated[0].system_prompt, "lead prompt");
    assert!(validated[0].history.is_empty());
    assert_eq!(
        validated[0].tools,
        vec![first.declaration().clone(), second.declaration().clone()]
    );
    assert_eq!(validated[0].options, ModelOptions::default());
    assert!(rig.provider.requests().is_empty());
}

// R1: cancel already fired when the turn starts waiting on the provider — cancellation
// wins over a terminal event that is ready at the same moment.
#[tokio::test(start_paused = true)]
async fn cancellation_wins_over_a_ready_completed_response() {
    let (mut agent, rig) = build(
        vec![text_response("never shown")],
        vec![],
        RecordingJournal::new(),
    );
    let cancel = CancellationToken::new();
    cancel.cancel();

    let end = turn(&mut agent, "hi", cancel).await;

    assert_eq!(end, TurnEnd::Cancelled);
    assert_eq!(
        kinds(&rig.journal),
        vec!["environment", "user_input", "assistant_interrupted"]
    );
    match &rig.journal.records()[2].body {
        RecordBody::AssistantInterrupted { reason, error, .. } => {
            assert_eq!(*reason, InterruptionReason::Cancelled);
            assert_eq!(*error, None);
        }
        other => panic!("unexpected record {other:?}"),
    }
    assert_eq!(agent.history(), &[Item::User { text: "hi".into() }]);
    assert_eq!(
        rig.events.events().last(),
        Some(&AgentEvent::TurnFinished {
            end: TurnEnd::Cancelled
        })
    );
}

// §4 row 1 for the FIRST call + R1: cancel fires right after AssistantCompleted is
// committed. The response stands; no call is authorized or executed; every call gets
// its "Cancelled before execution." result; the turn ends Cancelled.
#[tokio::test(start_paused = true)]
async fn cancel_after_assistant_completed_cancels_every_call_before_execution() {
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let journal = RecordingJournal::new().with_commit_hook(move |record| {
        if matches!(record.body, RecordBody::AssistantCompleted { .. }) {
            trigger.cancel();
        }
    });
    let tool = Arc::new(FakeTool::new("work"));
    let (mut agent, rig) = build(
        vec![tool_call_response(vec![
            json_call("c1", "work", "{}"),
            json_call("c2", "missing", "{}"),
        ])],
        vec![tool.clone()],
        journal,
    );

    let end = turn(&mut agent, "go", cancel).await;

    assert_eq!(end, TurnEnd::Cancelled);
    assert!(
        tool.calls().is_empty(),
        "no tool may run after cancellation"
    );
    assert!(
        rig.authorization.seen().is_empty(),
        "authorization must not be asked"
    );
    assert_eq!(
        kinds(&rig.journal),
        vec![
            "environment",
            "user_input",
            "assistant_completed",
            "tool_finished",
            "tool_finished"
        ]
    );
    let results: Vec<_> = agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some((
                result.call_id.as_str(),
                result.status,
                result.content.as_str(),
            )),
            _ => None,
        })
        .collect();
    assert_eq!(
        results,
        vec![
            ("c1", ToolStatus::Cancelled, "Cancelled before execution."),
            ("c2", ToolStatus::Cancelled, "Cancelled before execution."),
        ]
    );
    assert_eq!(rig.provider.requests().len(), 1);
}

// R4 / §3f: a message arriving after a tool-less response completed keeps the turn going.
#[tokio::test(start_paused = true)]
async fn inbox_message_arriving_after_the_response_continues_the_turn() {
    let slot: Arc<Mutex<Option<Inbox>>> = Arc::new(Mutex::new(None));
    let hook_slot = slot.clone();
    let sent = Arc::new(Mutex::new(false));
    let hook_sent = sent.clone();
    let journal = RecordingJournal::new().with_commit_hook(move |record| {
        let mut already = hook_sent.lock().unwrap();
        if matches!(record.body, RecordBody::AssistantCompleted { .. }) && !*already {
            *already = true;
            let inbox = hook_slot.lock().unwrap().clone().expect("inbox installed");
            assert!(inbox.send(InboxKind::Notification, "child 7 finished"));
        }
    });
    let (mut agent, rig) = build(
        vec![text_response("waiting"), text_response("got it")],
        vec![],
        journal,
    );
    *slot.lock().unwrap() = Some(agent.inbox());

    let end = turn(&mut agent, "start", CancellationToken::new()).await;

    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(
        kinds(&rig.journal),
        vec![
            "environment",
            "user_input",
            "assistant_completed",
            "inbox",
            "assistant_completed"
        ]
    );
    let requests = rig.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].history.last(),
        Some(&Item::Inbox {
            kind: InboxKind::Notification,
            text: "child 7 finished".into()
        })
    );
    let events = rig.events.events();
    assert!(events.contains(&AgentEvent::InboxDelivered { count: 1 }));
    assert!(events.contains(&AgentEvent::RequestStarted { request_index: 1 }));
    assert!(!agent.has_pending_inbox());
}

// R2: after a failed commit the agent stays usable and the failed seq is reused.
#[tokio::test(start_paused = true)]
async fn the_turn_after_a_commit_failure_reuses_the_failed_sequence_number() {
    // seq 0 environment, seq 1 user_input, seq 2 = AssistantCompleted fails once.
    let journal = RecordingJournal::new().failing_once_at(2);
    let (mut agent, rig) = build(
        vec![text_response("lost"), text_response("kept")],
        vec![],
        journal,
    );

    let first = turn(&mut agent, "one", CancellationToken::new()).await;
    assert!(matches!(first, TurnEnd::CommitFailed { .. }), "{first:?}");
    assert_eq!(agent.history(), &[Item::User { text: "one".into() }]);

    let second = turn(&mut agent, "two", CancellationToken::new()).await;
    assert_eq!(
        second,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(seqs(&rig.journal), vec![0, 1, 2, 3]);
    assert_eq!(
        kinds(&rig.journal),
        vec![
            "environment",
            "user_input",
            "user_input",
            "assistant_completed"
        ]
    );
    assert_eq!(agent.history().len(), 3);
}

// R2: a failed Environment commit is retried at seq 0 by the next turn.
#[tokio::test(start_paused = true)]
async fn a_failed_environment_commit_is_repeated_by_the_next_turn() {
    let journal = RecordingJournal::new().failing_once_at(0);
    let (mut agent, rig) = build(vec![text_response("ok")], vec![], journal);

    let first = turn(&mut agent, "one", CancellationToken::new()).await;
    assert!(matches!(first, TurnEnd::CommitFailed { .. }), "{first:?}");
    assert!(agent.history().is_empty());
    assert!(rig.provider.requests().is_empty());

    let second = turn(&mut agent, "two", CancellationToken::new()).await;
    assert_eq!(
        second,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(seqs(&rig.journal), vec![0, 1, 2]);
    assert_eq!(
        kinds(&rig.journal),
        vec!["environment", "user_input", "assistant_completed"]
    );
}

// §3d: reasoning deltas and activity never reach the history or the journal; a
// response mixing reasoning, text and a call keeps its block order.
#[tokio::test(start_paused = true)]
async fn block_order_of_a_mixed_response_is_preserved_in_history_and_request() {
    let call = json_call("c1", "work", r#"{"n":1}"#);
    let blocks = vec![
        p1_contracts::AssistantBlock::Reasoning {
            text: "thinking".into(),
            replay: None,
        },
        text_block("before"),
        p1_contracts::AssistantBlock::ToolCall(call.clone()),
    ];
    let script = vec![
        Step::Events(vec![
            StreamEvent::Activity,
            StreamEvent::ReasoningDelta {
                block: 0,
                text: "thinking".into(),
            },
            StreamEvent::TextDelta {
                block: 1,
                text: "before".into(),
            },
            StreamEvent::Finished(completed(blocks.clone(), StopReason::ToolUse, None)),
        ]),
        text_response("done"),
    ];
    let tool = Arc::new(FakeTool::new("work"));
    let (mut agent, rig) = build(script, vec![tool.clone()], RecordingJournal::new());

    let end = turn(&mut agent, "go", CancellationToken::new()).await;

    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(tool.calls(), vec![call]);
    let second_request = &rig.provider.requests()[1];
    match &second_request.history[1] {
        Item::Assistant(item) => assert_eq!(item.blocks, blocks),
        other => panic!("expected the assistant item, got {other:?}"),
    }
    assert!(matches!(second_request.history[2], Item::ToolResult(_)));
}
