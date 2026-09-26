//! ADR-0084 items 3 and 6: `Agent::reconfigure` is asynchronous. It validates the
//! whole candidate against the current history, commits the candidate's
//! `Environment` record, and only then installs every part, with no await between
//! the commit and the install. A refused candidate or a failed commit leaves the
//! current assembly answering.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_contracts::{
    BoxFuture, CancellationToken, CommitError, CommitSink, ContextError, ContextInput,
    ContextPolicy, JournalRecord, ModelOptions, Prepared, Provider, ProviderError,
    ProviderErrorKind, RecordBody, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts, BuildError, Reconfiguration, ReconfigureError};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, json_call, text_response, tool_call_response,
};
use tokio::sync::{Notify, Semaphore};

/// A context policy that counts how often it was asked and sends the history
/// unchanged, so a test can tell which policy prepared a request.
#[derive(Default)]
struct CountingContext {
    asked: AtomicUsize,
}

impl ContextPolicy for CountingContext {
    fn prepare<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Ok(None) })
    }
}

/// A journal whose commits from `gate_from` on wait for a permit. It announces on
/// `entered` that such a commit is pending, so a test can act at exactly that
/// point without timing.
struct GatedJournal {
    inner: RecordingJournal,
    gate_from: u64,
    entered: Notify,
    gate: Semaphore,
}

impl GatedJournal {
    fn new(gate_from: u64) -> Self {
        Self {
            inner: RecordingJournal::new(),
            gate_from,
            entered: Notify::new(),
            gate: Semaphore::new(0),
        }
    }
}

impl CommitSink for GatedJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        Box::pin(async move {
            if record.seq >= self.gate_from {
                self.entered.notify_one();
                let _permit = self
                    .gate
                    .acquire()
                    .await
                    .map_err(|_| CommitError("gate closed".into()))?;
            }
            self.inner.commit(record).await
        })
    }
}

fn parts(
    provider: Arc<dyn Provider>,
    journal: Arc<dyn CommitSink>,
    authorization: Arc<ScriptedAuthorization>,
    context: Arc<dyn ContextPolicy>,
) -> AgentParts {
    AgentParts {
        provider,
        tools: vec![Arc::new(FakeTool::new("read"))],
        system_prompt: "prompt".into(),
        options: ModelOptions::default(),
        context,
        authorization,
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

fn candidate(provider: Arc<dyn Provider>) -> Reconfiguration {
    Reconfiguration {
        provider,
        tools: vec![
            Arc::new(FakeTool::new("read")),
            Arc::new(FakeTool::new("write")),
        ],
        system_prompt: "second prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: None,
    }
}

fn environments(records: &[JournalRecord]) -> Vec<&JournalRecord> {
    records
        .iter()
        .filter(|record| matches!(record.body, RecordBody::Environment { .. }))
        .collect()
}

async fn turn(agent: &mut Agent, input: &str) -> TurnEnd {
    let end = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        agent.run_turn(input.into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
    end
}

fn assert_dense(records: &[JournalRecord]) {
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "{records:#?}");
    }
}

#[tokio::test]
async fn a_reconfigure_commits_the_candidate_before_it_returns_and_every_part_answers_the_next_turn()
 {
    let journal = Arc::new(RecordingJournal::new());
    let first = Arc::new(ScriptedProvider::new(vec![text_response("one")]));
    let old_authorization = Arc::new(ScriptedAuthorization::permit_all());
    let old_context = Arc::new(CountingContext::default());
    let mut agent = Agent::new(parts(
        first.clone(),
        journal.clone(),
        old_authorization.clone(),
        old_context.clone(),
    ))
    .unwrap();
    turn(&mut agent, "hi").await;
    assert_eq!(old_context.asked.load(Ordering::SeqCst), 1);

    let second = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("call_1", "write", "{}")]),
        text_response("two"),
    ]));
    let new_authorization = Arc::new(ScriptedAuthorization::denying(&["write"]));
    let new_context = Arc::new(CountingContext::default());
    agent
        .reconfigure(Reconfiguration {
            context: new_context.clone(),
            authorization: Some(new_authorization.clone()),
            ..candidate(second.clone())
        })
        .await
        .expect("the candidate is valid");

    // The record exists when the call returns, at the next dense seq, and it is
    // the candidate's.
    let records = journal.records();
    assert_eq!(records.len(), 4, "{records:#?}");
    match &records[3].body {
        RecordBody::Environment {
            system_prompt,
            tools,
            ..
        } => {
            assert_eq!(system_prompt, "second prompt");
            let names: Vec<&str> = tools.iter().map(|(tool, _)| tool.name.as_str()).collect();
            assert_eq!(names, ["read", "write"]);
        }
        other => panic!("expected the candidate's Environment at seq 3, saw {other:?}"),
    }

    turn(&mut agent, "more").await;

    let records = journal.records();
    assert_dense(&records);
    assert_eq!(
        environments(&records).len(),
        2,
        "the turn after the switch writes no second Environment"
    );
    assert!(matches!(records[4].body, RecordBody::UserInput { .. }));
    // Provider: the new one answered, the old one was not asked again.
    assert_eq!(second.requests().len(), 2);
    assert_eq!(first.requests().len(), 1);
    assert_eq!(second.requests()[0].system_prompt, "second prompt");
    assert_eq!(second.requests()[0].tools.len(), 2);
    // Context policy: the new one prepared both requests of the turn.
    assert_eq!(new_context.asked.load(Ordering::SeqCst), 2);
    assert_eq!(old_context.asked.load(Ordering::SeqCst), 1);
    // Authorization: the new policy decided the call, and denied it.
    assert_eq!(new_authorization.seen().len(), 1);
    assert!(old_authorization.seen().is_empty());
    assert!(records.iter().any(|record| matches!(
        &record.body,
        RecordBody::ToolFinished { result }
            if result.call_id == "call_1" && result.status == ToolStatus::Denied
    )));
}

