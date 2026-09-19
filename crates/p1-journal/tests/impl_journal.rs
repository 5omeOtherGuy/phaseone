//! End-to-end tests for the journal stores (must-pass a–f plus the cross-cutting
//! invariants 8a, 8b, 8c, 8e). All files live under `tempfile` dirs, no sleeps.
//!
//! NOTE on truncation at offset 0: `load` of a zero-byte file is
//! `Corrupt{line:1}` (journal.md / brief req 4, "empty file … → Corrupt{line: 1}").
//! At every other byte offset `load` returns a prefix plus a truncated tail and
//! never `Corrupt` (must-pass c).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::{
    AssistantBlock, AssistantItem, CancellationToken, CommitSink, DeclarationKind, Effort,
    InboxKind, InterruptionReason, Item, JournalRecord, ModelOptions, Origin, ProviderError,
    ProviderErrorKind, RecordBody, ReplayData, RouteDescription, StopReason, Tool, ToolCall,
    ToolDeclaration, ToolIdentity, ToolInput, ToolResultItem, ToolStatus, Usage,
};
use p1_core::{Agent, AgentParts, project};
use p1_journal::{
    JournalError, JsonlJournal, MemoryJournal, SyncPolicy, load, repair_truncated_tail,
};
use p1_testkit::{
    FakeTool, RecordingEvents, ReplacingContext, ScriptedAuthorization, ScriptedProvider, Step,
    json_call, text_response, tool_call_response,
};
use tempfile::TempDir;
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

// ----------------------------------------------------------------- helpers

async fn turn(agent: &mut Agent, input: &str) {
    timeout(
        LIMIT,
        agent.run_turn(input.to_string(), CancellationToken::new()),
    )
    .await
    .expect("run_turn hung");
}

fn steps() -> Vec<Step> {
    vec![
        text_response("hello"),
        tool_call_response(vec![
            json_call("c1", "alpha", "{}"),
            json_call("c2", "beta", "{}"),
        ]),
        text_response("done"),
        text_response("inbox answer"),
        text_response("after ctx"),
    ]
}

/// The scripted session from must-pass (a): a text turn, a turn with two tool
/// calls, an inbox message, and a context replacement. Returns the finished agent.
async fn run_scripted_session(journal: Arc<dyn CommitSink>) -> Agent {
    let provider = Arc::new(ScriptedProvider::new(steps()));
    let tools: Vec<Arc<dyn Tool>> = vec![
        Arc::new(FakeTool::new("alpha")),
        Arc::new(FakeTool::new("beta")),
    ];
    let parts = AgentParts {
        provider,
        tools,
        system_prompt: "journal test prompt".into(),
        options: ModelOptions::default(),
        context: Arc::new(ReplacingContext::new(
            8,
            vec![Item::User {
                text: "summary".into(),
            }],
        )),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events: Arc::new(RecordingEvents::new()),
    };
    let mut agent = Agent::new(parts).expect("agent builds");
    turn(&mut agent, "start").await;
    turn(&mut agent, "go").await;
    agent.inbox().send(InboxKind::Steering, "steer");
    timeout(LIMIT, agent.run_inbox_turn(CancellationToken::new()))
        .await
        .expect("inbox turn hung")
        .expect("inbox turn ran");
    turn(&mut agent, "ctx").await;
    agent
}

fn user(seq: u64, text: &str) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::UserInput { text: text.into() },
    }
}

fn six_records() -> Vec<JournalRecord> {
    vec![
        user(0, "a"),
        user(1, "bb"),
        user(2, "ccc"),
        user(3, "unicode ✓ ünïcödé"),
        user(4, "d\"quoted\" \\ back"),
        user(5, "longer text record number six"),
    ]
}

async fn commit(sink: &impl CommitSink, record: &JournalRecord) {
    sink.commit(record).await.expect("commit succeeds");
}

// ------------------------------------------------- (a) memory == jsonl

#[tokio::test]
async fn memory_and_jsonl_sessions_yield_identical_records() {
    let memory = MemoryJournal::new();
    let memory_agent = run_scripted_session(Arc::new(memory.clone())).await;

    let dir = TempDir::new().unwrap();
    let path = dir.path().join("session.jsonl");
    let jsonl = Arc::new(JsonlJournal::create(&path, SyncPolicy::EveryRecord).unwrap());
    let jsonl_agent = run_scripted_session(jsonl).await;

    // Same scripted session → identical record lists.
    let memory_records = memory.records();
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.truncated_tail, None);
    assert_eq!(memory_records, loaded.records);
    assert_eq!(memory_agent.history(), jsonl_agent.history());

    // project of both equals the live agent's history().
    let memory_projection = project(&memory_records).unwrap();
    let jsonl_projection = project(&loaded.records).unwrap();
    assert_eq!(memory_projection, jsonl_projection);
    assert_eq!(memory_projection.history, memory_agent.history());
    assert_eq!(jsonl_projection.history, jsonl_agent.history());
    assert_eq!(memory_projection.next_seq, memory_records.len() as u64);
    assert!(memory_projection.environment_committed);
    assert!(memory_projection.unresolved_calls.is_empty());
}

