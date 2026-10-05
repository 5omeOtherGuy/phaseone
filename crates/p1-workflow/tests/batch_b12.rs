//! Migration batch B12: typed masking, run-wide data fuel and orphan artifacts.
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
async fn aggregate_data_fuel_stops_many_individually_legal_strings() {
    let harness = Harness::new();
    let report = harness
        .run(
            r#"
        let values = [];
        for i in 0..24 {
            let s = "x";
            while s.len() < 4 * 1024 * 1024 { s += s; }
            values.push(|| s.len());
        }
        values.len()
    "#,
        )
        .await;
    assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
    let error = report.error.unwrap();
    assert!(error.contains("aggregate script data limit"), "{error}");
    assert!(harness.runner.requests().is_empty());
}

#[tokio::test(flavor = "multi_thread")]
async fn aggregate_data_fuel_is_shared_by_thunks_and_resets_for_new_runs() {
    let harness = Harness::new();
    let single = harness
        .run(
            r#"
        let s = "x";
        while s.len() < 1024 * 1024 { s += s; }
        for j in 0..100 { s.len(); }
        1
    "#,
        )
        .await;
    assert_eq!(single.outcome, RunOutcome::Completed, "{single:?}");
    let report = harness
        .run(
            r#"
        parallel([0, 1, 2, 3, 4, 5, 6, 7].map(|i| || {
            let s = "x";
            while s.len() < 1024 * 1024 { s += s; }
            for j in 0..100 { s.len(); }
            1
        }))
    "#,
        )
        .await;
    assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
    let error = report.error.unwrap();
    assert!(error.contains("aggregate script data limit"), "{error}");
    let next = harness.run("1").await;
    assert_eq!(next.outcome, RunOutcome::Completed, "{next:?}");
}

async fn check_container_fuel(initializer: &str) {
    let harness = Harness::new();
    let report = harness.run(&format!(
        "try {{ let values = []; for i in 0..48 {{ {initializer} values.push(|| a); }} }} catch (e) {{ 1 }}"
    )).await;
    assert_eq!(
        report.outcome,
        RunOutcome::Failed,
        "{initializer}: {report:?}"
    );
    let error = report.error.unwrap();
    assert!(error.contains("aggregate script data limit"), "{error}");
}

#[tokio::test(flavor = "multi_thread")]
async fn aggregate_data_fuel_counts_arrays_and_cannot_be_caught() {
    check_container_fuel("let a = [0]; while a.len() < 32768 { a += a; }").await;
}

#[tokio::test(flavor = "multi_thread")]
async fn aggregate_data_fuel_counts_maps_and_cannot_be_caught() {
    check_container_fuel(
        r#"let s = "x"; while s.len() < 128 * 1024 { s += s; } let a = #{ one: s, two: s };"#,
    )
    .await;
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
