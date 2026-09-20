//! Black-box acceptance tests for the p1 agent core, written from the
//! specification `docs/design/core.md` before any implementation exists.
//!
//! Every test drives `p1_core::Agent` only through its public API, using the
//! `p1_testkit` fakes and `p1_contracts` types. Time is paused; every await
//! that could hang on a wrong implementation is wrapped in
//! `tokio::time::timeout`, which fires instantly under paused time once the
//! runtime goes idle, so a hang becomes a failure instead of a stuck run.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    AgentEvent, AssistantBlock, AssistantItem, CancellationToken, ContextPolicy, Effect, Effort,
    InboxKind, InterruptionReason, Item, JournalRecord, ModelOptions, Outcome, Provider,
    ProviderError, ProviderErrorKind, RecordBody, StopReason, StreamEvent, Tool, ToolCall,
    ToolIdentity, ToolResultItem, ToolStatus, TurnEnd, Usage,
};
use p1_core::{Agent, AgentParts, BuildError, Inbox};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ReplacingContext,
    ScriptedAuthorization, ScriptedProvider, Step, completed, json_call, origin, text_block,
    text_response, tool_call_response,
};
use tokio::time::timeout;

// ---------------------------------------------------------------- helpers

const SYSTEM_PROMPT: &str = "test system prompt";
const TURN_TIMEOUT: Duration = Duration::from_secs(5);

/// Distinctive options so `Environment` records and provider requests can be
/// compared for exact equality.
fn model_options() -> ModelOptions {
    ModelOptions {
        reasoning_effort: Some(Effort::High),
        max_output_tokens: Some(64),
        cache_key: Some("test-cache".into()),
        native: [(
            "fake-route.flag".into(),
            p1_contracts::serde_json::json!({"on": true}),
        )]
        .into_iter()
        .collect(),
    }
}

/// Everything a test needs to observe: the agent plus clones of the scripted
/// fakes it was assembled from (the testkit fakes share state through `Arc`s).
struct Harness {
    agent: Agent,
    provider: ScriptedProvider,
    journal: RecordingJournal,
    events: RecordingEvents,
    authorization: ScriptedAuthorization,
    tools: Vec<FakeTool>,
    options: ModelOptions,
}

fn harness_full(
    script: Vec<Step>,
    tools: Vec<FakeTool>,
    context: Arc<dyn ContextPolicy>,
    authorization: ScriptedAuthorization,
    journal: RecordingJournal,
) -> Harness {
    let provider = ScriptedProvider::new(script);
    let events = RecordingEvents::new();
    let options = model_options();
    let parts = AgentParts {
        provider: Arc::new(provider.clone()),
        tools: tools
            .iter()
            .map(|tool| Arc::new(tool.clone()) as Arc<dyn Tool>)
            .collect(),
        system_prompt: SYSTEM_PROMPT.to_string(),
        options: options.clone(),
        context,
        authorization: Arc::new(authorization.clone()),
        journal: Arc::new(journal.clone()),
        events: Arc::new(events.clone()),
    };
    let agent = Agent::new(parts).expect("valid parts must build an agent");
    Harness {
        agent,
        provider,
        journal,
        events,
        authorization,
        tools,
        options,
    }
}

fn harness(script: Vec<Step>, tools: Vec<FakeTool>) -> Harness {
    harness_full(
        script,
        tools,
        Arc::new(PassthroughContext),
        ScriptedAuthorization::permit_all(),
        RecordingJournal::new(),
    )
}

/// Run one plain turn with a fresh (never fired) cancellation token.
async fn turn(agent: &mut Agent, input: &str) -> TurnEnd {
    timeout(
        TURN_TIMEOUT,
        agent.run_turn(input.to_string(), CancellationToken::new()),
    )
    .await
    .expect("run_turn did not finish (hung?)")
}

fn rec(seq: u64, body: RecordBody) -> JournalRecord {
    JournalRecord { seq, body }
}

/// The `Environment` record the first turn must commit, with exact contents.
fn environment_record(h: &Harness, seq: u64) -> JournalRecord {
    rec(
        seq,
        RecordBody::Environment {
            route: h.provider.describe(),
            system_prompt: SYSTEM_PROMPT.to_string(),
            tools: h
                .tools
                .iter()
                .map(|tool| (tool.declaration().clone(), tool.identity().clone()))
                .collect(),
            options: h.options.clone(),
        },
    )
}

fn user_input_record(seq: u64, text: &str) -> JournalRecord {
    rec(seq, RecordBody::UserInput { text: text.into() })
}

fn inbox_record(seq: u64, kind: InboxKind, text: &str) -> JournalRecord {
    rec(
        seq,
        RecordBody::Inbox {
            kind,
            text: text.into(),
        },
    )
}

fn assistant_record(
    seq: u64,
    item: AssistantItem,
    stop: StopReason,
    usage: Option<Usage>,
) -> JournalRecord {
    rec(seq, RecordBody::AssistantCompleted { item, stop, usage })
}

fn interrupted_record(
    seq: u64,
    reason: InterruptionReason,
    partial_text: &str,
    error: Option<ProviderError>,
) -> JournalRecord {
    rec(
        seq,
        RecordBody::AssistantInterrupted {
            reason,
            partial_text: partial_text.into(),
            error,
        },
    )
}

fn tool_started_record(seq: u64, call_id: &str, identity: ToolIdentity) -> JournalRecord {
    rec(
        seq,
        RecordBody::ToolStarted {
            call_id: call_id.into(),
            identity,
        },
    )
}

fn tool_finished_record(seq: u64, result: ToolResultItem) -> JournalRecord {
    rec(seq, RecordBody::ToolFinished { result })
}

fn user_item(text: &str) -> Item {
    Item::User { text: text.into() }
}

fn inbox_item(kind: InboxKind, text: &str) -> Item {
    Item::Inbox {
        kind,
        text: text.into(),
    }
}

fn text_item(text: &str) -> AssistantItem {
    AssistantItem {
        origin: origin(),
        blocks: vec![text_block(text)],
    }
}

fn calls_item(calls: &[ToolCall]) -> AssistantItem {
    AssistantItem {
        origin: origin(),
        blocks: calls
            .iter()
            .map(|call| AssistantBlock::ToolCall(call.clone()))
            .collect(),
    }
}

fn tool_result(call_id: &str, name: &str, status: ToolStatus, content: &str) -> ToolResultItem {
    ToolResultItem {
        call_id: call_id.into(),
        name: name.into(),
        status,
        content: content.into(),
    }
}

// ---------------------------------------------------------------- §1 construction

