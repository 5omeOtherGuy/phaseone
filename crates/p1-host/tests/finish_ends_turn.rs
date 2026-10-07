//! ADR-0120 at the host boundary: a shipped environment's `finish`, assembled by the real
//! catalog with the completion hub's gate, ends the turn without another request when the
//! host accepts it; a rejected call costs exactly one repair request. Scripted provider,
//! real `p1/finish` package, tempdir workspace, no network.

mod common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::RecordBody;
use p1_testkit::{ScriptedProvider, json_call, tool_call_response};
use tempfile::tempdir;

/// An accepted `done` in a session that changed no file.
const DONE_NONE: &str = r#"{"status":"done","summary":"answered","verification":["none"]}"#;
/// A `done` naming a command that never ran: the host refuses it.
const DONE_NEVER_RUN: &str = r#"{"status":"done","summary":"s","verification":["never-run"]}"#;

fn finish_environment(root: &Path) {
    write_environment(
        root,
        "finish-env",
        "fake",
        "fake-model",
        &["finish"],
        "test",
    );
}

#[tokio::test]
async fn an_accepted_finish_makes_no_further_request() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path());
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![tool_call_response(vec![json_call(
        "f1", "finish", DONE_NONE,
    )])]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        provider.requests().len(),
        1,
        "one tool-calling response, one request: the accepted finish adds none"
    );
    let loaded = p1_journal::load(&session).unwrap();
    match &loaded.records.last().expect("records").body {
        RecordBody::ToolFinished { result, .. } => assert_eq!(result.name, "finish"),
        other => panic!("the journal must end with the finish result: {other:?}"),
    }
}

#[tokio::test]
async fn a_rejected_finish_costs_exactly_one_repair_request() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    finish_environment(environments.path());
    let session = workspace.path().join("session.jsonl");
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("f1", "finish", DONE_NEVER_RUN)]),
        tool_call_response(vec![json_call("f2", "finish", DONE_NONE)]),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "finish-env",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        provider.requests().len(),
        2,
        "one request to reject, one to repair, and none for the accepted finish"
    );
}