#[tokio::test]
async fn a_replaced_authorization_policy_decides_the_calls_after_the_switch() {
    let journal = Arc::new(RecordingJournal::new());
    let provider = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("call_1", "read", "{}")]),
        text_response("one"),
    ]));
    let old_authorization = Arc::new(ScriptedAuthorization::permit_all());
    let mut agent = Agent::new(parts(
        provider.clone(),
        journal.clone(),
        old_authorization.clone(),
        Arc::new(PassthroughContext),
    ))
    .unwrap();
    turn(&mut agent, "hi").await;
    assert_eq!(old_authorization.seen().len(), 1);

    let next = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("call_2", "read", "{}")]),
        text_response("two"),
    ]));
    let new_authorization = Arc::new(ScriptedAuthorization::denying(&["read"]));
    agent
        .reconfigure(Reconfiguration {
            authorization: Some(new_authorization.clone()),
            ..candidate(next)
        })
        .await
        .unwrap();
    turn(&mut agent, "again").await;

    assert_eq!(old_authorization.seen().len(), 1, "the old policy is gone");
    let seen: Vec<String> = new_authorization
        .seen()
        .into_iter()
        .map(|(call_id, _)| call_id)
        .collect();
    assert_eq!(seen, ["call_2"]);
    assert!(journal.records().iter().any(|record| matches!(
        &record.body,
        RecordBody::ToolFinished { result }
            if result.call_id == "call_2" && result.status == ToolStatus::Denied
    )));
}

#[tokio::test]
async fn a_rejected_candidate_commits_nothing_and_the_old_parts_answer() {
    let journal = Arc::new(RecordingJournal::new());
    let first = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("still here"),
    ]));
    let authorization = Arc::new(ScriptedAuthorization::permit_all());
    let context = Arc::new(CountingContext::default());
    let mut agent = Agent::new(parts(
        first.clone(),
        journal.clone(),
        authorization,
        context.clone(),
    ))
    .unwrap();
    turn(&mut agent, "hi").await;

    let refusal = ProviderError::new(ProviderErrorKind::InvalidRequest, "cannot carry this");
    let second = Arc::new(ScriptedProvider::new(Vec::new()).rejecting_validation(refusal.clone()));
    let error = agent
        .reconfigure(Reconfiguration {
            context: Arc::new(CountingContext::default()),
            ..candidate(second.clone())
        })
        .await
        .expect_err("the candidate is refused");
    assert_eq!(
        error,
        ReconfigureError::Rejected(BuildError::ProviderRejected(refusal))
    );
    assert_eq!(journal.records().len(), 3, "nothing is written");

    turn(&mut agent, "more").await;
    let records = journal.records();
    assert_dense(&records);
    assert_eq!(environments(&records).len(), 1);
    assert_eq!(first.requests().len(), 2, "the old provider answered");
    assert_eq!(first.requests()[1].system_prompt, "prompt");
    assert!(second.requests().is_empty());
    assert_eq!(
        context.asked.load(Ordering::SeqCst),
        2,
        "the old policy too"
    );
}

