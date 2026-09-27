//! Manual compaction (ADR-0076, issue #203): `compact_now` makes exactly one
//! summary through the threshold path's own summarization, a short history is the
//! documented no-op, and the agent journals the very record a threshold summary
//! writes.

use std::sync::Arc;
use std::time::Duration;

use p1_context::{ContextConfig, SUMMARY_MARKER, SummarizingContext, estimate_tokens};
use p1_contracts::{
    AssistantBlock, AssistantItem, CancellationToken, Compaction, ContextInput, ContextPolicy,
    Item, JournalRecord, ModelOptions, Provider, RecordBody, StopReason, TurnEnd, serde_json,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    origin, text_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

fn user(text: &str) -> Item {
    Item::User { text: text.into() }
}

fn assistant(text: &str) -> Item {
    Item::Assistant(AssistantItem {
        origin: origin(),
        blocks: vec![AssistantBlock::Text { text: text.into() }],
    })
}

/// Four exchanges of ~100 tokens each: with an 80-token tail only the last one
/// is kept verbatim, so three units are older than the tail.
fn long_history() -> Vec<Item> {
    let mut history = vec![user("the task")];
    for round in 0..4 {
        history.push(assistant(&format!("{round}{}", "a".repeat(400))));
        history.push(user(&format!("next {round}")));
    }
    history.pop();
    history
}

/// The threshold far above the history: only a manual request summarizes.
fn config() -> ContextConfig {
    ContextConfig {
        window_tokens: 20_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: 10_000,
        keep_recent_tokens: 120,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

fn policy(provider: Arc<dyn Provider>, config: ContextConfig) -> SummarizingContext {
    SummarizingContext::new(
        provider,
        ModelOptions::default(),
        config,
        "summary prompt".into(),
    )
    .unwrap()
}

async fn compact(policy: &SummarizingContext, history: &[Item]) -> Compaction {
    timeout(
        LIMIT,
        policy.compact_now(ContextInput {
            history,
            last_usage: None,
            cancel: &CancellationToken::new(),
        }),
    )
    .await
    .expect("compact_now hung")
    .expect("compact_now succeeds")
}

#[tokio::test]
async fn compact_now_makes_exactly_one_summary_and_reports_the_token_counts() {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "## Task\nsummed",
    )]));
    let policy = policy(provider.clone(), config());
    let history = long_history();
    // Far below the threshold: the threshold path would leave it alone.
    let prepared = policy
        .prepare(ContextInput {
            history: &history,
            last_usage: None,
            cancel: &CancellationToken::new(),
        })
        .await
        .unwrap();
    assert!(prepared.is_none());
    assert!(provider.requests().is_empty());

    let Compaction::Replaced {
        prepared,
        tokens_before,
        tokens_after,
    } = compact(&policy, &history).await
    else {
        panic!("a long history is compacted");
    };
    assert_eq!(provider.requests().len(), 1, "exactly one summary request");
    let summaries = prepared
        .items
        .iter()
        .filter(|item| matches!(item, Item::User { text } if text.starts_with(SUMMARY_MARKER)))
        .count();
    assert_eq!(summaries, 1, "exactly one summary item");
    assert_eq!(
        prepared.items[0],
        user(&format!("{SUMMARY_MARKER}\n## Task\nsummed"))
    );
    assert_eq!(
        prepared.items.last(),
        history.last(),
        "the tail stays verbatim"
    );
    assert_eq!(tokens_before, estimate_tokens(&history));
    assert_eq!(tokens_after, estimate_tokens(&prepared.items));
    assert!(tokens_after < tokens_before);
}

#[tokio::test]
async fn a_history_with_nothing_older_than_the_kept_tail_is_the_documented_no_op() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let policy = policy(provider.clone(), config());
    for history in [
        vec![],
        vec![user("the task")],
        vec![user("the task"), assistant("done")],
        // A summary and the task before a tail: nothing new to summarize.
        vec![
            user(&format!("{SUMMARY_MARKER}\nearlier")),
            user("the task"),
            assistant("done"),
            user("more"),
            assistant("done again"),
        ],
    ] {
        match compact(&policy, &history).await {
            Compaction::Unchanged { tokens } => assert_eq!(tokens, estimate_tokens(&history)),
            Compaction::Replaced { .. } => panic!("{history:?} is too short to compact"),
        }
    }
    assert!(provider.requests().is_empty(), "no summary request is made");
}

