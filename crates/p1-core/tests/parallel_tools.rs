//! ADR-0118: concurrency-safe tool calls of one response run together. Tests 1, 2 (memory
//! journal), 4, 5, 6, 7 and 8 of the ADR's list, and the `max_parallel = 1` bound of the
//! owner amendment (2026-10-09). Every fake tool returns only when the test releases it
//! (an explicit `Notify` gate per call), and the test reads which calls are in flight from
//! the fakes themselves: no sleeps, no network, paused time only as a hang guard.

use std::collections::{BTreeSet, HashMap};
use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, CommitError, CommitSink, Concurrency, Effect, Item,
    JournalRecord, ModelOptions, RecordBody, Tool, ToolCall, ToolContext, ToolDeclaration,
    ToolIdentity, ToolOutcome, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    Step, json_call, text_response, tool_call_response,
};
use tokio::sync::Notify;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(30);
const UNKNOWN_OUTCOME: &str = "Interrupted: this call was started before the session stopped and its outcome is unknown. Check the current state before retrying.";

// ------------------------------------------------------------------ the lab

#[derive(Default)]
struct LabState {
    /// Every start and end, in the order the fakes saw them: `+id` / `-id`.
    log: Vec<String>,
    in_flight: BTreeSet<String>,
    max_in_flight: usize,
    started: BTreeSet<String>,
    ended: BTreeSet<String>,
}

/// What every gated fake shares: the gates the test opens and what the fakes report.
#[derive(Default)]
struct Lab {
    state: Mutex<LabState>,
    changed: Notify,
    gates: Mutex<HashMap<String, Arc<Notify>>>,
}

impl Lab {
    fn gate(&self, id: &str) -> Arc<Notify> {
        self.gates
            .lock()
            .unwrap()
            .entry(id.to_owned())
            .or_default()
            .clone()
    }

    /// Let the call `id` return. A release before the call waits is kept (a stored permit).
    fn release(&self, id: &str) {
        self.gate(id).notify_one();
    }

    fn update(&self, change: impl FnOnce(&mut LabState)) {
        change(&mut self.state.lock().unwrap());
        self.changed.notify_waiters();
    }

    fn read<T>(&self, read: impl FnOnce(&LabState) -> T) -> T {
        read(&self.state.lock().unwrap())
    }

    /// Wait until `condition` holds; every state change re-checks it.
    async fn until(&self, condition: impl Fn(&LabState) -> bool) {
        loop {
            let notified = self.changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.read(&condition) {
                return;
            }
            notified.await;
        }
    }

    async fn until_started(&self, ids: &[&str]) {
        self.until(|state| ids.iter().all(|id| state.started.contains(*id)))
            .await;
    }

    async fn until_ended(&self, ids: &[&str]) {
        self.until(|state| ids.iter().all(|id| state.ended.contains(*id)))
            .await;
    }

    fn in_flight(&self) -> BTreeSet<String> {
        self.read(|state| state.in_flight.clone())
    }

    fn log(&self) -> Vec<String> {
        self.read(|state| state.log.clone())
    }

    /// Release the calls one group at a time: wait until every call of the group runs,
    /// give the scheduler further polls to start anything else, check that exactly the
    /// group is in flight, then release it in `release` order and wait for its ends.
    async fn drive(&self, groups: &[&[&str]], reverse: bool) {
        for group in groups {
            self.until_started(group).await;
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            let expected: BTreeSet<String> = group.iter().map(|id| id.to_string()).collect();
            assert_eq!(self.in_flight(), expected, "exactly this group runs");
            let mut order: Vec<&str> = group.to_vec();
            if reverse {
                order.reverse();
            }
            for id in order {
                self.release(id);
            }
            self.until_ended(group).await;
        }
    }
}

/// A tool whose every call reports its start, waits for the test to release it (never for
/// cancellation: a running call is awaited, ADR-0023) and reports whether its token fired.
struct GatedTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    concurrency: Concurrency,
    effect: Effect,
    lab: Arc<Lab>,
}

impl GatedTool {
    fn arc(lab: &Arc<Lab>, name: &str, concurrency: Concurrency, effect: Effect) -> Arc<dyn Tool> {
        Arc::new(Self {
            declaration: ToolDeclaration {
                name: name.into(),
                description: format!("gated {name}"),
                kind: p1_contracts::DeclarationKind::Function {
                    input_schema: p1_contracts::serde_json::json!({"type": "object"}),
                },
            },
            identity: ToolIdentity {
                implementation: format!("gated-{name}"),
                variant: "test".into(),
            },
            concurrency,
            effect,
            lab: lab.clone(),
        })
    }
}

