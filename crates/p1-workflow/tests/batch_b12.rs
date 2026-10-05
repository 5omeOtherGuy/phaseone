//! Migration batch B12: typed masking and orphan artifacts.
mod support;

use p1_workflow::{JournalRecord, RunOutcome, SchemaCheck, StartRequest};
use serde_json::{Value, json};
use support::{Harness, done_with, request};

#[tokio::test(flavor = "multi_thread")]
async fn credential_keys_mask_every_json_type_at_persistence_boundaries() {
    let harness = Harness::new();
    let mut fields = serde_json::Map::new();
    for key in [
        "api_key",
        "password",
        "secret",
        "token",
        "authorization",
        "access",
        "refresh",
        "key",
    ] {
        fields.insert(
            key.to_uppercase(),
            json!([null, true, 123456, format!("opaque-{key}"), [format!("opaque-{key}")], {"part": format!("opaque-{key}")}]),
        );
    }
    let mut args = serde_json::Map::new();
    // Each recognized key appears with each possible JSON value type.
    for (key, variants) in fields {
        for (i, variant) in variants.as_array().unwrap().iter().enumerate() {
            args.insert(format!("case-{key}-{i}"), json!({key.clone(): variant}));
        }
    }
    args.insert("token_count".into(), json!(42));
    args.insert("tokens".into(), json!(["a", "b"]));
    let original = Value::Object(args);
    let mut expected = original.clone();
    for (key, value) in expected.as_object_mut().unwrap() {
        if key.starts_with("case-") {
            for value in value.as_object_mut().unwrap().values_mut() {
                *value = json!("<redacted:credential>");
            }
        }
    }
    harness
        .runner
        .queue("typed", done_with(original.clone(), SchemaCheck::Passed));
    let id = harness
        .start_request(StartRequest {
            args: original,
            ..request("#{ input: args, output: agent(\"typed\").value }")
        })
        .await;
    let report = harness.wait(&id).await;
    assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
    assert_eq!(report.value["input"], expected);
    assert_eq!(report.value["output"], expected);
    let read = |name| {
        serde_json::from_slice::<Value>(&std::fs::read(report.run_dir.join(name)).unwrap()).unwrap()
    };
    assert_eq!(read("args.json"), expected);
    assert_eq!(read("result.json")["value"], report.value);
    let journal = harness.journal(&id);
    assert!(matches!(&journal[0], JournalRecord::Started { args, .. } if *args == expected));
    assert!(journal.iter().any(|record| matches!(record, JournalRecord::Result { envelope, .. } if envelope.value == expected)));
}

#[tokio::test(flavor = "multi_thread")]
async fn orphan_result_without_ended_is_not_a_completed_predecessor() {
    let harness = Harness::new();
    let first = harness.run("1").await;
    let path = first.run_dir.join("journal.jsonl");
    let mut records = harness.journal(&first.id);
    assert!(matches!(records.pop(), Some(JournalRecord::Ended { .. })));
    let mut journal = String::new();
    for record in records {
        journal.push_str(&serde_json::to_string(&record).unwrap());
        journal.push('\n');
    }
    // Model abrupt exit after publication but before terminal append. Artifact stays;
    // only the journal can make a run an eligible predecessor.
    std::fs::write(path, journal).unwrap();
    assert!(first.run_dir.join("result.json").is_file());
    let error = harness
        .service
        .start(StartRequest {
            resume_from: Some(first.id),
            ..request("1")
        })
        .await
        .unwrap_err();
    assert!(error.to_string().contains("ended predecessor"), "{error}");
}

use p1_workflow::WorkflowService;
