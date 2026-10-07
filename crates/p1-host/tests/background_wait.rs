//! Issue #455: a headless run that starts a background job and has nothing else to do
//! ends its turn and is woken by the job's completion notice, with no model request in
//! between (ADR-0117 jobs, `run.rs` `wait_for_work`). The provider is a scripted fake:
//! no network, tempdirs only.

mod common;

use std::time::{Duration, Instant};

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{InboxKind, Item};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

#[tokio::test]
async fn a_background_job_wakes_the_idle_headless_run_with_its_result() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    // Request 1 starts the job; request 2 ends the turn while it runs; request 3 is the
    // turn the completion notice opens. Nothing else may reach the model.
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            r#"{"command":"sleep 2; echo done-marker","background":true}"#,
        )]),
        text_response("waiting for the job"),
        text_response("the job is done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let started = Instant::now();
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    let elapsed = started.elapsed();
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    assert_eq!(
        requests.len(),
        3,
        "one request per turn, none while waiting"
    );
    // The run waited for the job instead of ending: it lasted at least the job's sleep.
    assert!(elapsed >= Duration::from_secs(2), "elapsed {elapsed:?}");

    // The start call answered with a job id, not with the command's output.
    let start_result = requests[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("the start call's result");
    assert!(start_result.contains("j1"), "{start_result}");
    assert!(!start_result.contains("done-marker"), "{start_result}");

    // The third request carries the completion notice: exit 0 and the job's output.
    let notice = requests[2]
        .history
        .iter()
        .find_map(|item| match item {
            Item::Inbox { kind, text } if *kind == InboxKind::Notification => Some(text.clone()),
            _ => None,
        })
        .expect("the job's completion notice");
    assert!(notice.contains("Background job j1 ended"), "{notice}");
    assert!(notice.contains("Code(0)"), "{notice}");
    assert!(notice.contains("done-marker"), "{notice}");
}
