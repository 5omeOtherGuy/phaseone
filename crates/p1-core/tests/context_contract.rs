//! Implementer's tests for the context-control contract (context.md §1).
//!
//! Determinism rules of the frozen suites: paused time, explicit 5 s timeouts, no
//! sleeps, no network, no real user directories.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{
    AgentEvent, AssistantBlock, AssistantItem, BoxFuture, CancellationToken, ContextError,
    ContextInput, ContextPolicy, InterruptionReason, Item, JournalRecord, ModelOptions, Prepared,
    RecordBody, RouteDescription, StopReason, StreamEvent, ToolResultItem, ToolStatus, TurnEnd,
    Usage,
};
use p1_core::{Agent, AgentParts, project};
use p1_testkit::{
    GatedContext, RecordingEvents, RecordingJournal, ReplacingContext, ScriptedAuthorization,
    ScriptedProvider, Step, completed, origin, text_block, text_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);
const PROMPT: &str = "context contract prompt";

fn some_usage() -> Usage {
    Usage {
        input_uncached: Some(100),
        cache_read: Some(20),
        cache_write: Some(5),
        output: Some(7),
        reasoning_output: None,
        cost_micro_usd: Some(1_234),
    }
}

fn parts(
    provider: Arc<ScriptedProvider>,
    context: Arc<dyn ContextPolicy>,
    journal: Arc<RecordingJournal>,
    events: Arc<RecordingEvents>,
) -> AgentParts {
    AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: PROMPT.into(),
        options: ModelOptions::default(),
        context,
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events,
    }
}

fn env_record(seq: u64) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::Environment {
            route: RouteDescription {
                origin: origin(),
                supports_freeform_tools: true,
                mandatory_prompt_prefix: None,
                reports_cost: false,
                cache_key: p1_contracts::CacheKeySupport::Unsupported,
            },
            system_prompt: PROMPT.into(),
            tools: Vec::new(),
            options: ModelOptions::default(),
        },
    }
}

async fn run(agent: &mut Agent, input: &str, cancel: CancellationToken) -> TurnEnd {
    timeout(LIMIT, agent.run_turn(input.into(), cancel))
        .await
        .expect("run_turn hung")
}

/// One text response whose terminal event reports `usage`.
fn response_with_usage(text: &str, usage: Option<Usage>) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block(text)],
            StopReason::EndTurn,
            usage,
        )),
    ])
}

/// A policy that records the `last_usage` it was shown on every call.
#[derive(Clone, Default)]
struct UsageSpy {
    seen: Arc<Mutex<Vec<Option<Usage>>>>,
}

impl ContextPolicy for UsageSpy {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(input.last_usage.copied());
            Ok(None)
        })
    }
}

/// A policy that reports cancellation itself, without the turn's token firing.
struct CancellingContext;

impl ContextPolicy for CancellingContext {
    fn prepare<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async { Err(ContextError::Cancelled) })
    }
}

fn assistant_with_call(call: p1_contracts::ToolCall) -> Item {
    Item::Assistant(AssistantItem {
        origin: origin(),
        blocks: vec![AssistantBlock::ToolCall(call)],
    })
}

fn tool_result(call_id: &str, name: &str) -> Item {
    Item::ToolResult(ToolResultItem {
        call_id: call_id.into(),
        name: name.into(),
        status: ToolStatus::Ok,
        content: "ok".into(),
    })
}

fn unpaired(call_id: &str) -> String {
    format!("context policy returned an unpaired tool call or result: {call_id}")
}

fn has_context_record(journal: &RecordingJournal) -> bool {
    journal
        .records()
        .iter()
        .any(|record| matches!(record.body, RecordBody::ContextReplaced { .. }))
}

// --------------------------------------------------------------- (a) last_usage

#[tokio::test(start_paused = true)]
async fn prepare_sees_last_usage_of_the_previous_response() {
    let usage = some_usage();
    let spy = Arc::new(UsageSpy::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        response_with_usage("one", Some(usage)),
        text_response("two"),
        text_response("three"),
    ]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let parts = parts(provider, spy.clone(), journal, events);
    let mut agent = Agent::new(parts).unwrap();

    for input in ["one", "two", "three"] {
        run(&mut agent, input, CancellationToken::new()).await;
    }

    // Before any response: none. After a response with usage: that usage. After a
    // response without usage: none again.
    assert_eq!(*spy.seen.lock().unwrap(), vec![None, Some(usage), None]);
}

// --------------------------------------------- (b) commit, then the event

