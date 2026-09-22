//! ADR-0050 item 3: a worker gets exactly the tools its parent grants.
//!
//! The scratch environments below use only `{{tool_names}}` in their prompts, so
//! each assembles with whatever subset the grant names. Nothing here touches the
//! network or a real credential file: the providers are the testkit's scripted
//! fakes and the "route" is the fake's own description.
#![cfg(feature = "delegation")]

mod common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{Item, ProviderRequest};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A parent that can `worker_start`, and a scratch child whose prompt mentions
/// exactly the tools it is assembled with. The child's own `[[tools]]` lists `read`
/// and nothing else, so a grant of `read` uses the environment's own entry while any
/// other grant falls back to the module's default face.
fn scratch_environments(root: &Path) {
    write_environment(
        root,
        "grant-parent",
        "fake-parent",
        "model-parent",
        &["worker_start", "worker_result"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "grant-child",
        "fake-child",
        "model-child",
        &["read"],
        "CHILD {{tool_names}}",
    );
}

/// A parent that starts one worker with `tools` (a raw JSON array), waits for it and
/// acks the completion notification.
fn parent_that_starts(tools: &str) -> ScriptedProvider {
    ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            &format!(r#"{{"environment":"grant-child","task":"do it","tools":{tools}}}"#),
        )]),
        tool_call_response(vec![json_call(
            "c2",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        text_response("parent done"),
        text_response("parent notified"),
    ])
}

fn tool_names(request: &ProviderRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// The parent was shown a tool result containing `needle`, in any request's history.
fn parent_saw(requests: &[ProviderRequest], needle: &str) -> bool {
    requests.iter().any(|request| {
        request.history.iter().any(|item| match item {
            Item::ToolResult(result) => result.content.contains(needle),
            _ => false,
        })
    })
}

/// (a) A grant of `read` assembles the child with exactly `read` and `finish` — no
/// other tool of the parent's catalog leaks in.
#[tokio::test]
async fn a_grant_is_the_workers_whole_toolset() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![text_response("child done")]);
    let child_handle = child.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent_that_starts(r#"["read"]"#)),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "grant-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child_handle.requests();
    assert_eq!(requests.len(), 1, "the child runs one turn");
    assert_eq!(tool_names(&requests[0]), ["read", "finish"]);
    assert!(requests[0].system_prompt.contains("CHILD"));
}

/// (b) A grant naming a tool the scratch environment does not list is still
/// assembled, with the module's default face.
#[tokio::test]
async fn a_granted_tool_the_environment_does_not_list_is_still_assembled() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![text_response("child done")]);
    let child_handle = child.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent_that_starts(r#"["grep"]"#)),
        ("fake-child", child),
    ]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "grant-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let requests = child_handle.requests();
    assert_eq!(requests.len(), 1);
    // `grep` is not in the child's `[[tools]]`, so it carries its default declaration.
    assert_eq!(tool_names(&requests[0]), ["grep", "finish"]);
    assert!(
        !requests[0].tools[0].description.is_empty(),
        "the default face has a description: {:?}",
        requests[0].tools[0]
    );
    // The child's prompt names exactly its assembled tools.
    assert!(
        requests[0].system_prompt.contains("grep, finish"),
        "prompt: {}",
        requests[0].system_prompt
    );
}

/// (c) `tools: []` is refused with the actionable message; nothing is started and no
/// worker session file is left behind.
#[tokio::test]
async fn an_empty_grant_is_refused_and_starts_nothing() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![text_response("never")]);
    let child_handle = child.clone();
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"grant-child","task":"do it","tools":[]}"#,
        )]),
        text_response("parent done"),
    ]);
    let parent_handle = parent.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![
        ("fake-parent", parent),
        ("fake-child", child),
    ]));

    let session = workspace.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "grant-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        parent_saw(&parent_handle.requests(), "`tools` is required"),
        "the parent must read the refusal: {:?}",
        parent_handle.requests()
    );
    assert!(
        child_handle.requests().is_empty(),
        "a refused start runs no child"
    );
    assert!(
        !workspace.path().join("session.jsonl.w1.jsonl").exists(),
        "a refused start leaves no worker session file"
    );
}

/// (d) The success text names the grant plus `finish` (its `Started worker ` prefix
/// is unchanged, so `workers_started_in` still reads it).
#[tokio::test]
async fn the_success_text_names_the_grant() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    scratch_environments(environments.path());

    let child = ScriptedProvider::new(vec![text_response("child done")]);
    let parent = parent_that_starts(r#"["read"]"#);
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
            "grant-parent",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        parent_saw(
            &parent_handle.requests(),
            "Started worker w1 on fake-route/fake-model with tools: read, finish. You will be \
             notified when it finishes."
        ),
        "requests: {:?}",
        parent_handle.requests()
    );
}