impl Tool for GatedTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        self.effect
    }

    fn concurrency(&self, _call: &ToolCall) -> Concurrency {
        self.concurrency
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let id = call.call_id.clone();
            let gate = self.lab.gate(&id);
            self.lab.update(|state| {
                state.log.push(format!("+{id}"));
                state.in_flight.insert(id.clone());
                state.max_in_flight = state.max_in_flight.max(state.in_flight.len());
                state.started.insert(id.clone());
            });
            gate.notified().await;
            let saw_cancel = context.cancel.is_cancelled();
            self.lab.update(|state| {
                state.log.push(format!("-{id}"));
                state.in_flight.remove(&id);
                state.ended.insert(id.clone());
            });
            ToolOutcome::ok(format!("{id} done, saw cancel: {saw_cancel}"))
        })
    }
}

fn read_tool(lab: &Arc<Lab>) -> Arc<dyn Tool> {
    GatedTool::arc(lab, "read", Concurrency::Shared, Effect::ReadOnly)
}

fn call(id: &str, tool: &str) -> ToolCall {
    json_call(id, tool, "{}")
}

// ------------------------------------------------------------------ agents

/// Forwarding sink that declines `RequestTiming` (ADR-0121), so record sequences are exact.
#[derive(Clone)]
struct NoTimingJournal(RecordingJournal);

impl CommitSink for NoTimingJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        self.0.commit(record)
    }

    fn accepts_request_timing(&self) -> bool {
        false
    }
}

struct Run {
    agent: Agent,
    provider: Arc<ScriptedProvider>,
    journal: RecordingJournal,
    authorization: ScriptedAuthorization,
}

fn parts(
    provider: Arc<ScriptedProvider>,
    tools: Vec<Arc<dyn Tool>>,
    journal: &RecordingJournal,
    authorization: &ScriptedAuthorization,
) -> AgentParts {
    AgentParts {
        provider,
        tools,
        system_prompt: "parallel prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(authorization.clone()),
        journal: Arc::new(NoTimingJournal(journal.clone())),
        events: Arc::new(RecordingEvents::new()),
    }
}

fn agent(script: Vec<Step>, tools: Vec<Arc<dyn Tool>>, journal: RecordingJournal) -> Run {
    let provider = Arc::new(ScriptedProvider::new(script));
    let authorization = ScriptedAuthorization::permit_all();
    let agent =
        Agent::new(parts(provider.clone(), tools, &journal, &authorization)).expect("agent builds");
    Run {
        agent,
        provider,
        journal,
        authorization,
    }
}

/// Run one turn while `driver` releases the fakes; both on this task, no spawn.
async fn turn_driven(
    agent: &mut Agent,
    cancel: CancellationToken,
    driver: impl std::future::Future<Output = ()>,
) -> TurnEnd {
    let (end, ()) = timeout(LIMIT, async {
        tokio::join!(agent.run_turn("go".into(), cancel), driver)
    })
    .await
    .expect("the turn and its driver finish");
    end
}

/// The tool records after `AssistantCompleted`, as (kind, call id, status, content).
fn tool_records(journal: &RecordingJournal) -> Vec<(String, String, Option<ToolStatus>, String)> {
    journal
        .records()
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::ToolStarted { call_id, .. } => {
                Some(("started".into(), call_id.clone(), None, String::new()))
            }
            RecordBody::ToolFinished { result, .. } => Some((
                "finished".into(),
                result.call_id.clone(),
                Some(result.status),
                result.content.clone(),
            )),
            _ => None,
        })
        .collect()
}

fn kinds_and_ids(journal: &RecordingJournal) -> Vec<String> {
    tool_records(journal)
        .into_iter()
        .map(|(kind, id, _, _)| format!("{kind} {id}"))
        .collect()
}

fn assert_dense(journal: &RecordingJournal) {
    for (index, record) in journal.records().iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq is dense");
    }
}

/// The ids of the tool results in the history, in history order.
fn result_order(agent: &Agent) -> Vec<String> {
    agent
        .history()
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.call_id.clone()),
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------ tests

