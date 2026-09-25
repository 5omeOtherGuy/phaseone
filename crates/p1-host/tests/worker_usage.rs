//! Worker token usage is visible and durable.
//!
//! With `--session FILE`, worker `w<N>` journals to its OWN new JSONL file
//! `FILE.w<N>.jsonl`: the parent file never contains the child's records, an
//! existing worker file is SKIPPED — its id is reserved and the file is never
//! overwritten (issue #98) — and the host prints a `workers total (<n>)` line
//! after the parent's own `total` line. Without `--session` the child stays in
//! memory and no file appears.
#![cfg(feature = "delegation")]

mod common;

use std::fs;
use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{StopReason, StreamEvent, Usage};
use p1_testkit::{
    ScriptedProvider, Step, completed, json_call, text_block, text_response, tool_call_response,
};
use tempfile::tempdir;

/// A child response that reports usage, so the workers aggregate has real numbers.
fn child_text_with_usage(text: &str, usage: Usage) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block(text)],
            StopReason::EndTurn,
            Some(usage),
        )),
    ])
}

/// 100 uncached + 20 cached + 5 cache-write = 125 input, 7 output, $0.0123.
fn child_usage() -> Usage {
    Usage {
        input_uncached: Some(100),
        cache_read: Some(20),
        cache_write: Some(5),
        output: Some(7),
        reasoning_output: None,
        cost_micro_usd: Some(12_345),
    }
}

/// Parent starts one worker on environment `b`, ends its turn, and acks the
/// completion notification. Three requests; spare steps are unused.
fn one_worker_parent() -> ScriptedProvider {
    ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"b","task":"do it","tools":["read"]}"#,
        )]),
        text_response("parent started"),
        text_response("parent done"),
        text_response("ack"),
        text_response("ack again"),
    ])
}

fn declared_environments(environments: &Path) {
    write_environment(
        environments,
        "a",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        environments,
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "CHILD {{tool_names}}",
    );
}

/// The worker's session file next to the parent's, exactly `FILE.w<id>.jsonl`.
fn worker_session_file(workspace: &Path) -> std::path::PathBuf {
    workspace.join("session.jsonl.w1.jsonl")
}

#[tokio::test]
async fn a_session_worker_journals_to_its_own_file_and_the_host_reports_it() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());

    let child = ScriptedProvider::new(vec![child_text_with_usage("child done", child_usage())]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-a", one_worker_parent()),
        ("fake-b", child),
    ]));

    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    // The worker file is a real journal: header line plus the child's records.
    let worker_text = fs::read_to_string(worker_session_file(workspace.path()))
        .expect("worker session file exists");
    assert_eq!(
        worker_text.lines().next(),
        Some("{\"p1_journal\":2}"),
        "worker file header: {worker_text}"
    );
    assert!(worker_text.contains("\"record\":\"environment\""));
    assert!(worker_text.contains("\"record\":\"assistant_completed\""));
    assert!(
        worker_text.contains("\"input_uncached\":100"),
        "worker usage missing: {worker_text}"
    );
    assert!(
        worker_text.contains("child done"),
        "worker file: {worker_text}"
    );

    // The parent file holds the parent's records only.
    let parent_text = fs::read_to_string(&session).unwrap();
    assert!(parent_text.contains("\"record\":\"user_input\""));
    assert!(
        !parent_text.contains("\"input_uncached\":100"),
        "child usage leaked into the parent file: {parent_text}"
    );
    assert!(
        !parent_text.contains("child done"),
        "child text leaked into the parent file: {parent_text}"
    );

    // The exit line follows the parent's own total and carries the child's numbers.
    let stderr = harness.stderr.text();
    let expected = "workers total (1) · in 125 (cached 20) · out 7 · cost $0.0123";
    let parent_total = stderr.find("total model").unwrap_or_else(|| {
        panic!("parent total missing from stderr: {stderr}");
    });
    let workers_total = stderr.find(expected).unwrap_or_else(|| {
        panic!("workers total missing from stderr: {stderr}");
    });
    assert!(
        workers_total > parent_total,
        "workers total must follow the parent total: {stderr}"
    );
}

