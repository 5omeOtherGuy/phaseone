//! Issue #424, ADR-0124: the shipped `deepseek-review` environment is a leaf. Its main
//! agent gets exactly its own tools (no worker or workflow tools), and a review that writes
//! its report to the run's scratch directory finishes with `verification: ["none"]`, accepted
//! at once. The provider is a scripted fake: no network, tempdirs only.

mod common;

use common::{Harness, provider_hook, run_args, shipped_environments};
use p1_testkit::{ScriptedProvider, json_call, tool_call_response};
use tempfile::tempdir;

#[tokio::test]
async fn the_review_environment_reports_to_scratch_and_finishes_unverified() {
    let workspace = tempdir().unwrap();
    std::fs::write(workspace.path().join("lib.rs"), "fn main() {}\n").unwrap();
    let logs = tempdir().unwrap();
    let session = logs.path().join("session.jsonl");
    let report = logs
        .path()
        .join("session.jsonl.scratch")
        .join("findings.md");

    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "w1",
            "write",
            &serde_json::json!({
                "file_path": report.to_str().unwrap(),
                "content": "no findings\n",
            })
            .to_string(),
        )]),
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"done","summary":"0 findings","verification":["none"]}"#,
        )]),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("opencode-go-subscription", provider)]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "deepseek-review",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "review lib.rs",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    let names: Vec<&str> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert_eq!(
        names,
        ["read", "grep", "shell", "read_output", "write", "finish"],
        "a leaf environment gets no worker or workflow tools"
    );
    // The accepted finish ended the turn (ADR-0120): no request after it, so no refusal.
    assert_eq!(requests.len(), 2, "stderr: {}", harness.stderr.text());
    assert_eq!(std::fs::read_to_string(&report).unwrap(), "no findings\n");
    assert_eq!(
        std::fs::read_dir(workspace.path()).unwrap().count(),
        1,
        "the workspace holds only its own file"
    );
}