// ------------------------------------------------- (b) round trip

fn sample_records() -> Vec<JournalRecord> {
    let origin = Origin {
        route: "route ✓\nline".into(),
        model: "model-ü".into(),
    };
    let call = ToolCall {
        call_id: "c1".into(),
        name: "edit".into(),
        input: ToolInput::Json("{\"a\": [1, 2]}".into()),
    };
    let text_call = ToolCall {
        call_id: "c2".into(),
        name: "patch".into(),
        input: ToolInput::Text("line1\nline2".into()),
    };
    let assistant = AssistantItem {
        origin: origin.clone(),
        blocks: vec![
            AssistantBlock::Text {
                text: "héllo\nwörld".into(),
            },
            AssistantBlock::Reasoning {
                text: "think".into(),
                replay: Some(ReplayData {
                    origin: origin.clone(),
                    version: 3,
                    payload: p1_contracts::serde_json::json!({
                        "nested": {"a": [1, {"b": null}]},
                        "s": "line\n\ttab",
                    }),
                }),
            },
            AssistantBlock::ToolCall(call),
            AssistantBlock::ToolCall(text_call),
        ],
    };
    vec![
        JournalRecord {
            seq: 0,
            body: RecordBody::Environment {
                route: RouteDescription {
                    origin: origin.clone(),
                    supports_freeform_tools: true,
                    mandatory_prompt_prefix: Some("pre\nfix".into()),
                    reports_cost: false,
                },
                system_prompt: "sys\nprompt ✓".into(),
                tools: vec![(
                    ToolDeclaration {
                        name: "edit".into(),
                        description: "d\tesc".into(),
                        kind: DeclarationKind::Function {
                            input_schema: p1_contracts::serde_json::json!({
                                "type": "object",
                                "properties": {"a": {"type": "array"}},
                            }),
                        },
                    },
                    ToolIdentity {
                        implementation: "impl".into(),
                        variant: "v\t1".into(),
                    },
                )],
                options: ModelOptions {
                    reasoning_effort: Some(Effort::High),
                    max_output_tokens: Some(123),
                    cache_key: Some("k\n".into()),
                    native: BTreeMap::from([(
                        "x".to_string(),
                        p1_contracts::serde_json::json!({"y": [1, 2]}),
                    )]),
                },
            },
        },
        user(1, "user\ninput ✓"),
        JournalRecord {
            seq: 2,
            body: RecordBody::Inbox {
                kind: InboxKind::Notification,
                text: "note\n".into(),
            },
        },
        JournalRecord {
            seq: 3,
            body: RecordBody::AssistantCompleted {
                item: assistant.clone(),
                stop: StopReason::ToolUse,
                usage: None,
            },
        },
        JournalRecord {
            seq: 4,
            body: RecordBody::AssistantCompleted {
                item: AssistantItem {
                    origin,
                    blocks: vec![AssistantBlock::Text { text: "u2".into() }],
                },
                stop: StopReason::EndTurn,
                usage: Some(Usage::default()),
            },
        },
        JournalRecord {
            seq: 5,
            body: RecordBody::AssistantInterrupted {
                reason: InterruptionReason::ProviderFailed,
                partial_text: "part\n".into(),
                error: Some(ProviderError::new(ProviderErrorKind::Transport, "boom\n")),
            },
        },
        JournalRecord {
            seq: 6,
            body: RecordBody::ToolStarted {
                call_id: "c1".into(),
                identity: ToolIdentity {
                    implementation: "impl".into(),
                    variant: "v".into(),
                },
            },
        },
        JournalRecord {
            seq: 7,
            body: RecordBody::ToolFinished {
                result: ToolResultItem {
                    call_id: "c1".into(),
                    name: "edit".into(),
                    status: ToolStatus::Error,
                    content: "err\n✓".into(),
                },
            },
        },
        JournalRecord {
            seq: 8,
            body: RecordBody::ContextReplaced {
                items: vec![
                    Item::User {
                        text: "sum\n".into(),
                    },
                    Item::Assistant(assistant),
                ],
            },
        },
    ]
}

