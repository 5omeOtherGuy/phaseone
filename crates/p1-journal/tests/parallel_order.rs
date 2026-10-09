//! ADR-0118 test 2 on both stores: two Shared calls of one response, the second finishing
//! first, journal Started 1, Started 2, Finished 1, Finished 2 with dense `seq`, and the
//! memory and file journals hold identical records.

use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    BoxFuture, CancellationToken, Clock, CommitSink, Concurrency, DeclarationKind, Effect,
    ModelOptions, RecordBody, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolOutcome,
};
use p1_core::{Agent, AgentParts};
use p1_journal::{JsonlJournal, MemoryJournal, SyncPolicy, load};
use p1_testkit::{
    PassthroughContext, RecordingEvents, ScriptedAuthorization, ScriptedProvider, json_call,
    text_response, tool_call_response,
};
use tempfile::TempDir;
use tokio::sync::Notify;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

#[derive(Debug)]
struct FixedClock;

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        1_700_000_000_000
    }
}

/// A Shared read whose call `first` returns only after the call `second` ran: the second
/// call always finishes first.
struct SecondFinishesFirst {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    second_ran: Notify,
}

impl Tool for SecondFinishesFirst {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    fn concurrency(&self, _call: &ToolCall) -> Concurrency {
        Concurrency::Shared
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            if call.call_id == "first" {
                self.second_ran.notified().await;
            } else {
                self.second_ran.notify_one();
            }
            ToolOutcome::ok(format!("{} read", call.call_id))
        })
    }
}

async fn session(journal: Arc<dyn CommitSink>) {
    let tool: Arc<dyn Tool> = Arc::new(SecondFinishesFirst {
        declaration: ToolDeclaration {
            name: "read".into(),
            description: "read".into(),
            kind: DeclarationKind::Function {
                input_schema: p1_contracts::serde_json::json!({"type": "object"}),
            },
        },
        identity: ToolIdentity {
            implementation: "second-first".into(),
            variant: "test".into(),
        },
        second_ran: Notify::new(),
    });
    let mut agent = Agent::new(AgentParts {
        provider: Arc::new(ScriptedProvider::new(vec![
            tool_call_response(vec![
                json_call("first", "read", "{}"),
                json_call("second", "read", "{}"),
            ]),
            text_response("done"),
        ])),
        tools: vec![tool],
        system_prompt: "parallel order".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    })
    .expect("agent builds");
    agent.set_clock(Arc::new(FixedClock));
    timeout(LIMIT, agent.run_turn("go".into(), CancellationToken::new()))
        .await
        .expect("the turn finishes");
}

#[tokio::test]
async fn a_group_journals_in_block_order_on_memory_and_file_journals() {
    let memory = MemoryJournal::new();
    session(Arc::new(memory.clone())).await;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("session.jsonl");
    session(Arc::new(
        JsonlJournal::create(&path, SyncPolicy::EveryRecord).unwrap(),
    ))
    .await;

    let records = memory.records();
    let tool_records: Vec<String> = records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::ToolStarted { call_id, .. } => Some(format!("started {call_id}")),
            RecordBody::ToolFinished { result, .. } => {
                Some(format!("finished {} {}", result.call_id, result.content))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        tool_records,
        [
            "started first",
            "started second",
            "finished first first read",
            "finished second second read"
        ]
    );
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "seq is dense");
    }
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.records, records, "the file holds the same records");
}