/// ADR-0118 test 1: `[read, read, write, read]` — both reads run before either is released,
/// the write starts only after both finished, the last read only after the write.
#[tokio::test(start_paused = true)]
async fn reads_overlap_and_a_write_separates_the_groups() {
    let lab = Arc::new(Lab::default());
    let tools = vec![
        read_tool(&lab),
        GatedTool::arc(&lab, "write", Concurrency::Exclusive, Effect::WritesFiles),
    ];
    let calls = vec![
        call("r1", "read"),
        call("r2", "read"),
        call("w", "write"),
        call("r3", "read"),
    ];
    let mut run = agent(
        vec![tool_call_response(calls), text_response("done")],
        tools,
        RecordingJournal::new(),
    );
    let end = turn_driven(
        &mut run.agent,
        CancellationToken::new(),
        lab.drive(&[&["r1", "r2"], &["w"], &["r3"]], false),
    )
    .await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    let log = lab.log();
    let at = |entry: &str| log.iter().position(|e| e == entry).unwrap();
    assert!(at("+r2") < at("-r1"), "both reads run together: {log:?}");
    assert!(at("-r1") < at("+w") && at("-r2") < at("+w"), "{log:?}");
    assert!(at("-w") < at("+r3"), "{log:?}");
    assert_eq!(result_order(&run.agent), ["r1", "r2", "w", "r3"]);
}

/// ADR-0118 test 2 (memory journal): releasing the second read first still journals
/// Started 1, Started 2, Finished 1, Finished 2, with dense `seq`; results reach the next
/// request in block order.
#[tokio::test(start_paused = true)]
async fn the_journal_keeps_block_order_whatever_order_calls_finish_in() {
    let lab = Arc::new(Lab::default());
    let mut run = agent(
        vec![
            tool_call_response(vec![call("r1", "read"), call("r2", "read")]),
            text_response("done"),
        ],
        vec![read_tool(&lab)],
        RecordingJournal::new(),
    );
    let driver = async {
        lab.until_started(&["r1", "r2"]).await;
        lab.release("r2");
        lab.until_ended(&["r2"]).await;
        assert!(!lab.read(|state| state.ended.contains("r1")));
        lab.release("r1");
    };
    let end = turn_driven(&mut run.agent, CancellationToken::new(), driver).await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(lab.log(), ["+r1", "+r2", "-r2", "-r1"]);
    assert_eq!(
        kinds_and_ids(&run.journal),
        ["started r1", "started r2", "finished r1", "finished r2"]
    );
    assert_dense(&run.journal);
    let second = &run.provider.requests()[1];
    let sent: Vec<&str> = second
        .history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(
        sent,
        ["r1", "r2"],
        "results reach the provider in block order"
    );
}

/// ADR-0118 test 4: cancel while a group runs. Every running call sees its token, is
/// awaited until released and records its own result; later groups get `Cancelled before
/// execution.`; one result per call; the turn ends `Cancelled`.
#[tokio::test(start_paused = true)]
async fn cancel_during_a_group_awaits_running_calls_and_cancels_later_groups() {
    let lab = Arc::new(Lab::default());
    let tools = vec![
        read_tool(&lab),
        GatedTool::arc(&lab, "write", Concurrency::Exclusive, Effect::WritesFiles),
    ];
    let mut run = agent(
        vec![tool_call_response(vec![
            call("r1", "read"),
            call("r2", "read"),
            call("w", "write"),
            call("r3", "read"),
        ])],
        tools,
        RecordingJournal::new(),
    );
    let cancel = CancellationToken::new();
    let driver = {
        let cancel = cancel.clone();
        let lab = lab.clone();
        async move {
            lab.until_started(&["r1", "r2"]).await;
            cancel.cancel();
            // The turn cannot end while the running calls are held.
            for _ in 0..8 {
                tokio::task::yield_now().await;
            }
            assert_eq!(lab.in_flight().len(), 2, "running calls are awaited");
            lab.release("r1");
            lab.release("r2");
        }
    };
    let end = turn_driven(&mut run.agent, cancel, driver).await;
    assert_eq!(end, TurnEnd::Cancelled);
    let records = tool_records(&run.journal);
    let finished: Vec<(String, Option<ToolStatus>, String)> = records
        .iter()
        .filter(|(kind, ..)| kind == "finished")
        .map(|(_, id, status, content)| (id.clone(), *status, content.clone()))
        .collect();
    assert_eq!(
        finished,
        [
            (
                "r1".to_string(),
                Some(ToolStatus::Ok),
                "r1 done, saw cancel: true".to_string()
            ),
            (
                "r2".into(),
                Some(ToolStatus::Ok),
                "r2 done, saw cancel: true".into()
            ),
            (
                "w".into(),
                Some(ToolStatus::Cancelled),
                "Cancelled before execution.".into()
            ),
            (
                "r3".into(),
                Some(ToolStatus::Cancelled),
                "Cancelled before execution.".into()
            ),
        ]
    );
    assert_eq!(
        kinds_and_ids(&run.journal),
        [
            "started r1",
            "started r2",
            "finished r1",
            "finished r2",
            "finished w",
            "finished r3"
        ]
    );
    assert_eq!(
        lab.log(),
        ["+r1", "+r2", "-r1", "-r2"],
        "w and r3 never ran"
    );
    assert_eq!(result_order(&run.agent), ["r1", "r2", "w", "r3"]);
    assert_dense(&run.journal);
}

