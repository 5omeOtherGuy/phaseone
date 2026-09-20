//! Implementer's tests for journal projection and resume (must-pass g, h, i) plus
//! the `ResumeError` cases. Same determinism rules as the frozen suites: paused
//! time, explicit timeouts, no sleeps, no network, no filesystem.

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    CancellationToken, Item, JournalRecord, ModelOptions, RecordBody, RouteDescription, Tool,
    ToolIdentity, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts, Projection, ResumeError, project};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, json_call, origin, text_response, tool_call_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);
const UNKNOWN_OUTCOME: &str = "Interrupted: this call was started before the session stopped and its outcome is unknown. Check the current state before retrying.";
const CANCELLED_CONTENT: &str = "Cancelled before execution.";

fn make_parts(
    provider: Arc<ScriptedProvider>,
    tools: Vec<Arc<dyn Tool>>,
    journal: Arc<RecordingJournal>,
) -> AgentParts {
    AgentParts {
        provider,
        tools,
        system_prompt: "resume test prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

async fn run(agent: &mut Agent, input: &str, cancel: CancellationToken) -> TurnEnd {
    timeout(LIMIT, agent.run_turn(input.into(), cancel))
        .await
        .expect("run_turn hung")
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
            },
            system_prompt: "prompt".into(),
            tools: Vec::new(),
            options: ModelOptions::default(),
        },
    }
}

// ------------------------------------------------- (g) crash before ToolFinished

/// Build a one-call turn that leaves `ToolStarted` (and optionally `ToolFinished`)
/// in the journal, and return the records up to and including `stop_after`'s body.
async fn run_one_tool_call() -> Vec<JournalRecord> {
    let tool = Arc::new(FakeTool::new("alpha"));
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "alpha", "{}")]),
        text_response("done"),
    ]));
    let journal = Arc::new(RecordingJournal::new());
    let parts = make_parts(provider, vec![tool.clone()], journal.clone());
    let mut agent = Agent::new(parts).expect("agent builds");
    run(&mut agent, "go", CancellationToken::new()).await;
    let records = journal.records();
    assert_eq!(
        records
            .iter()
            .filter(|record| matches!(record.body, RecordBody::ToolStarted { .. }))
            .count(),
        1
    );
    records
}

#[tokio::test(start_paused = true)]
async fn crash_after_tool_started_resumes_as_unknown() {
    let all = run_one_tool_call().await;
    let started = all
        .iter()
        .position(|record| matches!(record.body, RecordBody::ToolStarted { .. }))
        .expect("a ToolStarted was committed");
    let prefix = all[..=started].to_vec();

    // A FRESH tool instance: if resume re-executed, its call log would be non-empty.
    let resumed_tool = Arc::new(FakeTool::new("alpha"));
    let resumed_provider = Arc::new(ScriptedProvider::new(vec![text_response("after resume")]));
    let resumed_journal = Arc::new(RecordingJournal::new());
    let (mut resumed, report) = Agent::resume(
        make_parts(
            resumed_provider.clone(),
            vec![resumed_tool.clone()],
            resumed_journal.clone(),
        ),
        &prefix,
    )
    .expect("resume succeeds");

    assert_eq!(report.unresolved_calls.len(), 1);
    assert_eq!(report.unresolved_calls[0].call.call_id, "c1");
    assert!(report.unresolved_calls[0].started.is_some());
    assert!(report.changed_tools.is_empty());
    assert!(report.missing_tools.is_empty());
    assert!(!report.environment_changed);
    // The projection is the assistant call with no result yet.
    assert_eq!(resumed.history().len(), 2);

    run(&mut resumed, "next", CancellationToken::new()).await;

    let committed = resumed_journal.records();
    assert_eq!(
        committed.len(),
        3,
        "ToolFinished, UserInput, AssistantCompleted"
    );
    // The prefix is four records (it ends at ToolStarted), so the reconciliation
    // continues at seq 4.
    assert_eq!(committed[0].seq, 4);
    assert_eq!(committed[1].seq, 5);
    assert_eq!(committed[2].seq, 6);
    match &committed[0].body {
        RecordBody::ToolFinished { result } => {
            assert_eq!(result.call_id, "c1");
            assert_eq!(result.status, ToolStatus::Unknown);
            assert_eq!(result.content, UNKNOWN_OUTCOME);
        }
        other => panic!("first resumed commit was {other:?}"),
    }
    assert!(matches!(committed[1].body, RecordBody::UserInput { .. }));

    // Invariant (d): resume never re-executes. The fresh tool saw nothing.
    assert!(resumed_tool.calls().is_empty());

    // The request pairs the reconciled result before the new user item.
    let request = &resumed_provider.requests()[0];
    let result_index = request
        .history
        .iter()
        .position(|item| matches!(item, Item::ToolResult(result) if result.call_id == "c1"))
        .expect("reconciled result is in the request history");
    let user_index = request
        .history
        .iter()
        .rposition(|item| matches!(item, Item::User { .. }))
        .expect("the new user item is in the request history");
    assert!(result_index < user_index);
}

