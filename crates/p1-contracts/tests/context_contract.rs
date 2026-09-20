//! Contract-level tests for the context-control records (context.md §1).

use p1_contracts::{JournalRecord, RecordBody, Usage, serde_json};

/// A journal line written before `usage` existed on the record still loads, with
/// `usage: None` — the field is `#[serde(default)]`.
#[test]
fn context_replaced_without_usage_deserializes() {
    let line = r#"{"seq":5,"record":"context_replaced","items":[]}"#;
    let record: JournalRecord = serde_json::from_str(line).expect("old journal line loads");
    assert_eq!(
        record,
        JournalRecord {
            seq: 5,
            body: RecordBody::ContextReplaced {
                items: Vec::new(),
                usage: None,
            },
        }
    );
}

/// The new field round-trips, so usage reported by a policy survives a restart.
#[test]
fn context_replaced_usage_round_trips() {
    let usage = Usage {
        input_uncached: Some(9),
        output: Some(2),
        ..Usage::default()
    };
    let record = JournalRecord {
        seq: 5,
        body: RecordBody::ContextReplaced {
            items: Vec::new(),
            usage: Some(usage),
        },
    };
    let line = serde_json::to_string(&record).unwrap();
    let loaded: JournalRecord = serde_json::from_str(&line).unwrap();
    assert_eq!(loaded, record);
}
