//! S6.11 (#363, D083b): the eight worker and workflow members are official-release host
//! entries. The catalog's `worker_*` and `workflow_*` keys are the member components, loaded
//! from the release manifest (the debug fallback in a test build), and no native member is
//! registered: `env show` resolves each key to its package's identity.
//!
//! The members take the host's lists through `workers-observe.grantable` and `.environments`
//! (D084): the parent is shown `worker_start` and `worker_continue` with the host's enums, and
//! a module outside the grantable list is refused with the native texts.
//!
//! The missing and unverifiable release cases are in `crates/p1-module-tests`
//! (`delegation_activation.rs`), which builds releases from the built packages.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use common::run_args;
use p1_contracts::{DeclarationKind, ProviderRequest, serde_json};
use p1_testkit::{json_call, text_response, tool_call_response};
use workflow_common::{Fakes, Scratch, results_of};

/// Every member's catalog key and the package it must resolve to.
const MEMBERS: [(&str, &str); 8] = [
    ("worker_start", "p1/worker-start"),
    ("worker_result", "p1/worker-result"),
    ("worker_continue", "p1/worker-continue"),
    ("worker_cancel", "p1/worker-cancel"),
    ("workflow_start", "p1/workflow-start"),
    ("workflow_status", "p1/workflow-status"),
    ("workflow_result", "p1/workflow-result"),
    ("workflow_cancel", "p1/workflow-cancel"),
];

#[tokio::test]
async fn the_catalogs_members_are_the_host_entry_components() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(Vec::new(), Vec::new(), Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let code = run_args(&mut harness, &["env", "show", "parent"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    let json = &stdout[stdout.find('{').expect("the resolved environment")..];
    let resolved: serde_json::Value = serde_json::from_str(json).expect("JSON");
    let tools = resolved["tools"].as_array().expect("tools");
    for (key, package) in MEMBERS {
        let tool = tools
            .iter()
            .find(|tool| tool["module"] == key)
            .unwrap_or_else(|| panic!("`{key}` is assembled: {stdout}"));
        assert_eq!(
            tool["identity"]["implementation"], package,
            "`{key}` is its package's component, never a native member"
        );
    }
    for native in ["p1-tool-delegate", "p1-tool-workflow"] {
        assert!(!stdout.contains(native), "{native} is registered: {stdout}");
    }
}

/// The `enum` of property `property` (or of its `items`) in `tool`'s schema, as the parent
/// was shown it.
fn schema_enum(request: &ProviderRequest, tool: &str, property: &str) -> Vec<String> {
    let declaration = request
        .tools
        .iter()
        .find(|declaration| declaration.name == tool)
        .unwrap_or_else(|| panic!("the parent has `{tool}`"));
    let DeclarationKind::Function { input_schema } = &declaration.kind else {
        panic!("`{tool}` is a function tool");
    };
    let property = &input_schema["properties"][property];
    let values = property
        .get("items")
        .map_or(&property["enum"], |items| &items["enum"]);
    serde_json::from_value(values.clone()).expect("an enum of strings")
}

#[tokio::test]
async fn the_members_take_the_hosts_lists_and_refuse_with_the_native_texts() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(
        vec![
            tool_call_response(vec![json_call(
                "c1",
                "worker_start",
                r#"{"environment":"fake","task":"look","tools":["read","bogus"]}"#,
            )]),
            tool_call_response(vec![json_call(
                "c2",
                "worker_start",
                r#"{"environment":"fake","task":"look","tools":[]}"#,
            )]),
            tool_call_response(vec![json_call(
                "c3",
                "worker_continue",
                r#"{"id":"w9","message":"again","add_tools":["bogus"]}"#,
            )]),
            text_response("parent done"),
        ],
        Vec::new(),
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

    let requests = fakes.parent.requests();
    let first = &requests[0];
    let grantable = schema_enum(first, "worker_start", "tools");
    assert!(grantable.iter().any(|tool| tool == "read"), "{grantable:?}");
    assert!(
        grantable.iter().all(|tool| tool != "finish"
            && !tool.starts_with("worker_")
            && !tool.starts_with("workflow_")),
        "{grantable:?}"
    );
    assert_eq!(
        schema_enum(first, "worker_continue", "add_tools"),
        grantable
    );
    assert_eq!(
        schema_enum(first, "worker_start", "environment"),
        ["fake", "parent"]
    );

    let valid = grantable.join(", ");
    let last = requests.last().expect("the parent's last request");
    assert_eq!(
        results_of(last, "worker_start"),
        [
            format!(
                "Cannot start worker: `bogus` is not a tool module a worker can be granted. \
                 Valid tools: {valid}"
            ),
            format!("`tools` is required: list every tool module the worker needs, from: {valid}"),
        ]
    );
    // Refused before the id is looked at, as the native tool refuses it.
    assert_eq!(
        results_of(last, "worker_continue"),
        [format!(
            "Cannot add tools to worker w9: `bogus` is not a tool module a worker can be \
             granted. Valid tools: {valid}"
        )]
    );
    assert_eq!(fakes.builds(), 0, "nothing was started");
}