#[tokio::test(start_paused = true)]
async fn replacement_is_committed_then_announced_with_usage() {
    let usage = some_usage();
    let replacement = vec![Item::User {
        text: "summary".into(),
    }];
    let context = Arc::new(ReplacingContext::new(0, replacement.clone()).with_usage(usage));
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let events = Arc::new(RecordingEvents::new());
    // Snapshot the events seen from INSIDE the `ContextReplaced` commit: the
    // announcing event must not exist yet (R6).
    let at_commit = Arc::new(Mutex::new(Vec::<AgentEvent>::new()));
    let hook_events = events.clone();
    let hook_snapshot = at_commit.clone();
    let journal = Arc::new(RecordingJournal::new().with_commit_hook(move |record| {
        if matches!(record.body, RecordBody::ContextReplaced { .. }) {
            *hook_snapshot.lock().unwrap() = hook_events.events();
        }
    }));
    let mut agent = Agent::new(parts(
        provider.clone(),
        context,
        journal.clone(),
        events.clone(),
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let records = journal.records();
    assert_eq!(
        records[2],
        JournalRecord {
            seq: 2,
            body: RecordBody::ContextReplaced {
                items: replacement.clone(),
                usage: Some(usage),
            },
        }
    );
    assert!(
        !at_commit
            .lock()
            .unwrap()
            .iter()
            .any(|event| matches!(event, AgentEvent::ContextReplaced { .. })),
        "the ContextReplaced event must not exist while its record is committing"
    );
    let announced = events
        .events()
        .into_iter()
        .find(|event| matches!(event, AgentEvent::ContextReplaced { .. }))
        .expect("ContextReplaced event emitted after the commit");
    assert_eq!(
        announced,
        AgentEvent::ContextReplaced {
            items_before: 1,
            items_after: 1,
            usage: Some(usage),
        }
    );
    assert_eq!(provider.requests()[0].history, replacement);
}

#[tokio::test(start_paused = true)]
async fn a_failed_replacement_commit_emits_no_event() {
    let context = Arc::new(ReplacingContext::new(
        0,
        vec![Item::User {
            text: "summary".into(),
        }],
    ));
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    // Environment (0), UserInput (1), ContextReplaced (2): fail the third.
    let journal = Arc::new(RecordingJournal::new().failing_at(2));
    let events = Arc::new(RecordingEvents::new());
    let mut agent = Agent::new(parts(
        provider.clone(),
        context,
        journal.clone(),
        events.clone(),
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::CommitFailed {
            message: "scripted failure at seq 2".into()
        }
    );
    assert_eq!(
        journal.records().len(),
        2,
        "nothing after the failure commits"
    );
    assert!(!has_context_record(&journal));
    assert!(
        !events
            .events()
            .iter()
            .any(|event| matches!(event, AgentEvent::ContextReplaced { .. }))
    );
    assert!(provider.requests().is_empty());
}

// ------------------------------------------- (c) cancellation while preparing

#[tokio::test(start_paused = true)]
async fn cancelling_a_pending_prepare_ends_the_turn_cancelled() {
    let context = GatedContext::new(vec![Item::User {
        text: "summary".into(),
    }]);
    let started = context.started.clone();
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let parts = parts(
        provider.clone(),
        Arc::new(context),
        journal.clone(),
        events.clone(),
    );
    let mut agent = Agent::new(parts).unwrap();

    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let task = tokio::spawn(async move { agent.run_turn("q".into(), cancel).await });
    timeout(LIMIT, started.notified())
        .await
        .expect("prepare never started");
    trigger.cancel();
    let end = timeout(LIMIT, task)
        .await
        .expect("run_turn hung after cancel")
        .expect("task joined");

    assert_eq!(end, TurnEnd::Cancelled);
    assert!(
        provider.requests().is_empty(),
        "no provider request was made"
    );
    let records = journal.records();
    assert_eq!(
        records.last().unwrap().body,
        RecordBody::AssistantInterrupted {
            reason: InterruptionReason::Cancelled,
            partial_text: String::new(),
            error: None,
        }
    );
    assert!(!has_context_record(&journal));
    assert!(
        !events
            .events()
            .iter()
            .any(|event| matches!(event, AgentEvent::ContextReplaced { .. }))
    );
    assert_eq!(
        events.events().last().cloned(),
        Some(AgentEvent::TurnFinished {
            end: TurnEnd::Cancelled
        })
    );
}

#[tokio::test(start_paused = true)]
async fn a_policy_reporting_cancellation_ends_the_turn_cancelled() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let mut agent = Agent::new(parts(
        provider.clone(),
        Arc::new(CancellingContext),
        journal.clone(),
        events.clone(),
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(end, TurnEnd::Cancelled);
    assert!(provider.requests().is_empty());
    let records = journal.records();
    assert_eq!(
        records.last().unwrap().body,
        RecordBody::AssistantInterrupted {
            reason: InterruptionReason::Cancelled,
            partial_text: String::new(),
            error: None,
        }
    );
    assert!(!has_context_record(&journal));
}

// ------------------------------------------------------ (d) ordinary failure

#[tokio::test(start_paused = true)]
async fn a_failed_prepare_ends_the_turn_with_the_exact_message() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let mut agent = Agent::new(parts(
        provider.clone(),
        Arc::new(ReplacingContext::failing()),
        journal,
        events,
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::ContextFailed {
            message: "scripted context failure".into()
        }
    );
    assert!(provider.requests().is_empty());
}

// ------------------------------------------------- (e) replacement validation

/// Assert the exact rejection: the spec's message naming `call_id`, nothing
/// committed past the input, and no provider request.
fn assert_rejected(
    end: TurnEnd,
    journal: &RecordingJournal,
    provider: &ScriptedProvider,
    call_id: &str,
) {
    assert_eq!(
        end,
        TurnEnd::ContextFailed {
            message: unpaired(call_id)
        }
    );
    assert_eq!(journal.records().len(), 2, "nothing was committed");
    assert!(!has_context_record(journal));
    assert!(provider.requests().is_empty());
}

#[tokio::test(start_paused = true)]
async fn a_result_for_an_unknown_call_is_rejected() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let replacement = vec![tool_result("c-ghost", "x")];
    let mut agent = Agent::new(parts(
        provider.clone(),
        Arc::new(ReplacingContext::new(0, replacement)),
        journal.clone(),
        events,
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_rejected(end, &journal, &provider, "c-ghost");
}

#[tokio::test(start_paused = true)]
async fn a_non_final_call_without_a_result_is_rejected() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let replacement = vec![
        assistant_with_call(p1_testkit::json_call("c1", "missing", "{}")),
        Item::User {
            text: "later".into(),
        },
    ];
    let mut agent = Agent::new(parts(
        provider.clone(),
        Arc::new(ReplacingContext::new(0, replacement)),
        journal.clone(),
        events,
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_rejected(end, &journal, &provider, "c1");
}

/// Run a turn whose replacement is `replacement` and assert it is accepted,
/// committed, and sent to the provider.
async fn assert_accepted(replacement: Vec<Item>) {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let mut agent = Agent::new(parts(
        provider.clone(),
        Arc::new(ReplacingContext::new(0, replacement.clone())),
        journal.clone(),
        events,
    ))
    .unwrap();

    let end = run(&mut agent, "q", CancellationToken::new()).await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert!(has_context_record(&journal));
    assert_eq!(provider.requests()[0].history, replacement);
}

#[tokio::test(start_paused = true)]
async fn a_final_call_without_a_result_is_accepted() {
    // The last item may keep unresolved calls: the request loop answers them next.
    assert_accepted(vec![
        Item::User {
            text: "task".into(),
        },
        assistant_with_call(p1_testkit::json_call("c1", "missing", "{}")),
    ])
    .await;
}

#[tokio::test(start_paused = true)]
async fn a_well_paired_history_is_accepted() {
    assert_accepted(vec![
        Item::User {
            text: "task".into(),
        },
        assistant_with_call(p1_testkit::json_call("c1", "missing", "{}")),
        tool_result("c1", "missing"),
    ])
    .await;
}

#[tokio::test(start_paused = true)]
async fn an_empty_replacement_is_accepted() {
    assert_accepted(Vec::new()).await;
}

// -------------------------------------------------------------- (f) resume

#[test]
fn project_returns_the_last_assistant_usage() {
    let usage = some_usage();
    let assistant = AssistantItem {
        origin: origin(),
        blocks: vec![text_block("x")],
    };
    let completed = |seq: u64, usage: Option<Usage>| JournalRecord {
        seq,
        body: RecordBody::AssistantCompleted {
            item: assistant.clone(),
            stop: StopReason::EndTurn,
            usage,
        },
    };

    let with_usage_last = vec![
        env_record(0),
        JournalRecord {
            seq: 1,
            body: RecordBody::UserInput { text: "q".into() },
        },
        completed(2, None),
        completed(3, Some(usage)),
    ];
    assert_eq!(project(&with_usage_last).unwrap().last_usage, Some(usage));

    let without_usage_last = vec![
        env_record(0),
        JournalRecord {
            seq: 1,
            body: RecordBody::UserInput { text: "q".into() },
        },
        completed(2, Some(usage)),
        completed(3, None),
    ];
    assert_eq!(project(&without_usage_last).unwrap().last_usage, None);
}

#[tokio::test(start_paused = true)]
async fn resume_restores_last_usage_for_the_first_prepare() {
    let usage = some_usage();
    let spy = Arc::new(UsageSpy::default());
    let provider = Arc::new(ScriptedProvider::new(vec![
        response_with_usage("one", Some(usage)),
        text_response("two"),
    ]));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let first_parts = parts(provider.clone(), spy.clone(), journal.clone(), events);
    let mut agent = Agent::new(first_parts).unwrap();
    run(&mut agent, "one", CancellationToken::new()).await;

    let records = journal.records();
    assert!(matches!(
        records.last().unwrap().body,
        RecordBody::AssistantCompleted { .. }
    ));
    let resumed_parts = parts(
        provider,
        spy.clone(),
        journal,
        Arc::new(RecordingEvents::new()),
    );
    let (mut resumed, _report) = Agent::resume(resumed_parts, &records).expect("resume");
    run(&mut resumed, "two", CancellationToken::new()).await;

    assert_eq!(*spy.seen.lock().unwrap(), vec![None, Some(usage)]);
}