#[tokio::test]
async fn jsonl_round_trips_every_record_body() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("round.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    let records = sample_records();
    for record in &records {
        commit(&journal, record).await;
    }
    drop(journal);

    let loaded = load(&path).unwrap();
    assert_eq!(loaded.truncated_tail, None);
    assert_eq!(loaded.records, records);
    // The file really is header + one line per record.
    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with("{\"p1_journal\":1}\n"));
    assert_eq!(text.lines().count(), records.len() + 1);
}

// ------------------------------------------------- (c) truncation

#[tokio::test]
async fn truncation_at_every_byte_offset() {
    let dir = TempDir::new().unwrap();
    let full_path = dir.path().join("full.jsonl");
    let journal = JsonlJournal::create(&full_path, SyncPolicy::OsBuffered).unwrap();
    let records = six_records();
    for record in &records {
        commit(&journal, record).await;
    }
    drop(journal);
    let full = std::fs::read(&full_path).unwrap();
    let header_end = full.iter().position(|byte| *byte == b'\n').unwrap() + 1;
    assert!(header_end > 1);

    for len in 0..=full.len() {
        let cut_path = dir.path().join(format!("cut-{len}.jsonl"));
        std::fs::write(&cut_path, &full[..len]).unwrap();

        if len == 0 {
            // Explicit rule: a zero-byte file is not a session (see the file header).
            assert_eq!(
                load(&cut_path).unwrap_err(),
                JournalError::Corrupt { line: 1 }
            );
            continue;
        }

        let loaded = load(&cut_path)
            .unwrap_or_else(|error| panic!("load must not be Corrupt at offset {len}: {error:?}"));
        let complete_newlines = full[..len].iter().filter(|byte| **byte == b'\n').count();
        let expected_records = if len >= header_end {
            complete_newlines - 1
        } else {
            0
        };
        assert_eq!(
            loaded.records.as_slice(),
            &records[..expected_records],
            "records at offset {len}"
        );

        let at_line_end = full[len - 1] == b'\n';
        assert_eq!(
            loaded.truncated_tail.is_none(),
            at_line_end,
            "tail presence at offset {len}"
        );
        if let Some(tail) = loaded.truncated_tail {
            let line_start = full[..len]
                .iter()
                .rposition(|byte| *byte == b'\n')
                .map_or(0, |position| position + 1);
            assert_eq!(tail.byte_offset as usize, line_start, "tail start at {len}");
            assert_eq!(
                tail.bytes as usize,
                len - line_start,
                "tail length at {len}"
            );

            // Caller decides to continue: cut the tail, re-open, append, load clean.
            repair_truncated_tail(&cut_path, &tail).unwrap();
        }

        let next_seq = loaded.records.len() as u64;
        let sink = JsonlJournal::open_for_append(&cut_path, SyncPolicy::OsBuffered, next_seq)
            .unwrap_or_else(|error| panic!("open_for_append after repair at {len}: {error:?}"));
        let appended = user(next_seq, &format!("appended after cut {len}"));
        commit(&sink, &appended).await;
        drop(sink);

        let reloaded = load(&cut_path).unwrap();
        assert_eq!(reloaded.truncated_tail, None, "reloaded tail at {len}");
        assert_eq!(reloaded.records.len(), loaded.records.len() + 1);
        assert_eq!(reloaded.records.last().unwrap(), &appended);
        assert_eq!(
            &reloaded.records[..loaded.records.len()],
            loaded.records.as_slice()
        );
    }
}

// ------------------------------------------------- (d) corrupt middle line

#[tokio::test]
async fn garbage_in_the_middle_line_is_corrupt() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("garbage.jsonl");
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"{\"p1_journal\":1}\n");
    bytes.extend_from_slice(&p1_contracts::serde_json::to_vec(&user(0, "one")).unwrap());
    bytes.push(b'\n');
    bytes.extend_from_slice(b"this is not json\n");
    bytes.extend_from_slice(&p1_contracts::serde_json::to_vec(&user(1, "two")).unwrap());
    bytes.push(b'\n');
    std::fs::write(&path, &bytes).unwrap();

    assert_eq!(load(&path).unwrap_err(), JournalError::Corrupt { line: 3 });
    // `load` never repairs silently: the bytes are untouched.
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
}

// ------------------------------------------------- (e) out-of-order commit