// §1: two tools with the same declaration name fail construction.
#[tokio::test(start_paused = true)]
async fn construction_rejects_duplicate_tool_names() {
    let provider = ScriptedProvider::new(vec![]);
    let parts = AgentParts {
        provider: Arc::new(provider),
        tools: vec![
            Arc::new(FakeTool::new("same")) as Arc<dyn Tool>,
            Arc::new(FakeTool::new("same")),
        ],
        system_prompt: SYSTEM_PROMPT.to_string(),
        options: model_options(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    };
    let error = match Agent::new(parts) {
        Ok(_) => panic!("duplicate tool names must fail construction"),
        Err(error) => error,
    };
    assert_eq!(error, BuildError::DuplicateToolName("same".into()));
}

// §1: a provider validation rejection fails construction with that error.
#[tokio::test(start_paused = true)]
async fn construction_rejects_provider_validation_failure() {
    let rejection = ProviderError::new(ProviderErrorKind::InvalidRequest, "unsupported option");
    let provider = ScriptedProvider::new(vec![]).rejecting_validation(rejection.clone());
    let parts = AgentParts {
        provider: Arc::new(provider),
        tools: vec![],
        system_prompt: SYSTEM_PROMPT.to_string(),
        options: model_options(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    };
    let error = match Agent::new(parts) {
        Ok(_) => panic!("provider rejection must fail construction"),
        Err(error) => error,
    };
    assert_eq!(error, BuildError::ProviderRejected(rejection));
}

// §1: construction alone commits nothing and emits nothing.
#[tokio::test(start_paused = true)]
async fn construction_commits_nothing_and_emits_nothing() {
    let h = harness(vec![text_response("never used")], vec![]);
    assert!(h.journal.records().is_empty());
    assert!(h.events.events().is_empty());
}

// ---------------------------------------------------------------- §2 records and sequence numbers

// §2: seq is dense from 0 across two turns, tool records included.
#[tokio::test(start_paused = true)]
async fn sequence_numbers_are_dense_across_two_turns() {
    let alpha = FakeTool::new("alpha");
    let call = json_call("c1", "alpha", "{}");
    let item0 = calls_item(std::slice::from_ref(&call));
    let result0 = tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok");
    let mut h = harness(
        vec![
            tool_call_response(vec![call.clone()]),
            text_response("done"),
            text_response("bye"),
        ],
        vec![alpha.clone()],
    );

    let end1 = turn(&mut h.agent, "go").await;
    assert_eq!(
        end1,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    let end2 = turn(&mut h.agent, "again").await;
    assert_eq!(
        end2,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let records = h.journal.records();
    assert_eq!(
        records.iter().map(|r| r.seq).collect::<Vec<_>>(),
        (0..8).collect::<Vec<_>>(),
        "seq must be dense from 0"
    );
    assert_eq!(
        records,
        vec![
            environment_record(&h, 0),
            user_input_record(1, "go"),
            assistant_record(2, item0.clone(), StopReason::ToolUse, None),
            tool_started_record(3, "c1", alpha.identity().clone()),
            tool_finished_record(4, result0.clone()),
            assistant_record(5, text_item("done"), StopReason::EndTurn, None),
            user_input_record(6, "again"),
            assistant_record(7, text_item("bye"), StopReason::EndTurn, None),
        ]
    );

    let events = h.events.events();
    assert_eq!(
        events,
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted { call: call.clone() },
            AgentEvent::ToolFinished {
                result: result0.clone()
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta { text: "bye".into() },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );

    // The provider always saw the full current history.
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 3);
    assert_eq!(requests[0].history, vec![user_item("go")]);
    assert_eq!(
        requests[1].history,
        vec![
            user_item("go"),
            Item::Assistant(item0.clone()),
            Item::ToolResult(result0.clone()),
        ]
    );
    assert_eq!(
        requests[2].history,
        vec![
            user_item("go"),
            Item::Assistant(item0.clone()),
            Item::ToolResult(result0.clone()),
            Item::Assistant(text_item("done")),
            user_item("again"),
        ]
    );
    assert_eq!(
        h.agent.history(),
        [
            user_item("go"),
            Item::Assistant(item0),
            Item::ToolResult(result0),
            Item::Assistant(text_item("done")),
            user_item("again"),
            Item::Assistant(text_item("bye")),
        ]
    );
}

// §2: Environment is seq 0, committed once, before the first UserInput, with
// the route description, prompt, tools-with-identity in order, and options.
#[tokio::test(start_paused = true)]
async fn environment_record_is_seq0_once_with_exact_contents() {
    let alpha = FakeTool::new("alpha");
    let beta = FakeTool::new("beta").with_identity("other-impl", "v2");
    let mut h = harness(
        vec![text_response("one"), text_response("two")],
        vec![alpha, beta],
    );
    turn(&mut h.agent, "first").await;
    turn(&mut h.agent, "second").await;

    let records = h.journal.records();
    assert_eq!(records.len(), 5);
    assert_eq!(records[0], environment_record(&h, 0));
    assert_eq!(
        records[1].body,
        RecordBody::UserInput {
            text: "first".into()
        },
        "Environment must be committed before the first UserInput"
    );
    assert_eq!(
        records
            .iter()
            .filter(|r| matches!(r.body, RecordBody::Environment { .. }))
            .count(),
        1,
        "Environment is committed once, not once per turn"
    );
}

// ---------------------------------------------------------------- §3 plain text turn

// §3: exact record and event sequence for one plain text turn; unknown usage
// stays None.
#[tokio::test(start_paused = true)]
async fn plain_text_turn_exact_records_and_events() {
    let mut h = harness(vec![text_response("hello")], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let item = text_item("hello");
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(2, item.clone(), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta {
                text: "hello".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi"), Item::Assistant(item)]);
    assert_eq!(h.provider.requests().len(), 1);
}

// §3e: known usage is passed through unchanged in record and event.
#[tokio::test(start_paused = true)]
async fn response_completed_passes_usage_through_unchanged() {
    let usage = Usage {
        input_uncached: Some(11),
        cache_read: Some(2),
        cache_write: Some(3),
        output: Some(7),
        reasoning_output: Some(4),
        cost_micro_usd: Some(1234),
    };
    let step = Step::Events(vec![StreamEvent::Finished(completed(
        vec![text_block("done")],
        StopReason::EndTurn,
        Some(usage),
    ))]);
    let mut h = harness(vec![step], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(2, text_item("done"), StopReason::EndTurn, Some(usage)),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: Some(usage),
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
}

// §3c: every provider request carries the system prompt, ALL tool declarations
// in the given order, the options, and the growing full history.
#[tokio::test(start_paused = true)]
async fn provider_request_carries_prompt_tools_options_and_history() {
    let alpha = FakeTool::new("alpha");
    let beta = FakeTool::new("beta");
    let mut h = harness(
        vec![text_response("one"), text_response("two")],
        vec![alpha.clone(), beta.clone()],
    );
    turn(&mut h.agent, "first").await;
    turn(&mut h.agent, "second").await;

    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    for request in &requests {
        assert_eq!(request.system_prompt, SYSTEM_PROMPT);
        assert_eq!(
            request.tools,
            vec![alpha.declaration().clone(), beta.declaration().clone()]
        );
        assert_eq!(request.options, h.options);
    }
    assert_eq!(requests[0].history, vec![user_item("first")]);
    assert_eq!(
        requests[1].history,
        vec![
            user_item("first"),
            Item::Assistant(text_item("one")),
            user_item("second"),
        ]
    );
}

// ---------------------------------------------------------------- §3c/§3d stream failures and interruptions

// §3c: a setup error commits AssistantInterrupted with empty partial text and
// fails the turn.
#[tokio::test(start_paused = true)]
async fn setup_error_interrupts_and_fails_the_turn() {
    let error = ProviderError::new(ProviderErrorKind::InvalidRequest, "unsupported option");
    let mut h = harness(vec![Step::SetupError(error.clone())], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::ProviderFailed {
            error: error.clone()
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            interrupted_record(
                2,
                InterruptionReason::ProviderFailed,
                "",
                Some(error.clone())
            ),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TurnFinished {
                end: TurnEnd::ProviderFailed { error }
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
    assert_eq!(h.provider.requests().len(), 1);
}

// §3d: Finished(Failed) commits AssistantInterrupted with the accumulated
// partial text and the error, and adds nothing to the history.
#[tokio::test(start_paused = true)]
async fn stream_failed_outcome_interrupts_with_partial_text() {
    let error = ProviderError::new(ProviderErrorKind::Transport, "boom");
    let step = Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "half".into(),
        },
        StreamEvent::Finished(Outcome::Failed(error.clone())),
    ]);
    let mut h = harness(vec![step], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::ProviderFailed {
            error: error.clone()
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            interrupted_record(
                2,
                InterruptionReason::ProviderFailed,
                "half",
                Some(error.clone())
            ),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta {
                text: "half".into()
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::ProviderFailed { error }
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
}

// §3d: a stream that ends without Finished is a Transport failure with the
// exact spec message; streamed text still counts as partial text.
#[tokio::test(start_paused = true)]
async fn stream_end_without_finished_is_transport_error() {
    let step = Step::Events(vec![StreamEvent::TextDelta {
        block: 0,
        text: "cut".into(),
    }]);
    let mut h = harness(vec![step], vec![]);

    let error = ProviderError {
        kind: ProviderErrorKind::Transport,
        message: "stream ended without a terminal event".into(),
    };
    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::ProviderFailed {
            error: error.clone()
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            interrupted_record(2, InterruptionReason::ProviderFailed, "cut", Some(error)),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta { text: "cut".into() },
            AgentEvent::TurnFinished {
                end: TurnEnd::ProviderFailed {
                    error: ProviderError {
                        kind: ProviderErrorKind::Transport,
                        message: "stream ended without a terminal event".into(),
                    }
                }
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
}

// §3d: the stream is dropped at Finished; later events are never observed.
#[tokio::test(start_paused = true)]
async fn events_after_finished_are_never_observed() {
    let step = Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "seen".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("done")],
            StopReason::EndTurn,
            None,
        )),
        StreamEvent::TextDelta {
            block: 0,
            text: "unobserved".into(),
        },
    ]);
    let mut h = harness(vec![step], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta {
                text: "seen".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
}

// §3d: ToolInputDelta is emitted as an event but never executed and never
// stored; only the complete call from Finished reaches the tool.
#[tokio::test(start_paused = true)]
async fn tool_input_delta_is_emitted_but_never_executed_or_stored() {
    let alpha = FakeTool::new("alpha");
    let full_call = json_call("c1", "alpha", "{\"x\":1}");
    let step = Step::Events(vec![
        StreamEvent::ToolInputDelta {
            call_id: "c1".into(),
            text: "{\"x\":".into(),
        },
        StreamEvent::Finished(completed(
            vec![AssistantBlock::ToolCall(full_call.clone())],
            StopReason::ToolUse,
            None,
        )),
    ]);
    let mut h = harness(vec![step, text_response("done")], vec![alpha.clone()]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ToolInputDelta {
                call_id: "c1".into(),
                text: "{\"x\":".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted {
                call: full_call.clone()
            },
            AgentEvent::ToolFinished {
                result: tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok")
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    // Executed exactly once, with the complete input from Finished.
    assert_eq!(alpha.calls(), vec![full_call.clone()]);
    // The only stored input is the complete call, never the partial delta.
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(2, calls_item(&[full_call]), StopReason::ToolUse, None),
            tool_started_record(3, "c1", alpha.identity().clone()),
            tool_finished_record(4, tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok")),
            assistant_record(5, text_item("done"), StopReason::EndTurn, None),
        ]
    );
}

// §3d: a cancelled response records its partial text, no error, and adds
// nothing to the history.
#[tokio::test(start_paused = true)]
async fn cancelled_response_records_partial_text_and_adds_no_history() {
    let step = Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "Hel".into(),
        },
        StreamEvent::TextDelta {
            block: 0,
            text: "lo".into(),
        },
        StreamEvent::Finished(Outcome::Cancelled),
    ]);
    let mut h = harness(vec![step], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(end, TurnEnd::Cancelled);

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            interrupted_record(2, InterruptionReason::Cancelled, "Hello", None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta { text: "Hel".into() },
            AgentEvent::TextDelta { text: "lo".into() },
            AgentEvent::TurnFinished {
                end: TurnEnd::Cancelled
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
}

// ---------------------------------------------------------------- §3f loop-back

// §3f: a Paused response makes the core re-request with the same history
// (now including the paused assistant item).
#[tokio::test(start_paused = true)]
async fn paused_rerequests_with_the_same_history() {
    let paused_item = text_item("so far");
    let step = Step::Events(vec![StreamEvent::Finished(completed(
        vec![text_block("so far")],
        StopReason::Paused,
        None,
    ))]);
    let mut h = harness(vec![step, text_response("done")], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(2, paused_item.clone(), StopReason::Paused, None),
            assistant_record(3, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::Paused,
                usage: None,
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].history, vec![user_item("hi")]);
    assert_eq!(
        requests[1].history,
        vec![user_item("hi"), Item::Assistant(paused_item.clone())]
    );
    assert_eq!(
        h.agent.history(),
        [
            user_item("hi"),
            Item::Assistant(paused_item),
            Item::Assistant(text_item("done"))
        ]
    );
}

// ---------------------------------------------------------------- §3b context policy

// §3b: Ok(None) leaves the history unchanged and journals nothing extra.
#[tokio::test(start_paused = true)]
async fn passthrough_context_leaves_history_unchanged() {
    let mut h = harness(vec![text_response("ok")], vec![]);

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(2, text_item("ok"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(h.provider.requests()[0].history, vec![user_item("hi")]);
}

// §3b: a replacement is journalled as ContextReplaced and becomes the history
// actually sent from then on.
#[tokio::test(start_paused = true)]
async fn context_replacement_is_journalled_and_sent() {
    let alpha = FakeTool::new("alpha");
    let call = json_call("c1", "alpha", "{}");
    let replacement = vec![user_item("rewritten")];
    // Replaces whenever the history holds more than one item.
    let context = ReplacingContext::new(1, replacement.clone());
    let mut h = harness_full(
        vec![
            tool_call_response(vec![call.clone()]),
            text_response("done"),
        ],
        vec![alpha.clone()],
        Arc::new(context),
        ScriptedAuthorization::permit_all(),
        RecordingJournal::new(),
    );

    let end = turn(&mut h.agent, "go").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "go"),
            assistant_record(
                2,
                calls_item(std::slice::from_ref(&call)),
                StopReason::ToolUse,
                None
            ),
            tool_started_record(3, "c1", alpha.identity().clone()),
            tool_finished_record(4, tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok")),
            rec(
                5,
                RecordBody::ContextReplaced {
                    items: replacement.clone(),
                    usage: None,
                }
            ),
            assistant_record(6, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    let requests = h.provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].history, vec![user_item("go")]);
    assert_eq!(requests[1].history, replacement);
    assert_eq!(
        h.agent.history(),
        [user_item("rewritten"), Item::Assistant(text_item("done"))]
    );
}

// §3b: a failing context policy ends the turn with ContextFailed and the
// error's message, before any request is made.
#[tokio::test(start_paused = true)]
async fn context_failure_ends_the_turn() {
    let mut h = harness_full(
        vec![],
        vec![],
        Arc::new(ReplacingContext::failing()),
        ScriptedAuthorization::permit_all(),
        RecordingJournal::new(),
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::ContextFailed {
            message: "scripted context failure".into()
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![environment_record(&h, 0), user_input_record(1, "hi")]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::TurnFinished {
                end: TurnEnd::ContextFailed {
                    message: "scripted context failure".into()
                }
            },
        ]
    );
    assert!(h.provider.requests().is_empty());
    assert_eq!(h.agent.history(), [user_item("hi")]);
}

// ---------------------------------------------------------------- §4 tool table

// §4: a call to a name no assembled tool has is Unavailable with the exact
// content; no ToolStarted record or event; never executed; never authorized.
#[tokio::test(start_paused = true)]
async fn unavailable_tool_gets_exact_status_and_content() {
    let alpha = FakeTool::new("alpha");
    let ghost = json_call("c9", "ghost", "{}");
    let mut h = harness(
        vec![
            tool_call_response(vec![ghost.clone()]),
            text_response("done"),
        ],
        vec![alpha.clone()],
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let result = tool_result(
        "c9",
        "ghost",
        ToolStatus::Unavailable,
        "Tool `ghost` is not available.",
    );
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(std::slice::from_ref(&ghost)),
                StopReason::ToolUse,
                None
            ),
            tool_finished_record(3, result.clone()),
            assistant_record(4, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolFinished {
                result: result.clone()
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert!(
        alpha.calls().is_empty(),
        "unavailable tools are never executed"
    );
    assert!(
        h.authorization.seen().is_empty(),
        "authorization is not asked for unavailable tools"
    );
}

// §4: a denied call records only ToolFinished with the reason as content; the
// tool is never executed and never started.
#[tokio::test(start_paused = true)]
async fn denied_tool_is_never_executed_and_reports_reason() {
    let beta = FakeTool::new("beta");
    let call = json_call("c1", "beta", "{}");
    let mut h = harness_full(
        vec![
            tool_call_response(vec![call.clone()]),
            text_response("done"),
        ],
        vec![beta.clone()],
        Arc::new(PassthroughContext),
        ScriptedAuthorization::denying(&["beta"]),
        RecordingJournal::new(),
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let result = tool_result("c1", "beta", ToolStatus::Denied, "denied by test policy");
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(std::slice::from_ref(&call)),
                StopReason::ToolUse,
                None
            ),
            tool_finished_record(3, result.clone()),
            assistant_record(4, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolFinished { result },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert!(beta.calls().is_empty(), "denied tools are never executed");
    assert_eq!(
        h.authorization.seen(),
        vec![("c1".to_string(), Effect::ReadOnly)]
    );
}

// §4: a permitted call commits ToolStarted (with the tool's identity) before
// ToolFinished, and the tool's own status and content are recorded verbatim.
#[tokio::test(start_paused = true)]
async fn permitted_tool_runs_with_started_before_finished() {
    let alpha = FakeTool::new("alpha");
    let call = json_call("c1", "alpha", "{}");
    let mut h = harness(
        vec![
            tool_call_response(vec![call.clone()]),
            text_response("done"),
        ],
        vec![alpha.clone()],
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let result = tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok");
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(std::slice::from_ref(&call)),
                StopReason::ToolUse,
                None
            ),
            tool_started_record(3, "c1", alpha.identity().clone()),
            tool_finished_record(4, result.clone()),
            assistant_record(5, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted { call: call.clone() },
            AgentEvent::ToolFinished {
                result: result.clone()
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert_eq!(alpha.calls(), vec![call]);
}

// §4: several calls run strictly sequentially in block order — each tool
// finishes before the next starts.
#[tokio::test(start_paused = true)]
async fn tool_calls_run_sequentially_in_block_order() {
    let alpha = FakeTool::new("alpha");
    let first = json_call("c1", "alpha", "{}");
    let second = json_call("c2", "alpha", "{}");
    let mut h = harness(
        vec![
            tool_call_response(vec![first.clone(), second.clone()]),
            text_response("done"),
        ],
        vec![alpha.clone()],
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let result1 = tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok");
    let result2 = tool_result("c2", "alpha", ToolStatus::Ok, "alpha ok");
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(&[first.clone(), second.clone()]),
                StopReason::ToolUse,
                None
            ),
            tool_started_record(3, "c1", alpha.identity().clone()),
            tool_finished_record(4, result1.clone()),
            tool_started_record(5, "c2", alpha.identity().clone()),
            tool_finished_record(6, result2.clone()),
            assistant_record(7, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted {
                call: first.clone()
            },
            AgentEvent::ToolFinished {
                result: result1.clone()
            },
            AgentEvent::ToolStarted {
                call: second.clone()
            },
            AgentEvent::ToolFinished {
                result: result2.clone()
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert_eq!(
        alpha.calls(),
        vec![first, second],
        "execution follows block order"
    );
}

// §4: one response mixing permitted, unavailable and denied calls follows the
// table for each, in block order, and authorization is asked only for the
// tools that exist.
#[tokio::test(start_paused = true)]
async fn mixed_calls_in_one_response_follow_the_table() {
    let alpha = FakeTool::new("alpha");
    let beta = FakeTool::new("beta");
    let call_a = json_call("c1", "alpha", "{}");
    let call_g = json_call("c2", "ghost", "{}");
    let call_b = json_call("c3", "beta", "{}");
    let mut h = harness_full(
        vec![
            tool_call_response(vec![call_a.clone(), call_g.clone(), call_b.clone()]),
            text_response("done"),
        ],
        vec![alpha.clone(), beta.clone()],
        Arc::new(PassthroughContext),
        ScriptedAuthorization::denying(&["beta"]),
        RecordingJournal::new(),
    );

    let end = turn(&mut h.agent, "go").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    let result_a = tool_result("c1", "alpha", ToolStatus::Ok, "alpha ok");
    let result_g = tool_result(
        "c2",
        "ghost",
        ToolStatus::Unavailable,
        "Tool `ghost` is not available.",
    );
    let result_b = tool_result("c3", "beta", ToolStatus::Denied, "denied by test policy");
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "go"),
            assistant_record(
                2,
                calls_item(&[call_a.clone(), call_g.clone(), call_b.clone()]),
                StopReason::ToolUse,
                None,
            ),
            tool_started_record(3, "c1", alpha.identity().clone()),
            tool_finished_record(4, result_a.clone()),
            tool_finished_record(5, result_g.clone()),
            tool_finished_record(6, result_b.clone()),
            assistant_record(7, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted {
                call: call_a.clone()
            },
            AgentEvent::ToolFinished {
                result: result_a.clone()
            },
            AgentEvent::ToolFinished {
                result: result_g.clone()
            },
            AgentEvent::ToolFinished {
                result: result_b.clone()
            },
            AgentEvent::RequestStarted { request_index: 1 },
            AgentEvent::TextDelta {
                text: "done".into()
            },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert_eq!(alpha.calls(), vec![call_a.clone()]);
    assert!(beta.calls().is_empty());
    // Authorization sees only the assembled tools, in call order.
    assert_eq!(
        h.authorization.seen(),
        vec![
            ("c1".to_string(), Effect::ReadOnly),
            ("c3".to_string(), Effect::ReadOnly),
        ]
    );
    // The next request carries the full history including all tool results.
    assert_eq!(
        h.provider.requests()[1].history,
        vec![
            user_item("go"),
            Item::Assistant(calls_item(&[call_a, call_g, call_b])),
            Item::ToolResult(result_a),
            Item::ToolResult(result_g),
            Item::ToolResult(result_b),
        ]
    );
}

// §4: authorization sees the tool's effect for the call.
#[tokio::test(start_paused = true)]
async fn authorization_sees_the_tools_effect() {
    let alpha = FakeTool::new("alpha").with_effect(Effect::Executes);
    let call = json_call("c1", "alpha", "{}");
    let mut h = harness(
        vec![
            tool_call_response(vec![call.clone()]),
            text_response("done"),
        ],
        vec![alpha.clone()],
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.authorization.seen(),
        vec![("c1".to_string(), Effect::Executes)]
    );
    assert_eq!(alpha.calls(), vec![call]);
}

// §4: a name that appears only in old history (or belongs to nobody) is never
// dispatchable.
#[tokio::test(start_paused = true)]
async fn tool_names_from_history_or_nobody_are_not_dispatchable() {
    let ghost1 = json_call("c1", "ghost", "{}");
    let ghost2 = json_call("c2", "ghost", "{}");
    let mut h = harness(
        vec![
            tool_call_response(vec![ghost1.clone()]),
            tool_call_response(vec![ghost2.clone()]),
            text_response("done"),
        ],
        vec![],
    );

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(std::slice::from_ref(&ghost1)),
                StopReason::ToolUse,
                None
            ),
            tool_finished_record(
                3,
                tool_result(
                    "c1",
                    "ghost",
                    ToolStatus::Unavailable,
                    "Tool `ghost` is not available."
                ),
            ),
            assistant_record(
                4,
                calls_item(std::slice::from_ref(&ghost2)),
                StopReason::ToolUse,
                None
            ),
            tool_finished_record(
                5,
                tool_result(
                    "c2",
                    "ghost",
                    ToolStatus::Unavailable,
                    "Tool `ghost` is not available."
                ),
            ),
            assistant_record(6, text_item("done"), StopReason::EndTurn, None),
        ]
    );
    // The second request's history contained the earlier Unavailable result;
    // the name is still not dispatchable.
    assert_eq!(
        h.provider.requests()[1].history,
        vec![
            user_item("hi"),
            Item::Assistant(calls_item(&[ghost1])),
            Item::ToolResult(tool_result(
                "c1",
                "ghost",
                ToolStatus::Unavailable,
                "Tool `ghost` is not available.",
            )),
        ]
    );
    assert!(h.authorization.seen().is_empty());
}

// ---------------------------------------------------------------- §4 + §6 cancellation

/// Spawn a helper that waits for `notify`, then fires `cancel`.
fn cancel_after_notify(
    notify: Arc<tokio::sync::Notify>,
    cancel: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        notify.notified().await;
        cancel.cancel();
    })
}

// §6: cancelling while the core waits on a stream that IGNORES cancellation
// still interrupts the response with its partial text and ends the turn.
#[tokio::test(start_paused = true)]
async fn cancel_while_stream_ignores_cancellation() {
    let step = Step::EventsThenHang(vec![StreamEvent::TextDelta {
        block: 0,
        text: "partial".into(),
    }]);
    let mut h = harness(vec![step], vec![]);

    let cancel = CancellationToken::new();
    let canceller = cancel_after_notify(h.provider.drained.clone(), cancel.clone());
    let end = timeout(TURN_TIMEOUT, h.agent.run_turn("hi".into(), cancel))
        .await
        .expect("run_turn did not finish (hung on an uncancellable stream?)");
    timeout(TURN_TIMEOUT, canceller)
        .await
        .expect("canceller did not finish")
        .unwrap();

    assert_eq!(end, TurnEnd::Cancelled);
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            interrupted_record(2, InterruptionReason::Cancelled, "partial", None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta {
                text: "partial".into()
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Cancelled
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
    assert_eq!(h.provider.requests().len(), 1);
}

// §6: cancelling a stream that reacts to cancellation (Finished(Cancelled))
// interrupts the response and ends the turn the same way.
#[tokio::test(start_paused = true)]
async fn cancel_via_awaiting_stream_ends_cancelled() {
    let step = Step::EventsThenAwaitCancel(vec![StreamEvent::TextDelta {
        block: 0,
        text: "part".into(),
    }]);
    let mut h = harness(vec![step], vec![]);

    let cancel = CancellationToken::new();
    let canceller = cancel_after_notify(h.provider.drained.clone(), cancel.clone());
    let end = timeout(TURN_TIMEOUT, h.agent.run_turn("hi".into(), cancel))
        .await
        .expect("run_turn did not finish (hung?)");
    timeout(TURN_TIMEOUT, canceller)
        .await
        .expect("canceller did not finish")
        .unwrap();

    assert_eq!(end, TurnEnd::Cancelled);
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            interrupted_record(2, InterruptionReason::Cancelled, "part", None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta {
                text: "part".into()
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Cancelled
            },
        ]
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
    assert_eq!(h.provider.requests().len(), 1);
}

// §4 + §6: cancelling while a tool runs awaits the tool (its Cancelled result
// is recorded), remaining calls get "Cancelled before execution.", the turn
// ends Cancelled, and the agent is reusable with a well-formed history.
#[tokio::test(start_paused = true)]
async fn cancel_while_tool_runs_awaits_result_and_cancels_rest() {
    let slow = FakeTool::new("slow").running_until_cancelled();
    let first = json_call("c1", "slow", "{}");
    let second = json_call("c2", "slow", "{}");
    let mut h = harness(
        vec![
            tool_call_response(vec![first.clone(), second.clone()]),
            text_response("again"),
        ],
        vec![slow.clone()],
    );

    let cancel = CancellationToken::new();
    let canceller = cancel_after_notify(slow.started.clone(), cancel.clone());
    let end = timeout(TURN_TIMEOUT, h.agent.run_turn("go".into(), cancel))
        .await
        .expect("run_turn did not finish (hung on a running tool?)");
    timeout(TURN_TIMEOUT, canceller)
        .await
        .expect("canceller did not finish")
        .unwrap();
    assert_eq!(end, TurnEnd::Cancelled);

    let result1 = tool_result("c1", "slow", ToolStatus::Cancelled, "cancelled");
    let result2 = tool_result(
        "c2",
        "slow",
        ToolStatus::Cancelled,
        "Cancelled before execution.",
    );
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "go"),
            assistant_record(
                2,
                calls_item(&[first.clone(), second.clone()]),
                StopReason::ToolUse,
                None,
            ),
            tool_started_record(3, "c1", slow.identity().clone()),
            tool_finished_record(4, result1.clone()),
            // c2 was cancelled before execution: ToolFinished only, no ToolStarted.
            tool_finished_record(5, result2.clone()),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted {
                call: first.clone()
            },
            AgentEvent::ToolFinished {
                result: result1.clone()
            },
            AgentEvent::ToolFinished {
                result: result2.clone()
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Cancelled
            },
        ]
    );
    assert_eq!(
        slow.calls(),
        vec![first.clone()],
        "only the first call executed"
    );
    // History is well-formed: no partial response, every call has a result.
    assert_eq!(
        h.agent.history(),
        [
            user_item("go"),
            Item::Assistant(calls_item(&[first.clone(), second.clone()])),
            Item::ToolResult(result1),
            Item::ToolResult(result2),
        ]
    );
    assert_eq!(
        h.provider.requests().len(),
        1,
        "no further request after cancellation"
    );

    // Reusable afterwards with a fresh token.
    let end2 = turn(&mut h.agent, "again").await;
    assert_eq!(
        end2,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(
        h.agent.history(),
        [
            user_item("go"),
            Item::Assistant(calls_item(&[first, second])),
            Item::ToolResult(tool_result(
                "c1",
                "slow",
                ToolStatus::Cancelled,
                "cancelled"
            )),
            Item::ToolResult(tool_result(
                "c2",
                "slow",
                ToolStatus::Cancelled,
                "Cancelled before execution."
            )),
            user_item("again"),
            Item::Assistant(text_item("again")),
        ]
    );
}

// ---------------------------------------------------------------- §5 inbox

// §5: messages are delivered in arrival order, exactly once, at the boundary,
// with the right kinds, an InboxDelivered{count} event, and matching records
// and history items.
#[tokio::test(start_paused = true)]
async fn inbox_delivery_order_once_kinds_and_count() {
    let mut h = harness(vec![text_response("one"), text_response("two")], vec![]);

    let inbox = h.agent.inbox();
    assert!(!h.agent.has_pending_inbox());
    assert!(inbox.send(InboxKind::Steering, "first"));
    assert!(inbox.send(InboxKind::Notification, "second"));
    assert!(h.agent.has_pending_inbox());

    let end = turn(&mut h.agent, "hi").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            inbox_record(2, InboxKind::Steering, "first"),
            inbox_record(3, InboxKind::Notification, "second"),
            assistant_record(4, text_item("one"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::InboxDelivered { count: 2 },
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta { text: "one".into() },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert_eq!(
        h.provider.requests()[0].history,
        vec![
            user_item("hi"),
            inbox_item(InboxKind::Steering, "first"),
            inbox_item(InboxKind::Notification, "second"),
        ]
    );
    assert!(!h.agent.has_pending_inbox());

    // Second turn: nothing pending, nothing re-delivered.
    let end = turn(&mut h.agent, "again").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            inbox_record(2, InboxKind::Steering, "first"),
            inbox_record(3, InboxKind::Notification, "second"),
            assistant_record(4, text_item("one"), StopReason::EndTurn, None),
            user_input_record(5, "again"),
            assistant_record(6, text_item("two"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events
            .events()
            .iter()
            .filter(|e| matches!(e, AgentEvent::InboxDelivered { .. }))
            .count(),
        1,
        "each message is delivered exactly once"
    );
    assert_eq!(
        h.agent.history(),
        [
            user_item("hi"),
            inbox_item(InboxKind::Steering, "first"),
            inbox_item(InboxKind::Notification, "second"),
            Item::Assistant(text_item("one")),
            user_item("again"),
            Item::Assistant(text_item("two")),
        ]
    );
}

// §5: a message sent while the agent is idle stays pending until the next turn.
#[tokio::test(start_paused = true)]
async fn message_sent_while_idle_waits_for_next_turn() {
    let mut h = harness(vec![text_response("one"), text_response("two")], vec![]);

    let end = turn(&mut h.agent, "first").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert!(
        h.journal
            .records()
            .iter()
            .all(|r| !matches!(r.body, RecordBody::Inbox { .. })),
        "no inbox delivery in a turn with nothing pending"
    );

    assert!(h.agent.inbox().send(InboxKind::Steering, "late"));
    assert!(h.agent.has_pending_inbox());

    let end = turn(&mut h.agent, "second").await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "first"),
            assistant_record(2, text_item("one"), StopReason::EndTurn, None),
            user_input_record(3, "second"),
            inbox_record(4, InboxKind::Steering, "late"),
            assistant_record(5, text_item("two"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.provider.requests()[1].history,
        vec![
            user_item("first"),
            Item::Assistant(text_item("one")),
            user_item("second"),
            inbox_item(InboxKind::Steering, "late"),
        ]
    );
}

// §5: run_inbox_turn on an empty inbox returns None with no event and no
// record (not even Environment).
#[tokio::test(start_paused = true)]
async fn run_inbox_turn_on_empty_inbox_is_silent() {
    let mut h = harness(vec![], vec![]);

    let result = timeout(
        TURN_TIMEOUT,
        h.agent.run_inbox_turn(CancellationToken::new()),
    )
    .await
    .expect("run_inbox_turn did not finish");
    assert_eq!(result, None);
    assert!(h.journal.records().is_empty());
    assert!(h.events.events().is_empty());
}

// §5: run_inbox_turn with a pending message runs a turn with no UserInput
// record.
#[tokio::test(start_paused = true)]
async fn run_inbox_turn_runs_without_user_input_record() {
    let mut h = harness(vec![text_response("ok")], vec![]);
    assert!(h.agent.inbox().send(InboxKind::Notification, "note"));

    let end = timeout(
        TURN_TIMEOUT,
        h.agent.run_inbox_turn(CancellationToken::new()),
    )
    .await
    .expect("run_inbox_turn did not finish");
    assert_eq!(
        end,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );

    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            inbox_record(1, InboxKind::Notification, "note"),
            assistant_record(2, text_item("ok"), StopReason::EndTurn, None),
        ]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::InboxDelivered { count: 1 },
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TextDelta { text: "ok".into() },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::EndTurn,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::Completed {
                    stop: StopReason::EndTurn
                }
            },
        ]
    );
    assert_eq!(
        h.agent.history(),
        [
            inbox_item(InboxKind::Notification, "note"),
            Item::Assistant(text_item("ok"))
        ]
    );
}

// §5: has_pending_inbox reports pending messages.
#[tokio::test(start_paused = true)]
async fn has_pending_inbox_reports_pending_messages() {
    let mut h = harness(vec![text_response("ok")], vec![]);
    assert!(!h.agent.has_pending_inbox());
    assert!(h.agent.inbox().send(InboxKind::Steering, "wake"));
    assert!(h.agent.has_pending_inbox());

    let end = timeout(
        TURN_TIMEOUT,
        h.agent.run_inbox_turn(CancellationToken::new()),
    )
    .await
    .expect("run_inbox_turn did not finish");
    assert_eq!(
        end,
        Some(TurnEnd::Completed {
            stop: StopReason::EndTurn
        })
    );
    assert!(
        !h.agent.has_pending_inbox(),
        "delivery consumes the message"
    );
}

// §5: inbox_ready does not resolve while nothing is pending, resolves as soon
// as a message is, and does not consume it.
#[tokio::test(start_paused = true)]
async fn inbox_ready_waits_for_a_message_and_does_not_consume() {
    let h = harness(vec![], vec![]);

    let empty = timeout(Duration::from_millis(500), h.agent.inbox_ready()).await;
    assert!(
        empty.is_err(),
        "inbox_ready must not resolve with an empty inbox"
    );

    assert!(h.agent.inbox().send(InboxKind::Steering, "now"));
    timeout(TURN_TIMEOUT, h.agent.inbox_ready())
        .await
        .expect("inbox_ready did not resolve once a message was pending");
    assert!(
        h.agent.has_pending_inbox(),
        "inbox_ready does not consume the message"
    );
}

// §5: send returns false once the agent has been dropped.
#[tokio::test(start_paused = true)]
async fn send_fails_after_agent_dropped() {
    let h = harness(vec![], vec![]);
    let inbox = h.agent.inbox();
    assert!(inbox.send(InboxKind::Steering, "before"));
    drop(h.agent);
    assert!(!inbox.send(InboxKind::Steering, "after"));
}

// §5/§8: an Inbox clone is usable from another spawned task.
#[tokio::test(start_paused = true)]
async fn inbox_clone_is_usable_from_spawned_task() {
    let h = harness(vec![], vec![]);
    let inbox = h.agent.inbox();
    let sender = tokio::spawn(async move { inbox.send(InboxKind::Notification, "from task") });
    let sent = timeout(TURN_TIMEOUT, sender)
        .await
        .expect("sender did not finish")
        .unwrap();
    assert!(sent);
    assert!(h.agent.has_pending_inbox());
}

// ---------------------------------------------------------------- §7 commit failure

fn commit_failure_harness(fail_at: u64) -> (Harness, FakeTool, ScriptedAuthorization) {
    let alpha = FakeTool::new("alpha");
    let authorization = ScriptedAuthorization::permit_all();
    let journal = RecordingJournal::new().failing_at(fail_at);
    let h = harness_full(
        vec![tool_call_response(vec![json_call("c1", "alpha", "{}")])],
        vec![alpha.clone()],
        Arc::new(PassthroughContext),
        authorization.clone(),
        journal,
    );
    (h, alpha, authorization)
}

async fn run_failing_turn(h: &mut Harness, fail_at: u64) -> TurnEnd {
    let end = timeout(
        TURN_TIMEOUT,
        h.agent.run_turn("hi".into(), CancellationToken::new()),
    )
    .await
    .expect("turn did not finish");
    let expected = TurnEnd::CommitFailed {
        message: format!("scripted failure at seq {fail_at}"),
    };
    assert_eq!(end, expected.clone());
    assert_eq!(
        h.events.events().last(),
        Some(&AgentEvent::TurnFinished { end: expected }),
        "TurnFinished is still the last event"
    );
    end
}

// §7: a failed Environment commit ends the turn before anything else happens.
#[tokio::test(start_paused = true)]
async fn commit_failure_at_environment() {
    let (mut h, _alpha, _auth) = commit_failure_harness(0);
    let end = run_failing_turn(&mut h, 0).await;
    assert_eq!(
        end,
        TurnEnd::CommitFailed {
            message: "scripted failure at seq 0".into()
        }
    );
    assert!(h.journal.records().is_empty());
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::TurnFinished {
                end: TurnEnd::CommitFailed {
                    message: "scripted failure at seq 0".into()
                }
            },
        ]
    );
    assert!(h.provider.requests().is_empty());
    assert!(h.agent.history().is_empty());
}

// §7: a failed UserInput commit ends the turn; nothing is pushed or requested.
#[tokio::test(start_paused = true)]
async fn commit_failure_at_user_input() {
    let (mut h, _alpha, _auth) = commit_failure_harness(1);
    run_failing_turn(&mut h, 1).await;
    assert_eq!(h.journal.records(), vec![environment_record(&h, 0)]);
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::TurnFinished {
                end: TurnEnd::CommitFailed {
                    message: "scripted failure at seq 1".into()
                }
            },
        ]
    );
    assert!(h.provider.requests().is_empty());
    assert!(h.agent.history().is_empty());
}

// §7: a failed AssistantCompleted commit ends the turn; no tool is authorized
// or executed.
#[tokio::test(start_paused = true)]
async fn commit_failure_at_assistant_completed() {
    let (mut h, alpha, auth) = commit_failure_harness(2);
    run_failing_turn(&mut h, 2).await;
    // The failed commit stores nothing: only seq 0 and 1 are in the journal.
    assert_eq!(
        h.journal.records(),
        vec![environment_record(&h, 0), user_input_record(1, "hi")]
    );
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::TurnFinished {
                end: TurnEnd::CommitFailed {
                    message: "scripted failure at seq 2".into()
                }
            },
        ]
    );
    assert!(
        auth.seen().is_empty(),
        "no tool is authorized after the failed commit"
    );
    assert!(
        alpha.calls().is_empty(),
        "no tool is executed after the failed commit"
    );
    assert_eq!(h.agent.history(), [user_item("hi")]);
}

// §7: a failed ToolStarted commit ends the turn; the tool is NOT executed.
#[tokio::test(start_paused = true)]
async fn commit_failure_at_tool_started() {
    let (mut h, alpha, _auth) = commit_failure_harness(3);
    run_failing_turn(&mut h, 3).await;
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(&[json_call("c1", "alpha", "{}")]),
                StopReason::ToolUse,
                None
            ),
        ]
    );
    assert!(
        alpha.calls().is_empty(),
        "the tool must not execute when ToolStarted fails to commit"
    );
    // R6: the failed ToolStarted record announces no event; the committed
    // AssistantCompleted does.
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::CommitFailed {
                    message: "scripted failure at seq 3".into()
                }
            },
        ]
    );
    assert!(
        h.provider.requests().len() == 1,
        "no further provider request"
    );
    // AssistantCompleted (seq 2) committed before ToolStarted failed, so the
    // assistant item IS in the history.
    assert_eq!(
        h.agent.history(),
        [
            user_item("hi"),
            Item::Assistant(calls_item(&[json_call("c1", "alpha", "{}")]))
        ]
    );
}

// §7: a failed ToolFinished commit ends the turn; the tool DID run, but no
// further provider request is made.
#[tokio::test(start_paused = true)]
async fn commit_failure_at_tool_finished() {
    let (mut h, alpha, _auth) = commit_failure_harness(4);
    run_failing_turn(&mut h, 4).await;
    let call = json_call("c1", "alpha", "{}");
    assert_eq!(
        h.journal.records(),
        vec![
            environment_record(&h, 0),
            user_input_record(1, "hi"),
            assistant_record(
                2,
                calls_item(std::slice::from_ref(&call)),
                StopReason::ToolUse,
                None
            ),
            tool_started_record(3, "c1", alpha.identity().clone()),
        ]
    );
    assert_eq!(
        alpha.calls(),
        vec![call],
        "the tool ran (ToolStarted committed)"
    );
    // R6: ToolFinished's commit failed, so no ToolFinished event; the tool's
    // result push is skipped — history stays [User, Assistant].
    assert_eq!(
        h.events.events(),
        vec![
            AgentEvent::TurnStarted,
            AgentEvent::RequestStarted { request_index: 0 },
            AgentEvent::ResponseCompleted {
                model: "fake-model".into(),
                stop: StopReason::ToolUse,
                usage: None,
            },
            AgentEvent::ToolStarted {
                call: json_call("c1", "alpha", "{}")
            },
            AgentEvent::TurnFinished {
                end: TurnEnd::CommitFailed {
                    message: "scripted failure at seq 4".into()
                }
            },
        ]
    );
    assert_eq!(h.provider.requests().len(), 1);
    assert_eq!(
        h.agent.history(),
        [
            user_item("hi"),
            Item::Assistant(calls_item(&[json_call("c1", "alpha", "{}")]))
        ]
    );
}

// ---------------------------------------------------------------- §8 threading

// §8: compile-time assertions — Agent is Send, run_turn/run_inbox_turn/
// inbox_ready futures are Send, Inbox is Send + Sync + Clone. Nothing runs.
#[test]
fn threading_compile_time_assertions() {
    fn is_send<T: Send>(_: Option<T>) {}
    fn is_sync<T: Sync>(_: Option<T>) {}
    fn is_clone<T: Clone>(_: Option<T>) {}
    is_send::<Agent>(None);
    is_sync::<Inbox>(None);
    is_clone::<Inbox>(None);

    fn future_is_send<F: Future + Send>(_: Option<F>) {}
    fn probes(agent: &mut Agent) {
        future_is_send(Some(
            agent.run_turn(String::new(), CancellationToken::new()),
        ));
        future_is_send(Some(agent.run_inbox_turn(CancellationToken::new())));
        future_is_send(Some(agent.inbox_ready()));
    }
    let _ = probes;
}