#[tokio::test]
async fn a_failed_commit_installs_nothing_uses_no_seq_and_a_later_switch_commits_densely() {
    // seq 0..=2 are the first turn; the switch's Environment at seq 3 fails once.
    let journal = Arc::new(RecordingJournal::new().failing_once_at(3));
    let first = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("still here"),
    ]));
    let old_authorization = Arc::new(ScriptedAuthorization::permit_all());
    let old_context = Arc::new(CountingContext::default());
    let mut agent = Agent::new(parts(
        first.clone(),
        journal.clone(),
        old_authorization,
        old_context.clone(),
    ))
    .unwrap();
    turn(&mut agent, "hi").await;

    let second = Arc::new(ScriptedProvider::new(vec![text_response("never")]));
    let new_context = Arc::new(CountingContext::default());
    let error = agent
        .reconfigure(Reconfiguration {
            context: new_context.clone(),
            authorization: Some(Arc::new(ScriptedAuthorization::denying(&["read"]))),
            ..candidate(second.clone())
        })
        .await
        .expect_err("the journal refuses the record");
    assert!(
        matches!(error, ReconfigureError::CommitFailed(ref message) if message.contains("seq 3")),
        "{error:?}"
    );
    assert_eq!(journal.records().len(), 3);

    // The old assembly answers, its turn takes seq 3 (not burned), and it writes
    // no Environment: the old one is still the committed one.
    turn(&mut agent, "more").await;
    let records = journal.records();
    assert_dense(&records);
    assert_eq!(records.len(), 5);
    assert!(matches!(records[3].body, RecordBody::UserInput { .. }));
    assert_eq!(environments(&records).len(), 1);
    assert_eq!(first.requests().len(), 2);
    assert_eq!(first.requests()[1].system_prompt, "prompt");
    assert_eq!(first.requests()[1].tools.len(), 1);
    assert!(second.requests().is_empty());
    assert_eq!(old_context.asked.load(Ordering::SeqCst), 2);
    assert_eq!(new_context.asked.load(Ordering::SeqCst), 0);

    // The journal works again: the next switch commits at the next dense seq.
    let third = Arc::new(ScriptedProvider::new(vec![text_response("three")]));
    agent.reconfigure(candidate(third.clone())).await.unwrap();
    turn(&mut agent, "last").await;
    let records = journal.records();
    assert_dense(&records);
    assert!(matches!(records[5].body, RecordBody::Environment { .. }));
    assert_eq!(environments(&records).len(), 2);
    assert_eq!(third.requests().len(), 1);
}

