//! ADR-0053 item 7: every MAIN agent gets the four workflow tools, a worker never does.
//!
//! The line path is covered by `workflow_run.rs` (the parent's first request); here the
//! `env show` path, a `worker_start` that grants `workflow_start` (refused), and a
//! direct worker whose tools stay exactly its grant plus `finish`.
//!
//! With the feature off the host still builds and the workflow keys are unknown modules:
//! `cargo check -p p1-host --no-default-features --features delegation` (the gate builds
//! the default features; this file is compiled out then).
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use common::run_args;
use p1_testkit::{json_call, text_response, tool_call_response};
use workflow_common::{Fakes, Scratch, done, results_of, tool_names};

const WORKFLOW_TOOLS: [&str; 4] = [
    "workflow_start",
    "workflow_status",
    "workflow_result",
    "workflow_cancel",
];

#[tokio::test]
async fn env_show_lists_the_workflow_tools_for_a_main_agent() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(Vec::new(), Vec::new(), Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let code = run_args(&mut harness, &["env", "show", "parent"]).await;
    let stdout = harness.stdout.text();
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    for module in WORKFLOW_TOOLS {
        assert!(
            stdout.contains(&format!("\"module\": \"{module}\"")),
            "env show must list `{module}`: {stdout}"
        );
    }
}

#[tokio::test]
async fn a_worker_is_never_granted_or_given_a_workflow_tool() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        vec![
            tool_call_response(vec![json_call(
                "c1",
                "worker_start",
                r#"{"environment":"fake","task":"orchestrate","tools":["workflow_start"]}"#,
            )]),
            tool_call_response(vec![json_call(
                "c2",
                "worker_start",
                r#"{"environment":"fake","task":"look","tools":["read"]}"#,
            )]),
            tool_call_response(vec![json_call(
                "c3",
                "worker_result",
                r#"{"id":"w1","wait":true}"#,
            )]),
            text_response("parent done"),
            text_response("parent notified"),
        ],
        done("looked"),
        Vec::new(),
    );
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "parent",
            "--workspace",
            scratch.workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let parent = fakes.parent.requests();
    let started = results_of(parent.last().unwrap(), "worker_start");
    assert_eq!(started.len(), 2, "{started:?}");
    assert!(
        started[0].contains("workflow_start"),
        "refused: {}",
        started[0]
    );
    assert!(!started[0].contains("Started worker"), "{}", started[0]);

    // The worker that did start (w1: the refused grant built nothing) has its grant
    // plus `finish`, and no workflow tool.
    assert_eq!(fakes.builds(), 1);
    for request in fakes.main.requests() {
        assert_eq!(tool_names(&request), ["read", "finish"]);
    }
}
