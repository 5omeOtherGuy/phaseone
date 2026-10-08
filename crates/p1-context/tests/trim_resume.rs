//! ADR-0127: a trim of old tool results is a context replacement like a summary. It is
//! installed through the core's one replacement path, journalled as `ContextReplaced`, and a
//! session resumed from the journal continues from the trimmed history.

use std::sync::Arc;
use std::time::Duration;

use p1_context::{ContextConfig, SummarizingContext, TRIM_MARKER};
use p1_contracts::{
    CancellationToken, ContextPolicy, Item, ModelOptions, RecordBody, StopReason, ToolOutcome,
    TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_journal::{JsonlJournal, SyncPolicy};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, ScriptedAuthorization, ScriptedProvider,
    json_call, text_response, tool_call_response,
};
use tokio::time::timeout;

/// A hang guard only: every turn below ends on its own.
const LIMIT: Duration = Duration::from_secs(30);

/// The issue's example numbers: summarize at 10,000, trim from 1,000, a tail that holds
/// the newest call/result unit and the answer, results cut to 100 characters.
fn trim_config() -> ContextConfig {
    ContextConfig {
        window_tokens: 20_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: 10_000,
        keep_recent_tokens: 1_200,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 100,
        reasoning_excerpt_chars: p1_context::DEFAULT_REASONING_EXCERPT_CHARS,
        trim_at_tokens: Some(1_000),
    }
}

fn parts(
    provider: Arc<ScriptedProvider>,
    context: Arc<dyn ContextPolicy>,
    journal: Arc<JsonlJournal>,
) -> AgentParts {
    AgentParts {
        provider,
        tools: vec![Arc::new(
            FakeTool::new("read").returning(ToolOutcome::ok("x".repeat(3_000))),
        )],
        system_prompt: "agent prompt".into(),
        options: ModelOptions::default(),
        context,
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    }
}

/// The trim policy; its summary provider has no script, so a summary request would panic.
fn trimming() -> Arc<dyn ContextPolicy> {
    let summaries = Arc::new(ScriptedProvider::new(vec![]));
    Arc::new(
        SummarizingContext::new(
            summaries,
            ModelOptions::default(),
            trim_config(),
            "summary prompt".into(),
        )
        .unwrap(),
    )
}

async fn turn(agent: &mut Agent, input: &str) {
    let end = timeout(
        LIMIT,
        agent.run_turn(input.into(), CancellationToken::new()),
    )
    .await
    .expect("the turn hung");
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
}

fn result_content(item: &Item) -> Option<&str> {
    match item {
        Item::ToolResult(result) => Some(result.content.as_str()),
        _ => None,
    }
}

#[tokio::test(start_paused = true)]
async fn a_trim_is_journalled_and_a_resumed_session_continues_from_the_trimmed_history() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");

    // A session of four reads of 3,000 characters each and an answer, with no trim yet.
    let reads = Arc::new(ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "read", "{}")]),
        tool_call_response(vec![json_call("c2", "read", "{}")]),
        tool_call_response(vec![json_call("c3", "read", "{}")]),
        tool_call_response(vec![json_call("c4", "read", "{}")]),
        text_response("ok"),
    ]));
    let journal = Arc::new(JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap());
    let mut agent =
        Agent::new(parts(reads, Arc::new(PassthroughContext), journal.clone())).unwrap();
    turn(&mut agent, "task").await;
    let untrimmed = agent.history().to_vec();
    assert_eq!(untrimmed.len(), 10);
    drop(agent);
    drop(journal);

    // Resumed with the trim policy: the next turn's preparation shortens c1 to c3.
    let (journal, resumed) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    let journal = Arc::new(journal);
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("done")]));
    let (mut agent, _) = Agent::resume(
        parts(provider.clone(), trimming(), journal.clone()),
        &resumed.records,
    )
    .unwrap();
    turn(&mut agent, "next").await;
    let sent = &provider.requests()[0].history;
    for index in [2, 4, 6] {
        let content = result_content(&sent[index]).expect("a tool result");
        assert!(content.ends_with(TRIM_MARKER), "item {index}: {content}");
    }
    assert_eq!(sent[8], untrimmed[8], "the tail's result is byte-exact");
    let after_trim = agent.history().to_vec();
    drop(agent);
    drop(journal);

    // The journal holds one replacement, the trimmed history, with no summary usage.
    let (journal, resumed) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    let replacements: Vec<_> = resumed
        .records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::ContextReplaced { items, usage } => Some((items.clone(), *usage)),
            _ => None,
        })
        .collect();
    assert_eq!(replacements.len(), 1);
    let (replaced, usage) = &replacements[0];
    assert_eq!(*usage, None);
    assert_eq!(replaced.as_slice(), &sent[..]);

    // A resumed session continues from the trimmed history: its next request carries it.
    let journal = Arc::new(journal);
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("fine")]));
    let (mut agent, _) = Agent::resume(
        parts(provider.clone(), trimming(), journal.clone()),
        &resumed.records,
    )
    .unwrap();
    assert_eq!(agent.history(), after_trim.as_slice());
    turn(&mut agent, "again").await;
    let sent = &provider.requests()[0].history;
    assert_eq!(&sent[..after_trim.len()], after_trim.as_slice());
    assert_eq!(
        sent[after_trim.len()],
        Item::User {
            text: "again".into()
        }
    );
}