/// ADR-0118 test 5: cancel before a group — no `{ToolStarted}`, every call Cancelled, and
/// authorization is never asked.
#[tokio::test(start_paused = true)]
async fn cancel_before_a_group_starts_nothing_and_asks_nothing() {
    let lab = Arc::new(Lab::default());
    let cancel = CancellationToken::new();
    let journal = {
        let cancel = cancel.clone();
        RecordingJournal::new().with_commit_hook(move |record| {
            if matches!(record.body, RecordBody::AssistantCompleted { .. }) {
                cancel.cancel();
            }
        })
    };
    let mut run = agent(
        vec![tool_call_response(vec![
            call("r1", "read"),
            call("r2", "read"),
        ])],
        vec![read_tool(&lab)],
        journal,
    );
    let end = turn_driven(&mut run.agent, cancel, async {}).await;
    assert_eq!(end, TurnEnd::Cancelled);
    assert_eq!(kinds_and_ids(&run.journal), ["finished r1", "finished r2"]);
    assert!(
        tool_records(&run.journal)
            .iter()
            .all(
                |(_, _, status, content)| *status == Some(ToolStatus::Cancelled)
                    && content == "Cancelled before execution."
            )
    );
    assert!(
        run.authorization.seen().is_empty(),
        "authorization not asked"
    );
    assert!(lab.log().is_empty(), "no call executed");
}

/// The tool records the next turn commits before its input (R5), as (id, status).
fn reconciled(journal: &RecordingJournal, after_seq: u64) -> Vec<(String, ToolStatus, String)> {
    journal
        .records()
        .iter()
        .filter(|record| record.seq >= after_seq)
        .filter_map(|record| match &record.body {
            RecordBody::ToolFinished { result, .. } => Some((
                result.call_id.clone(),
                result.status,
                result.content.clone(),
            )),
            _ => None,
        })
        .collect()
}