#[tokio::test(start_paused = true)]
async fn crash_after_assistant_completed_resumes_as_cancelled() {
    let all = run_one_tool_call().await;
    let completed = all
        .iter()
        .position(|record| matches!(record.body, RecordBody::AssistantCompleted { .. }))
        .expect("an AssistantCompleted was committed");
    let prefix = all[..=completed].to_vec();

    let resumed_tool = Arc::new(FakeTool::new("alpha"));
    let resumed_provider = Arc::new(ScriptedProvider::new(vec![text_response("after resume")]));
    let resumed_journal = Arc::new(RecordingJournal::new());
    let (mut resumed, report) = Agent::resume(
        make_parts(
            resumed_provider,
            vec![resumed_tool.clone()],
            resumed_journal.clone(),
        ),
        &prefix,
    )
    .expect("resume succeeds");

    assert_eq!(report.unresolved_calls.len(), 1);
    assert_eq!(report.unresolved_calls[0].call.call_id, "c1");
    assert!(report.unresolved_calls[0].started.is_none());

    run(&mut resumed, "next", CancellationToken::new()).await;

    match &resumed_journal.records()[0].body {
        RecordBody::ToolFinished { result } => {
            assert_eq!(result.status, ToolStatus::Cancelled);
            assert_eq!(result.content, CANCELLED_CONTENT);
        }
        other => panic!("first resumed commit was {other:?}"),
    }
    assert!(resumed_tool.calls().is_empty());
}

// ------------------------------------------------- (h) resume then continue

async fn run_text_session(text: &str) -> (Vec<JournalRecord>, Arc<FakeTool>) {
    let tool = Arc::new(FakeTool::new("t").with_identity("impl", "v1"));
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("hello")]));
    let journal = Arc::new(RecordingJournal::new());
    let parts = make_parts(provider, vec![tool.clone()], journal.clone());
    let mut agent = Agent::new(parts).expect("agent builds");
    run(&mut agent, text, CancellationToken::new()).await;
    (journal.records(), tool)
}

