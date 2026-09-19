//! Lead acceptance: a session file cut at EVERY byte offset (a crash at any moment) can
//! be loaded, repaired, resumed and continued — and the continued session is always
//! well-formed: dense sequence numbers, every tool call paired with exactly one result,
//! a side-effecting tool never executed twice for the same call.

use std::collections::BTreeMap;
use std::fs;
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    AssistantBlock, CancellationToken, Item, ModelOptions, StopReason, Tool, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_journal::{JsonlJournal, SyncPolicy, load, repair_truncated_tail};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, ScriptedAuthorization, ScriptedProvider, Step,
    json_call, text_response, tool_call_response,
};

fn parts(script: Vec<Step>, tool: Arc<FakeTool>, journal: JsonlJournal) -> AgentParts {
    AgentParts {
        provider: Arc::new(ScriptedProvider::new(script)),
        tools: vec![tool as Arc<dyn Tool>],
        system_prompt: "lead crash test".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(journal),
        events: Arc::new(RecordingEvents::new()),
    }
}

async fn turn(agent: &mut Agent, input: &str) -> TurnEnd {
    tokio::time::timeout(
        Duration::from_secs(5),
        agent.run_turn(input.into(), CancellationToken::new()),
    )
    .await
    .expect("turn hung")
}

fn assert_well_formed(history: &[Item], context: &str) {
    let mut open: BTreeMap<String, usize> = BTreeMap::new();
    for item in history {
        match item {
            Item::Assistant(assistant) => {
                assert!(
                    open.values().all(|n| *n == 1),
                    "{context}: a call was left unpaired"
                );
                for block in &assistant.blocks {
                    if let AssistantBlock::ToolCall(call) = block {
                        assert!(
                            open.insert(call.call_id.clone(), 0).is_none(),
                            "{context}: duplicate call id"
                        );
                    }
                }
            }
            Item::ToolResult(result) => {
                let seen = open
                    .get_mut(&result.call_id)
                    .unwrap_or_else(|| panic!("{context}: orphan result"));
                *seen += 1;
            }
            _ => {}
        }
    }
    assert!(
        open.values().all(|n| *n == 1),
        "{context}: calls/results not one-to-one: {open:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn a_session_cut_at_any_byte_resumes_into_a_well_formed_continuation() {
    // 1. A reference session: a turn with two tool calls, then a text turn.
    let dir = tempfile::tempdir().unwrap();
    let full = dir.path().join("full.jsonl");
    let tool = Arc::new(FakeTool::new("deploy"));
    {
        let journal = JsonlJournal::create(&full, SyncPolicy::EveryRecord).unwrap();
        let script = vec![
            tool_call_response(vec![
                json_call("c1", "deploy", "{}"),
                json_call("c2", "deploy", "{}"),
            ]),
            text_response("both done"),
            text_response("second turn"),
        ];
        let mut agent = Agent::new(parts(script, tool.clone(), journal)).unwrap();
        assert!(matches!(
            turn(&mut agent, "deploy twice").await,
            TurnEnd::Completed { .. }
        ));
        assert!(matches!(
            turn(&mut agent, "and again?").await,
            TurnEnd::Completed { .. }
        ));
    }
    let bytes = fs::read(&full).unwrap();
    let reference = load(&full).unwrap();
    assert!(reference.truncated_tail.is_none());
    assert_eq!(
        reference.records.len(),
        10,
        "env,user,assistant,2x(start,finish),assistant,user,assistant"
    );
    let header_len = bytes.iter().position(|b| *b == b'\n').unwrap() + 1;

    // 2. Crash at every byte offset after the header.
    for cut in header_len..=bytes.len() {
        let path = dir.path().join(format!("cut-{cut}.jsonl"));
        fs::write(&path, &bytes[..cut]).unwrap();
        let context = format!("cut at byte {cut}");

        let loaded = load(&path).unwrap_or_else(|error| panic!("{context}: load failed: {error}"));
        // Never lose a complete record, never invent one.
        let complete = bytes[..cut].iter().filter(|b| **b == b'\n').count() - 1;
        assert_eq!(loaded.records.len(), complete, "{context}");
        assert_eq!(loaded.records, reference.records[..complete], "{context}");
        if let Some(tail) = &loaded.truncated_tail {
            repair_truncated_tail(&path, tail).unwrap();
        }

        // 3. Resume and continue with a fresh tool instance that counts executions.
        let next_seq = loaded.records.len() as u64;
        // OsBuffered: this loop runs ~2000 times and durability is not what it tests.
        let journal = JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, next_seq)
            .unwrap_or_else(|error| panic!("{context}: open_for_append failed: {error}"));
        let resumed_tool = Arc::new(FakeTool::new("deploy"));
        let script = vec![text_response("continuing")];
        let (mut agent, report) = Agent::resume(
            parts(script, resumed_tool.clone(), journal),
            &loaded.records,
        )
        .unwrap_or_else(|error| panic!("{context}: resume failed: {error}"));
        let end = turn(&mut agent, "what happened?").await;
        assert_eq!(
            end,
            TurnEnd::Completed {
                stop: StopReason::EndTurn
            },
            "{context}"
        );

        // Resume NEVER re-executes anything: whatever was unresolved is reported to the model.
        assert!(
            resumed_tool.calls().is_empty(),
            "{context}: resume executed a tool ({report:?})"
        );
        assert_well_formed(agent.history(), &context);
        drop(agent);

        // 4. The continued file is itself a clean, dense journal.
        let continued =
            load(&path).unwrap_or_else(|error| panic!("{context}: reload failed: {error}"));
        assert!(continued.truncated_tail.is_none(), "{context}");
        let seqs: Vec<u64> = continued.records.iter().map(|record| record.seq).collect();
        assert_eq!(
            seqs,
            (0..continued.records.len() as u64).collect::<Vec<_>>(),
            "{context}"
        );
        fs::remove_file(&path).unwrap();
    }
}