fn parts(
    provider: Arc<ScriptedProvider>,
    context: Arc<dyn ContextPolicy>,
    journal: Arc<RecordingJournal>,
) -> AgentParts {
    AgentParts {
        provider,
        tools: vec![],
        system_prompt: "system".into(),
        options: ModelOptions::default(),
        context,
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

fn replaced(records: &[JournalRecord]) -> Vec<String> {
    records
        .iter()
        .filter(|record| matches!(record.body, RecordBody::ContextReplaced { .. }))
        .map(|record| serde_json::to_string(&record.body).unwrap())
        .collect()
}

#[tokio::test]
async fn the_manual_record_is_byte_for_byte_the_threshold_record() {
    // A session of four exchanges, journalled by a passthrough agent.
    let base_journal = Arc::new(RecordingJournal::new());
    let mut script = vec![];
    for round in 0..4 {
        script.push(text_response(&format!("{round}{}", "a".repeat(400))));
    }
    let base_provider = Arc::new(ScriptedProvider::new(script));
    let mut base = Agent::new(parts(
        base_provider,
        Arc::new(PassthroughContext),
        base_journal.clone(),
    ))
    .unwrap();
    for input in ["the task", "next 0", "next 1", "next 2"] {
        let end = timeout(LIMIT, base.run_turn(input.into(), CancellationToken::new()))
            .await
            .unwrap();
        assert_eq!(
            end,
            TurnEnd::Completed {
                stop: StopReason::EndTurn
            }
        );
    }
    let records = base_journal.records();
    let history_with_next = {
        let mut history = base.history().to_vec();
        history.push(user("next 3"));
        history
    };
    // The threshold sits just below that history, so the next turn summarizes.
    let config = ContextConfig {
        summarize_at_tokens: estimate_tokens(&history_with_next) - 1,
        ..config()
    };

    // Threshold: the turn's `prepare` summarizes before its request.
    let threshold_provider = Arc::new(ScriptedProvider::new(vec![
        text_response("## Task\nsummed"),
        text_response("answer"),
    ]));
    let threshold_journal = Arc::new(RecordingJournal::new());
    let (mut threshold, _) = Agent::resume(
        parts(
            threshold_provider.clone(),
            Arc::new(policy(threshold_provider.clone(), config.clone())),
            threshold_journal.clone(),
        ),
        &records,
    )
    .unwrap();
    timeout(
        LIMIT,
        threshold.run_turn("next 3".into(), CancellationToken::new()),
    )
    .await
    .unwrap();

    // Manual: the same history (the input journalled), compacted on request.
    let mut manual_records = records.clone();
    manual_records.push(JournalRecord {
        seq: records.len() as u64,
        body: RecordBody::UserInput {
            text: "next 3".into(),
        },
    });
    let manual_provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "## Task\nsummed",
    )]));
    let manual_journal = Arc::new(RecordingJournal::new());
    let (mut manual, _) = Agent::resume(
        parts(
            manual_provider.clone(),
            Arc::new(policy(manual_provider.clone(), config)),
            manual_journal.clone(),
        ),
        &manual_records,
    )
    .unwrap();
    assert_eq!(manual.history(), history_with_next.as_slice());
    let compaction = timeout(LIMIT, manual.compact_now(&CancellationToken::new()))
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(compaction, Compaction::Replaced { .. }));

    assert_eq!(
        format!("{:?}", manual_provider.requests()[0]),
        format!("{:?}", threshold_provider.requests()[0]),
        "one summary request, the same one"
    );
    let threshold_records = replaced(&threshold_journal.records());
    let manual_records = replaced(&manual_journal.records());
    assert_eq!(threshold_records.len(), 1);
    assert_eq!(manual_records, threshold_records);
    // The run continues on the summary.
    assert!(
        matches!(&manual.history()[0], Item::User { text } if text.starts_with(SUMMARY_MARKER))
    );
}