#[tokio::test(start_paused = true)]
async fn resume_then_continue_keeps_the_sequence_dense() {
    let (records, tool) = run_text_session("hi").await;
    assert_eq!(records.len(), 3);
    assert_eq!(records[0].seq, 0);
    assert_eq!(records[2].seq, 2);

    let resumed_provider = Arc::new(ScriptedProvider::new(vec![text_response("again")]));
    let resumed_journal = Arc::new(RecordingJournal::new());
    let (mut resumed, report) = Agent::resume(
        make_parts(resumed_provider, vec![tool], resumed_journal.clone()),
        &records,
    )
    .expect("resume succeeds");
    assert!(report.changed_tools.is_empty());
    assert!(!report.environment_changed);

    run(&mut resumed, "again", CancellationToken::new()).await;

    let committed = resumed_journal.records();
    assert_eq!(committed.len(), 2);
    assert_eq!(committed[0].seq, 3);
    assert_eq!(committed[1].seq, 4);
    assert!(matches!(committed[0].body, RecordBody::UserInput { .. }));
    // Nothing changed: Environment is NOT committed again.
    assert!(
        !committed
            .iter()
            .any(|record| matches!(record.body, RecordBody::Environment { .. }))
    );
    // Dense across the whole session.
    let mut seqs: Vec<u64> = records
        .iter()
        .map(|record| record.seq)
        .chain(committed.iter().map(|record| record.seq))
        .collect();
    seqs.sort_unstable();
    assert_eq!(seqs, (0..5).collect::<Vec<_>>());
}

#[tokio::test(start_paused = true)]
async fn resume_recommits_a_changed_environment_before_the_input() {
    let (records, _tool) = run_text_session("hi").await;

    let changed_tool = Arc::new(FakeTool::new("t").with_identity("impl", "v2"));
    let resumed_provider = Arc::new(ScriptedProvider::new(vec![text_response("again")]));
    let resumed_journal = Arc::new(RecordingJournal::new());
    let (mut resumed, report) = Agent::resume(
        make_parts(
            resumed_provider,
            vec![changed_tool],
            resumed_journal.clone(),
        ),
        &records,
    )
    .expect("resume succeeds");
    assert_eq!(report.changed_tools, vec!["t".to_string()]);
    assert!(report.environment_changed);

    run(&mut resumed, "again", CancellationToken::new()).await;

    let committed = resumed_journal.records();
    assert_eq!(committed.len(), 3);
    assert_eq!(committed[0].seq, 3);
    match &committed[0].body {
        RecordBody::Environment { tools, .. } => {
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0].0.name, "t");
            assert_eq!(
                tools[0].1,
                ToolIdentity {
                    implementation: "impl".into(),
                    variant: "v2".into(),
                }
            );
        }
        other => panic!("first resumed commit was {other:?}"),
    }
    // Committed BEFORE the turn's input.
    assert!(matches!(committed[1].body, RecordBody::UserInput { .. }));
}

#[tokio::test(start_paused = true)]
async fn resume_reports_tools_that_no_longer_exist() {
    let (records, _tool) = run_text_session("hi").await;

    let resumed_provider = Arc::new(ScriptedProvider::new(vec![text_response("again")]));
    let resumed_journal = Arc::new(RecordingJournal::new());
    let (_resumed, report) = Agent::resume(
        make_parts(resumed_provider, Vec::new(), resumed_journal),
        &records,
    )
    .expect("resume succeeds");
    assert_eq!(report.missing_tools, vec!["t".to_string()]);
    assert!(report.changed_tools.is_empty());
    assert!(report.environment_changed);
}

// ------------------------------------------------- (i) empty records

#[tokio::test(start_paused = true)]
async fn resume_with_no_records_behaves_like_new() {
    let fresh_journal = Arc::new(RecordingJournal::new());
    let mut fresh = Agent::new(make_parts(
        Arc::new(ScriptedProvider::new(vec![text_response("hi")])),
        Vec::new(),
        fresh_journal.clone(),
    ))
    .expect("agent builds");
    run(&mut fresh, "hello", CancellationToken::new()).await;

    let resumed_journal = Arc::new(RecordingJournal::new());
    let (mut resumed, report) = Agent::resume(
        make_parts(
            Arc::new(ScriptedProvider::new(vec![text_response("hi")])),
            Vec::new(),
            resumed_journal.clone(),
        ),
        &[],
    )
    .expect("resume from nothing succeeds");
    assert!(report.unresolved_calls.is_empty());
    assert!(report.changed_tools.is_empty());
    assert!(report.missing_tools.is_empty());
    assert!(!report.environment_changed);
    assert!(resumed.history().is_empty());

    run(&mut resumed, "hello", CancellationToken::new()).await;
    assert_eq!(fresh_journal.records(), resumed_journal.records());
    assert_eq!(fresh.history(), resumed.history());
}