/// The structural proof that nothing is installed while the commit is pending: the
/// reconfigure future is dropped at exactly the point its `Environment` commit
/// waits, and the agent is then observed whole and unchanged. Letting a second
/// attempt's commit complete installs the candidate. No timing is involved: the
/// journal itself announces the pending commit.
///
/// This holds for the fake below because it blocks BEFORE it writes. It is not a
/// claim about the session store: `JsonlJournal::commit` is
/// `spawn_blocking(append_blocking).await`, and tokio does not cancel a
/// `spawn_blocking` task when the awaited future is dropped, so a dropped
/// reconfigure can still leave the candidate's `Environment` durable and the store's
/// own sequence ahead of `Agent::next_seq` — the next turn's `UserInput` then takes
/// that same seq and is refused as out of order, and the journal names an assembly
/// that never answered, which ADR-0078 §4 forbids. The install guarantee holds in
/// both cases; a caller that can abort must treat the outcome as unknown and resume
/// (ADR draft, item 2.3).
#[tokio::test]
async fn nothing_is_installed_while_the_commit_is_pending() {
    let journal = Arc::new(GatedJournal::new(3));
    let first = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("still here"),
    ]));
    let mut agent = Agent::new(parts(
        first.clone(),
        journal.clone(),
        Arc::new(ScriptedAuthorization::permit_all()),
        Arc::new(PassthroughContext),
    ))
    .unwrap();
    turn(&mut agent, "hi").await;
    assert_eq!(journal.inner.records().len(), 3);

    let second = Arc::new(ScriptedProvider::new(vec![text_response("two")]));
    tokio::select! {
        biased;
        result = agent.reconfigure(candidate(second.clone())) => {
            panic!("the gated commit cannot complete: {result:?}")
        }
        // Polled only after the reconfigure future returned Pending from inside
        // the journal's commit.
        () = journal.entered.notified() => {}
    }
    assert_eq!(second.validated().len(), 1, "the candidate was validated");
    assert_eq!(
        journal.inner.records().len(),
        3,
        "and this fake, which writes only after the gate opens, committed nothing"
    );

    // Open the gate: the old assembly's turn commits at seq 3, unchanged.
    journal.gate.add_permits(1);
    turn(&mut agent, "more").await;
    let records = journal.inner.records();
    assert_dense(&records);
    assert!(matches!(records[3].body, RecordBody::UserInput { .. }));
    assert_eq!(environments(&records).len(), 1);
    assert_eq!(first.requests().len(), 2, "the old provider answered");
    assert!(second.requests().is_empty());

    // With the gate open, the same candidate commits and is installed on return.
    agent
        .reconfigure(candidate(second.clone()))
        .await
        .expect("the commit completes");
    turn(&mut agent, "after").await;
    let records = journal.inner.records();
    assert_dense(&records);
    assert!(matches!(records[5].body, RecordBody::Environment { .. }));
    assert_eq!(environments(&records).len(), 2);
    assert_eq!(second.requests().len(), 1, "the new provider answered");
}

/// Decision recorded in the ADR draft: a candidate equal in content to the current
/// environment is still an explicit change, so it commits one record.
#[tokio::test]
async fn an_unchanged_candidate_still_commits_one_environment() {
    let journal = Arc::new(RecordingJournal::new());
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_response("one"),
        text_response("two"),
    ]));
    let mut agent = Agent::new(parts(
        provider.clone(),
        journal.clone(),
        Arc::new(ScriptedAuthorization::permit_all()),
        Arc::new(PassthroughContext),
    ))
    .unwrap();
    turn(&mut agent, "hi").await;

    agent
        .reconfigure(Reconfiguration {
            provider: provider.clone(),
            tools: vec![Arc::new(FakeTool::new("read"))],
            system_prompt: "prompt".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: None,
        })
        .await
        .unwrap();
    turn(&mut agent, "more").await;

    let records = journal.records();
    assert_dense(&records);
    let environments = environments(&records);
    assert_eq!(environments.len(), 2);
    assert_eq!(environments[0].body, environments[1].body);
    assert_eq!(environments[1].seq, 3);
}

/// A switch before the first turn commits the candidate at seq 0; the construction
/// environment, never used, is never written, and the first turn writes none.
#[tokio::test]
async fn a_switch_before_the_first_turn_commits_only_the_candidate() {
    let journal = Arc::new(RecordingJournal::new());
    let first = Arc::new(ScriptedProvider::new(Vec::new()));
    let mut agent = Agent::new(parts(
        first.clone(),
        journal.clone(),
        Arc::new(ScriptedAuthorization::permit_all()),
        Arc::new(PassthroughContext),
    ))
    .unwrap();
    let second = Arc::new(ScriptedProvider::new(vec![text_response("one")]));
    agent.reconfigure(candidate(second.clone())).await.unwrap();
    turn(&mut agent, "hi").await;

    let records = journal.records();
    assert_dense(&records);
    assert_eq!(records.len(), 3);
    match &records[0].body {
        RecordBody::Environment { system_prompt, .. } => {
            assert_eq!(system_prompt, "second prompt")
        }
        other => panic!("expected the candidate's Environment first, saw {other:?}"),
    }
    assert_eq!(environments(&records).len(), 1);
    assert!(first.requests().is_empty());
}