/// The records a resumed agent commits for the same unresolved calls at its next turn.
async fn reconciled_after_resume(
    records: &[JournalRecord],
    tools: Vec<Arc<dyn Tool>>,
) -> Vec<(String, ToolStatus, String)> {
    let journal = RecordingJournal::new();
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("again")]));
    let authorization = ScriptedAuthorization::permit_all();
    let (mut agent, _report) =
        Agent::resume(parts(provider, tools, &journal, &authorization), records)
            .expect("the records resume");
    let end = timeout(
        LIMIT,
        agent.run_turn("again".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    reconciled(&journal, 0)
}

/// ADR-0118 test 6, first half: the commit of the second `{ToolStarted}` of a group fails.
/// No call of the group executes; the next turn's R5 answers the started call `Unknown` and
/// the rest `Cancelled`, in block order; a resume from the same records does the same.
#[tokio::test(start_paused = true)]
async fn a_failed_tool_started_ends_the_turn_before_the_group_runs() {
    let lab = Arc::new(Lab::default());
    // seq 0 environment, 1 input, 2 response, 3 started r1, 4 started r2 (fails once).
    let mut run = agent(
        vec![
            tool_call_response(vec![
                call("r1", "read"),
                call("r2", "read"),
                call("r3", "read"),
            ]),
            text_response("again"),
        ],
        vec![read_tool(&lab)],
        RecordingJournal::new().failing_once_at(4),
    );
    let end = turn_driven(&mut run.agent, CancellationToken::new(), async {}).await;
    assert!(matches!(end, TurnEnd::CommitFailed { .. }), "{end:?}");
    assert!(lab.log().is_empty(), "no call of the group executed");
    let first_turn = run.journal.records();
    let end = timeout(
        LIMIT,
        run.agent.run_turn("again".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    let expected = vec![
        (
            "r1".to_string(),
            ToolStatus::Unknown,
            UNKNOWN_OUTCOME.to_string(),
        ),
        (
            "r2".into(),
            ToolStatus::Cancelled,
            "Cancelled before execution.".into(),
        ),
        (
            "r3".into(),
            ToolStatus::Cancelled,
            "Cancelled before execution.".into(),
        ),
    ];
    assert_eq!(reconciled(&run.journal, 4), expected);
    assert_dense(&run.journal);
    assert_eq!(
        reconciled_after_resume(&first_turn, vec![read_tool(&lab)]).await,
        expected,
        "resume reconciles the same records"
    );
}

/// ADR-0118 test 6, second half: the commit of the first `{ToolFinished}` of a group fails
/// after every running call returned; both started calls become `Unknown` next turn, and a
/// resume from the same records does the same.
#[tokio::test(start_paused = true)]
async fn a_failed_tool_finished_ends_the_turn_after_the_group_returned() {
    let lab = Arc::new(Lab::default());
    // seq 3 started r1, 4 started r2, 5 finished r1 (fails once).
    let mut run = agent(
        vec![
            tool_call_response(vec![call("r1", "read"), call("r2", "read")]),
            text_response("again"),
        ],
        vec![read_tool(&lab)],
        RecordingJournal::new().failing_once_at(5),
    );
    let end = turn_driven(
        &mut run.agent,
        CancellationToken::new(),
        lab.drive(&[&["r1", "r2"]], true),
    )
    .await;
    assert!(matches!(end, TurnEnd::CommitFailed { .. }), "{end:?}");
    assert_eq!(lab.log(), ["+r1", "+r2", "-r2", "-r1"], "both returned");
    let first_turn = run.journal.records();
    let end = timeout(
        LIMIT,
        run.agent.run_turn("again".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    let expected = vec![
        (
            "r1".to_string(),
            ToolStatus::Unknown,
            UNKNOWN_OUTCOME.to_string(),
        ),
        ("r2".into(), ToolStatus::Unknown, UNKNOWN_OUTCOME.into()),
    ];
    assert_eq!(reconciled(&run.journal, 5), expected);
    assert_dense(&run.journal);
    assert_eq!(
        reconciled_after_resume(&first_turn, vec![read_tool(&lab)]).await,
        expected
    );
}

/// ADR-0118 test 7: twelve Shared calls — never more than 10 in flight, the eleventh starts
/// when one returns, twelve results in block order.
#[tokio::test(start_paused = true)]
async fn at_most_ten_calls_run_at_once() {
    let lab = Arc::new(Lab::default());
    let ids: Vec<String> = (1..=12).map(|n| format!("c{n:02}")).collect();
    let calls = ids.iter().map(|id| call(id, "read")).collect();
    let mut run = agent(
        vec![tool_call_response(calls), text_response("done")],
        vec![read_tool(&lab)],
        RecordingJournal::new(),
    );
    let driver = async {
        let first_ten: Vec<&str> = ids[..10].iter().map(String::as_str).collect();
        lab.until_started(&first_ten).await;
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(lab.in_flight().len(), 10);
        assert!(!lab.read(|state| state.started.contains("c11")));
        // The slowest-looking first call is not what frees a slot: the fifth returns.
        lab.release("c05");
        lab.until_started(&["c11"]).await;
        assert!(!lab.read(|state| state.started.contains("c12")));
        for id in &ids {
            lab.release(id);
        }
    };
    let end = turn_driven(&mut run.agent, CancellationToken::new(), driver).await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(lab.read(|state| state.max_in_flight), 10);
    assert_eq!(result_order(&run.agent), ids);
    assert_dense(&run.journal);
}

/// ADR-0118 test 8: a tool that keeps the default, and Shared tools whose effect writes
/// files or delegates, never overlap anything; an unknown tool is a group of its own too.
#[tokio::test(start_paused = true)]
async fn default_writing_and_delegating_calls_run_alone() {
    let lab = Arc::new(Lab::default());
    let tools = vec![
        read_tool(&lab),
        GatedTool::arc(&lab, "plain", Concurrency::Exclusive, Effect::ReadOnly),
        GatedTool::arc(
            &lab,
            "shared_writer",
            Concurrency::Shared,
            Effect::WritesFiles,
        ),
        GatedTool::arc(
            &lab,
            "shared_worker",
            Concurrency::Shared,
            Effect::Delegates,
        ),
    ];
    let mut run = agent(
        vec![
            tool_call_response(vec![
                call("r1", "read"),
                call("p", "plain"),
                call("r2", "read"),
                call("sw", "shared_writer"),
                call("r3", "read"),
                call("sd", "shared_worker"),
                call("r4", "read"),
                call("u", "missing"),
                call("r5", "read"),
            ]),
            text_response("done"),
        ],
        tools,
        RecordingJournal::new(),
    );
    let end = turn_driven(
        &mut run.agent,
        CancellationToken::new(),
        lab.drive(
            &[
                &["r1"],
                &["p"],
                &["r2"],
                &["sw"],
                &["r3"],
                &["sd"],
                &["r4"],
                &["r5"],
            ],
            false,
        ),
    )
    .await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(lab.read(|state| state.max_in_flight), 1);
    assert_eq!(
        result_order(&run.agent),
        ["r1", "p", "r2", "sw", "r3", "sd", "r4", "u", "r5"]
    );
}

/// Owner amendment 2026-10-09: `max_parallel = 1` runs two reads one after the other, with
/// the records of sequential execution.
#[tokio::test(start_paused = true)]
async fn a_bound_of_one_never_overlaps_two_reads() {
    let lab = Arc::new(Lab::default());
    let mut run = agent(
        vec![
            tool_call_response(vec![call("r1", "read"), call("r2", "read")]),
            text_response("done"),
        ],
        vec![read_tool(&lab)],
        RecordingJournal::new(),
    );
    run.agent
        .set_max_parallel_tools(NonZeroUsize::new(1).unwrap());
    let end = turn_driven(
        &mut run.agent,
        CancellationToken::new(),
        lab.drive(&[&["r1"], &["r2"]], false),
    )
    .await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(lab.read(|state| state.max_in_flight), 1);
    assert_eq!(lab.log(), ["+r1", "-r1", "+r2", "-r2"]);
    assert_eq!(
        kinds_and_ids(&run.journal),
        ["started r1", "finished r1", "started r2", "finished r2"]
    );
}

/// #592 comment: `ends_turn` is asked per call, right after that call's own `execute`, so
/// two concurrent calls of one tool cannot mix their answers.
#[tokio::test(start_paused = true)]
async fn ends_turn_is_answered_per_call() {
    /// Ends the turn exactly for the call whose input says so; remembers the LAST outcome
    /// it saw, as a single-cell wrapper would, so a late question gets the wrong answer.
    struct LastOutcome {
        inner: Arc<dyn Tool>,
        last: Mutex<bool>,
    }
    impl Tool for LastOutcome {
        fn declaration(&self) -> &ToolDeclaration {
            self.inner.declaration()
        }
        fn identity(&self) -> &ToolIdentity {
            self.inner.identity()
        }
        fn effect(&self, call: &ToolCall) -> Effect {
            self.inner.effect(call)
        }
        fn concurrency(&self, call: &ToolCall) -> Concurrency {
            self.inner.concurrency(call)
        }
        fn ends_turn(&self, _outcome: &ToolOutcome) -> bool {
            *self.last.lock().unwrap()
        }
        fn execute<'a>(
            &'a self,
            call: &'a ToolCall,
            context: ToolContext,
        ) -> BoxFuture<'a, ToolOutcome> {
            Box::pin(async move {
                let outcome = self.inner.execute(call, context).await;
                *self.last.lock().unwrap() = call.call_id == "ender";
                outcome
            })
        }
    }
    let lab = Arc::new(Lab::default());
    let tool: Arc<dyn Tool> = Arc::new(LastOutcome {
        inner: read_tool(&lab),
        last: Mutex::new(false),
    });
    let mut run = agent(
        vec![
            tool_call_response(vec![call("ender", "read"), call("other", "read")]),
            text_response("must not be requested"),
        ],
        vec![tool],
        RecordingJournal::new(),
    );
    // `ender` returns first, `other` last: a question asked after both returned would
    // read `other`'s answer (false) for `ender`.
    let driver = async {
        lab.until_started(&["ender", "other"]).await;
        lab.release("ender");
        lab.until_ended(&["ender"]).await;
        lab.release("other");
    };
    let end = turn_driven(&mut run.agent, CancellationToken::new(), driver).await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    assert_eq!(
        run.provider.requests().len(),
        1,
        "the accepted call ended the turn"
    );
}