// ------------------------------------------------- projection rules

#[test]
fn project_of_empty_records_is_empty() {
    assert_eq!(
        project(&[]).unwrap(),
        Projection {
            history: Vec::new(),
            next_seq: 0,
            environment_committed: false,
            unresolved_calls: Vec::new(),
            last_usage: None,
        }
    );
}

#[test]
fn project_rejects_a_missing_environment() {
    let records = vec![JournalRecord {
        seq: 0,
        body: RecordBody::UserInput {
            text: "no env".into(),
        },
    }];
    assert_eq!(
        project(&records).unwrap_err(),
        ResumeError::MissingEnvironment
    );
}

#[test]
fn project_rejects_a_sequence_gap() {
    let records = vec![
        env_record(0),
        JournalRecord {
            seq: 2,
            body: RecordBody::UserInput { text: "gap".into() },
        },
    ];
    assert_eq!(
        project(&records).unwrap_err(),
        ResumeError::Sequence {
            expected: 1,
            got: 2
        }
    );
}

#[test]
fn project_rejects_a_result_for_an_unknown_call() {
    let records = vec![
        env_record(0),
        JournalRecord {
            seq: 1,
            body: RecordBody::UserInput { text: "hi".into() },
        },
        JournalRecord {
            seq: 2,
            body: RecordBody::ToolFinished {
                result: p1_contracts::ToolResultItem {
                    call_id: "ghost".into(),
                    name: "t".into(),
                    status: ToolStatus::Ok,
                    content: "x".into(),
                },
            },
        },
    ];
    assert_eq!(
        project(&records).unwrap_err(),
        ResumeError::UnknownCall {
            call_id: "ghost".into()
        }
    );
}

#[test]
fn project_skips_non_history_records_and_applies_context_replacement() {
    use p1_contracts::{
        AssistantBlock, AssistantItem, InterruptionReason, ProviderError, ProviderErrorKind,
        StopReason,
    };

    let assistant = AssistantItem {
        origin: origin(),
        blocks: vec![AssistantBlock::Text { text: "a".into() }],
    };
    let records = vec![
        env_record(0),
        JournalRecord {
            seq: 1,
            body: RecordBody::UserInput { text: "u".into() },
        },
        JournalRecord {
            seq: 2,
            body: RecordBody::ToolStarted {
                call_id: "c".into(),
                identity: ToolIdentity {
                    implementation: "i".into(),
                    variant: "v".into(),
                },
            },
        },
        JournalRecord {
            seq: 3,
            body: RecordBody::AssistantInterrupted {
                reason: InterruptionReason::Cancelled,
                partial_text: "p".into(),
                error: Some(ProviderError::new(ProviderErrorKind::Transport, "x")),
            },
        },
        JournalRecord {
            seq: 4,
            body: RecordBody::AssistantCompleted {
                item: assistant.clone(),
                stop: StopReason::EndTurn,
                usage: None,
            },
        },
        JournalRecord {
            seq: 5,
            body: RecordBody::ContextReplaced {
                items: vec![Item::User {
                    text: "summary".into(),
                }],
                usage: None,
            },
        },
        JournalRecord {
            seq: 6,
            body: RecordBody::AssistantCompleted {
                item: assistant.clone(),
                stop: StopReason::EndTurn,
                usage: None,
            },
        },
    ];
    let projection = project(&records).unwrap();
    assert_eq!(
        projection.history,
        vec![
            Item::User {
                text: "summary".into()
            },
            Item::Assistant(assistant),
        ]
    );
    assert_eq!(projection.next_seq, 7);
    assert!(projection.environment_committed);
    assert!(projection.unresolved_calls.is_empty());
}