#[tokio::test]
async fn a_second_worker_gets_the_next_session_file() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"b","task":"first","tools":["read"]}"#,
        )]),
        text_response("first started"),
        tool_call_response(vec![json_call(
            "c2",
            "worker_start",
            r#"{"environment":"b","task":"second","tools":["read"]}"#,
        )]),
        text_response("second started"),
        text_response("ack"),
        text_response("ack"),
        text_response("ack"),
    ]);
    let child = ScriptedProvider::new(vec![
        child_text_with_usage("child one", child_usage()),
        child_text_with_usage("child two", child_usage()),
    ]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));

    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    for name in ["session.jsonl.w1.jsonl", "session.jsonl.w2.jsonl"] {
        let text = fs::read_to_string(workspace.path().join(name)).unwrap_or_else(|error| {
            panic!("{name} missing: {error}");
        });
        assert_eq!(text.lines().next(), Some("{\"p1_journal\":2}"), "{name}");
        assert_eq!(
            text.matches("\"record\":\"assistant_completed\"").count(),
            1,
            "{name} holds exactly one child response: {text}"
        );
    }

    let stderr = harness.stderr.text();
    let expected = "workers total (2) · in 250 (cached 40) · out 14 · cost $0.0246";
    assert!(
        stderr.contains(expected),
        "stderr missing `{expected}`: {stderr}"
    );
}

#[tokio::test]
async fn without_a_session_the_worker_stays_in_memory_but_is_still_counted() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());

    let child = ScriptedProvider::new(vec![child_text_with_usage("child done", child_usage())]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-a", one_worker_parent()),
        ("fake-b", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let created: Vec<_> = fs::read_dir(workspace.path())
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    assert!(
        created.is_empty(),
        "no file may be created without --session: {created:?}"
    );

    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("workers total (1) · in 125 (cached 20) · out 7 · cost $0.0123"),
        "workers total missing from stderr: {stderr}"
    );
}

/// An existing `session.jsonl.w1.jsonl` is SKIPPED, never reused and never
/// overwritten (issue #98): the reservation sees the file before the first
/// `worker_start`, so the parent's first worker is `w2`, and the file that was
/// already there keeps every byte it had.
#[tokio::test]
async fn an_existing_worker_file_is_skipped_and_never_overwritten() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    declared_environments(environments.path());

    let sentinel = "keep me: this is not a journal\n";
    let worker_file = worker_session_file(workspace.path());
    fs::write(&worker_file, sentinel).unwrap();

    let child = ScriptedProvider::new(vec![child_text_with_usage("child done", child_usage())]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-a", one_worker_parent()),
        ("fake-b", child.clone()),
    ]));

    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "a",
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
        fs::read_to_string(&worker_file).unwrap(),
        sentinel,
        "an existing worker file must not be touched"
    );

    // The id the existing file occupies is reserved, so the worker runs as `w2`
    // and journals there instead of colliding with `w1`.
    let second = workspace.path().join("session.jsonl.w2.jsonl");
    let text = fs::read_to_string(&second)
        .unwrap_or_else(|error| panic!("{} missing: {error}", second.display()));
    assert_eq!(text.lines().next(), Some("{\"p1_journal\":2}"), "{text}");
    assert!(text.contains("child done"), "worker file: {text}");

    // The worker past the existing file really ran, and its usage reached the
    // parent's aggregate.
    assert!(
        !child.requests().is_empty(),
        "the worker started as w2 was asked to run"
    );
    let stderr = harness.stderr.text();
    let expected = "workers total (1) · in 125 (cached 20) · out 7 · cost $0.0123";
    assert!(
        stderr.contains(expected),
        "stderr missing `{expected}`: {stderr}"
    );
}
