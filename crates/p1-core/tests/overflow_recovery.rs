//! Overflow recovery: a request the provider rejects as too long is re-sent once after a
//! compaction that really shrank the history (#738). Deterministic: no sleeps, no network.

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, Compaction, ContextError, ContextInput, ContextPolicy,
    InterruptionReason, Item, ModelOptions, Prepared, ProviderError, ProviderErrorKind, RecordBody,
    StopReason, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider, Step, text_response,
};
use tokio::time::timeout;

/// Never summarizes on its own; `compact_now` answers as configured.
struct ManualOnly {
    shrink: bool,
}

impl ContextPolicy for ManualOnly {
    fn prepare<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async { Ok(None) })
    }

    fn compact_now<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        let shrink = self.shrink;
        Box::pin(async move {
            Ok(if shrink {
                Compaction::Replaced {
                    prepared: Prepared {
                        items: vec![Item::User {
                            text: "summary".into(),
                        }],
                        usage: None,
                    },
                    tokens_before: 100,
                    tokens_after: 10,
                }
            } else {
                Compaction::Unchanged { tokens: 100 }
            })
        })
    }
}

fn overflow() -> ProviderError {
    ProviderError::new(ProviderErrorKind::ContextWindowExceeded, "too long")
}

fn agent(script: Vec<Step>, shrink: bool) -> (Agent, Arc<ScriptedProvider>, Arc<RecordingJournal>) {
    let provider = Arc::new(ScriptedProvider::new(script));
    let journal = Arc::new(RecordingJournal::new());
    let agent = Agent::new(AgentParts {
        provider: provider.clone(),
        tools: Vec::new(),
        system_prompt: "p".into(),
        options: ModelOptions::default(),
        context: Arc::new(ManualOnly { shrink }),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: journal.clone(),
        events: Arc::new(RecordingEvents::new()),
    })
    .unwrap();
    (agent, provider, journal)
}

async fn run(agent: &mut Agent) -> TurnEnd {
    timeout(
        Duration::from_secs(5),
        agent.run_turn("go".into(), CancellationToken::new()),
    )
    .await
    .expect("run_turn hung")
}

fn kinds(journal: &RecordingJournal) -> Vec<&'static str> {
    journal
        .records()
        .iter()
        .filter_map(|r| match &r.body {
            RecordBody::AssistantInterrupted {
                reason: InterruptionReason::ProviderFailed,
                error: Some(e),
                ..
            } if e.kind == ProviderErrorKind::ContextWindowExceeded => Some("overflow"),
            RecordBody::ContextReplaced { .. } => Some("compaction"),
            RecordBody::AssistantCompleted { .. } => Some("completed"),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn overflow_compacts_and_resends_and_the_journal_shows_all_three_steps() {
    let (mut agent, provider, journal) = agent(
        vec![Step::SetupError(overflow()), text_response("done")],
        true,
    );
    let end = run(&mut agent).await;
    assert!(
        matches!(
            end,
            TurnEnd::Completed {
                stop: StopReason::EndTurn
            }
        ),
        "{end:?}"
    );
    assert_eq!(kinds(&journal), ["overflow", "compaction", "completed"]);
    let requests = provider.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(
        requests[1].history,
        vec![Item::User {
            text: "summary".into()
        }]
    );
}

#[tokio::test]
async fn an_unchanged_compaction_ends_the_turn_with_the_original_error() {
    let (mut agent, provider, journal) = agent(vec![Step::SetupError(overflow())], false);
    let end = run(&mut agent).await;
    assert!(
        matches!(&end, TurnEnd::ProviderFailed { error } if error.kind == ProviderErrorKind::ContextWindowExceeded),
        "{end:?}"
    );
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(kinds(&journal), ["overflow"]);
}

#[tokio::test]
async fn a_limit_of_zero_turns_recovery_off() {
    let (mut agent, provider, journal) = agent(vec![Step::SetupError(overflow())], true);
    agent.set_max_overflow_retries(0);
    let end = run(&mut agent).await;
    assert!(matches!(end, TurnEnd::ProviderFailed { .. }), "{end:?}");
    assert_eq!(provider.requests().len(), 1);
    assert_eq!(kinds(&journal), ["overflow"]);
}

#[tokio::test]
async fn only_one_retry_per_request_by_default() {
    let (mut agent, provider, journal) = agent(
        vec![Step::SetupError(overflow()), Step::SetupError(overflow())],
        true,
    );
    let end = run(&mut agent).await;
    assert!(matches!(end, TurnEnd::ProviderFailed { .. }), "{end:?}");
    assert_eq!(provider.requests().len(), 2);
    assert_eq!(kinds(&journal), ["overflow", "compaction", "overflow"]);
}
