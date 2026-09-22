//! ADR-0050 item 6: a short-handed worker is visible without the parent's
//! cooperation.
//!
//! The worker service records, per worker, its granted tools, its `finish` status and
//! `needs`, and every call to a tool it was not given; `worker_result` returns them
//! ahead of the final text, and the host renders every worker's end itself.
//!
//! The scratch environments below use only `{{tool_names}}` in their prompts, so each
//! assembles with whatever subset the grant names. Nothing here touches the network or
//! a real credential file: the providers are the testkit's scripted fakes and the
//! "route" is the fake's own description.
#![cfg(feature = "delegation")]

mod common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, ProviderRequest};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A parent that can `worker_start` and `worker_result`, and a scratch child whose
/// prompt names exactly the tools it is assembled with.
fn scratch_environments(root: &Path) {
    write_environment(
        root,
        "report-parent",
        "fake-parent",
        "model-parent",
        &["worker_start", "worker_result", "worker_continue"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "report-child",
        "fake-child",
        "model-child",
        &["read"],
        "CHILD {{tool_names}}",
    );
}

/// Every `worker_result` content the parent's LAST request carried, in call order.
fn worker_results(requests: &[ProviderRequest]) -> Vec<String> {
    let Some(last) = requests.last() else {
        return Vec::new();
    };
    last.history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == "worker_result" => {
                Some(result.content.clone())
            }
            _ => None,
        })
        .collect()
}

fn start(environment: &str, task: &str, tools: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "c1",
        "worker_start",
        &format!(r#"{{"environment":"{environment}","task":"{task}","tools":{tools}}}"#),
    )])
}

fn result_waiting(id: &str) -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "c2",
        "worker_result",
        &format!(r#"{{"id":"{id}","wait":true}}"#),
    )])
}

/// (a) A worker granted `[read]` that calls `edit` twice and then reports itself
/// blocked: `worker_result` begins with its report, and the line front end printed
/// the worker's end itself.
#[tokio::test]
async fn a_blocked_worker_reports_its_missing_tool_in_the_result_and_on_stderr() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    // `edit` twice, then an accepted `finish` blocked naming `edit` — an accepted
    // `finish` call is the LAST thing the model does before it prints its final text.
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("e1", "edit", r#"{"file_path":"a.txt"}"#),
            json_call("e2", "edit", r#"{"file_path":"b.txt"}"#),
        ]),
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            r#"{"status":"blocked","summary":"cannot edit","needs":"edit"}"#,
        )]),
        text_response("child gave up"),
    ]);
    let parent = ScriptedProvider::new(vec![
        start("report-child", "do it", r#"["read"]"#),
        result_waiting("w1"),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "report-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, finish\n\
             finish: blocked — needs: edit\n\
             calls to tools it was not given: edit x2\n\
             ---\n\
             Worker w1: finished\n\n\
             child gave up"
                .to_string()
        ],
        "the result begins with the report, then `---`, then today's status and text"
    );

    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(
            "· worker w1 (fake-route/fake-model; read, finish) blocked: needs edit — tried \
             edit x2\n"
        ),
        "the line front end must print the worker's end itself: {stderr}"
    );
}

/// (b) A worker that ends its turn without calling `finish`: the result says so, and
/// the host still renders the worker's end.
#[tokio::test]
async fn a_worker_that_never_finished_says_so_in_the_result_and_on_stderr() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![text_response("child answered")]);
    let parent = ScriptedProvider::new(vec![
        start("report-child", "do it", r#"["read"]"#),
        result_waiting("w1"),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "report-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, finish\n\
             finish: not called\n\
             ---\n\
             Worker w1: finished\n\n\
             child answered"
                .to_string()
        ],
        "no finish call, and no missing-call line (there were none)"
    );

    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("· worker w1 (fake-route/fake-model; read, finish) ended without finish\n"),
        "the host reports a worker that ended without finishing: {stderr}"
    );
}

/// (c) A continued worker's second turn starts with a FRESH report: the missing
/// calls of the first turn are gone from the second turn's result.
#[tokio::test]
async fn a_continued_workers_second_turn_starts_with_a_fresh_report() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    // Turn 1 calls `edit` twice and ends; turn 2 makes no calls at all.
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("e1", "edit", r#"{"file_path":"a.txt"}"#),
            json_call("e2", "edit", r#"{"file_path":"b.txt"}"#),
        ]),
        text_response("first turn"),
        text_response("second turn"),
    ]);
    let parent = ScriptedProvider::new(vec![
        start("report-child", "do it", r#"["read"]"#),
        result_waiting("w1"),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"try again"}"#,
        )]),
        result_waiting("w1"),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "report-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let results = worker_results(&parent_handle.requests());
    assert_eq!(
        results,
        vec![
            // Turn 1 recorded the two calls to a tool the worker did not have.
            "tools: read, finish\n\
             finish: not called\n\
             calls to tools it was not given: edit x2\n\
             ---\n\
             Worker w1: finished\n\n\
             first turn"
                .to_string(),
            // Turn 2 is a new turn: everything but the granted tools starts empty.
            "tools: read, finish\n\
             finish: not called\n\
             ---\n\
             Worker w1: finished\n\n\
             second turn"
                .to_string(),
        ]
    );

    // Two turns, two ends reported; only the first named what it tried.
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(
            "· worker w1 (fake-route/fake-model; read, finish) ended without finish — tried \
             edit x2\n"
        ),
        "turn 1's end names the missing calls: {stderr}"
    );
    assert!(
        stderr.contains("; read, finish) ended without finish\n"),
        "turn 2's end has no `tried` part: {stderr}"
    );
}

/// The tap reads the tool the worker was ASSEMBLED with, not the model's name for
/// it: a `finish` face that renames the tool is still found by its identity, and an
/// unavailable call is counted by the name the model used.
#[tokio::test]
async fn an_unavailable_call_is_counted_from_the_models_own_name() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    // A worker that calls tools it was never granted, twice, and then ends: the
    // report counts each name in first-seen order.
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("m1", "shell", r#"{"command":"ls"}"#)]),
        tool_call_response(vec![json_call("m2", "edit", r#"{"file_path":"a.txt"}"#)]),
        text_response("child gave up"),
    ]);
    let parent = ScriptedProvider::new(vec![
        start("report-child", "do it", r#"["read"]"#),
        result_waiting("w1"),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "report-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        worker_results(&parent_handle.requests()),
        vec![
            "tools: read, finish\n\
             finish: not called\n\
             calls to tools it was not given: shell x1, edit x1\n\
             ---\n\
             Worker w1: finished\n\n\
             child gave up"
                .to_string()
        ]
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("— tried shell x1, edit x1\n"),
        "first-seen order: {stderr}"
    );
}