#[tokio::test]
async fn out_of_order_commit_is_rejected_without_storing() {
    let expected = JournalError::OutOfOrder {
        expected: 2,
        got: 3,
    }
    .to_string();

    // Memory store: gap and repeat.
    let memory = MemoryJournal::new();
    commit(&memory, &user(0, "a")).await;
    commit(&memory, &user(1, "b")).await;
    let error = memory.commit(&user(3, "d")).await.unwrap_err();
    assert_eq!(error.0, expected);
    assert_eq!(memory.records(), vec![user(0, "a"), user(1, "b")]);
    let repeat = memory.commit(&user(1, "b")).await.unwrap_err();
    assert_eq!(
        repeat.0,
        JournalError::OutOfOrder {
            expected: 2,
            got: 1
        }
        .to_string()
    );
    assert_eq!(memory.records().len(), 2);

    // File store: the file is byte-for-byte unchanged and still recoverable.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("order.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::EveryRecord).unwrap();
    commit(&journal, &user(0, "a")).await;
    commit(&journal, &user(1, "b")).await;
    let before = std::fs::read(&path).unwrap();
    let error = journal.commit(&user(3, "d")).await.unwrap_err();
    assert_eq!(error.0, expected);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    let error = journal.commit(&user(1, "b")).await.unwrap_err();
    assert_eq!(
        error.0,
        JournalError::OutOfOrder {
            expected: 2,
            got: 1
        }
        .to_string()
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    // The store is still usable at the right sequence.
    commit(&journal, &user(2, "c")).await;
    drop(journal);
    assert_eq!(load(&path).unwrap().records.len(), 3);
}

// ------------------------------------------------- (f) lock

#[tokio::test]
async fn second_writer_is_locked() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("locked.jsonl");
    let first = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    assert_eq!(
        JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 0).unwrap_err(),
        JournalError::Locked
    );
    // `create` on an existing path is a different, earlier failure.
    assert_eq!(
        JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap_err(),
        JournalError::AlreadyExists
    );
    drop(first);
    let second = JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 0)
        .expect("lock released when the first sink dropped");
    commit(&second, &user(0, "after unlock")).await;
    drop(second);
    assert_eq!(load(&path).unwrap().records.len(), 1);
}

// ------------------------------------------------- invariants 8a, 8c, 8e

/// 8a: after `commit` returns `Ok` the record is complete in the file, never
/// partial. Checked by re-reading the file after every single commit.
#[tokio::test]
async fn every_commit_is_completely_visible_once_it_returns() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("durable.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::EveryRecord).unwrap();
    let records = six_records();
    for (index, record) in records.iter().enumerate() {
        commit(&journal, record).await;
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.truncated_tail, None);
        assert_eq!(loaded.records.len(), index + 1);
        assert_eq!(loaded.records.as_slice(), &records[..=index]);
    }
}

/// 8c: the commit future is `Send`, and commits serialise through the store's
/// mutex with no lock held across an await (a held `MutexGuard` across an await
/// would make the future `!Send` and this would not compile).
#[tokio::test]
async fn commit_future_is_send_and_commits_serialise() {
    fn require_send<F: Send>(future: F) -> F {
        future
    }
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("send.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    require_send(journal.commit(&user(0, "a"))).await.unwrap();
    require_send(journal.commit(&user(1, "b"))).await.unwrap();
    drop(journal);
    assert_eq!(load(&path).unwrap().records.len(), 2);
}

/// 8e: session files are created 0600.
#[tokio::test]
async fn created_session_files_are_mode_0600() {
    use std::os::unix::fs::PermissionsExt;
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("mode.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    drop(journal);
    let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600);
}

/// The header of an unknown version is refused, never guessed.
#[tokio::test]
async fn unknown_version_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("version.jsonl");
    std::fs::write(&path, b"{\"p1_journal\":2}\n").unwrap();
    assert_eq!(load(&path).unwrap_err(), JournalError::UnknownVersion);
    // A file with no p1_journal header at all is corrupt, not a version problem.
    std::fs::write(&path, b"{\"other\":1}\n").unwrap();
    assert_eq!(load(&path).unwrap_err(), JournalError::Corrupt { line: 1 });
}

/// `MemoryJournal::from_records` refuses a non-dense seed, like `load` does.
#[tokio::test]
async fn memory_can_be_seeded_only_from_a_dense_sequence() {
    let error = MemoryJournal::from_records(vec![user(0, "a"), user(2, "c")]).unwrap_err();
    assert_eq!(
        error,
        JournalError::OutOfOrder {
            expected: 1,
            got: 2
        }
    );
    let memory = MemoryJournal::from_records(vec![user(0, "a")]).unwrap();
    assert_eq!(memory.records().len(), 1);
    // The seed continues the sequence.
    memory.commit(&user(1, "b")).await.unwrap();
    assert_eq!(memory.records().len(), 2);
}
