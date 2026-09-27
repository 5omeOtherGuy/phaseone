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

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{env_show_json, run_args, write_environment};
use p1_assembly::Catalog;
use p1_contracts::{BoxFuture, CancellationToken, DeclarationKind, ProviderRequest, serde_json};
use p1_testkit::{json_call, text_response, tool_call_response};
use p1_workflow::{RunId, RunStatus, StartRequest, WorkflowError, WorkflowService};
use workflow_common::{Fakes, Scratch, results_of, write};

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

/// S6.11 item 1: a host entry accepts the face overrides the native member it replaced did.
/// An environment that names `worker_start` with `name = "spawn"` gets the same component,
/// its schema and semantics, exposed under that name; the identity stays the package's.
#[tokio::test]
async fn a_host_entry_takes_the_face_the_native_member_took() {
    let scratch = Scratch::new();
    let fakes = Fakes::new(Vec::new(), Vec::new(), Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    write(
        &scratch
            .root
            .path()
            .join("environments/spawn/environment.toml"),
        "family = \"spawn\"\nprovider = \"fake-parent\"\nmodel = \"model-parent\"\n\n\
         [[tools]]\nmodule = \"worker_start\"\nname = \"spawn\"\n",
    );
    write(
        &scratch.root.path().join("environments/spawn/prompt.md"),
        "PARENT {{tool_names}}\n",
    );

    let code = run_args(&mut harness, &["env", "show", "spawn"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stdout = harness.stdout.text();
    let resolved = env_show_json(&stdout);
    let tools = resolved["tools"].as_array().expect("tools");
    let tool = tools
        .iter()
        .find(|tool| tool["declaration"]["name"] == "spawn")
        .unwrap_or_else(|| panic!("`worker_start` is assembled under `spawn`: {stdout}"));
    assert_eq!(tool["identity"]["implementation"], "p1/worker-start");
    assert!(
        tools
            .iter()
            .all(|tool| tool["declaration"]["name"] != "worker_start"),
        "the entry keeps only the face the environment gave it: {stdout}"
    );
}

/// The `modules.lock` text selecting the built `package` under `module`, pinning what the
/// release ships exactly as its package manifest records it.
fn lock_for(module: &str, package: &str) -> String {
    let directory = format!(
        "p1-module-{}",
        package.strip_prefix("p1/").expect("a p1 package")
    );
    let dir = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules")
        .join(&directory);
    let path = dir.join(format!("{directory}.manifest.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the built package {} is missing ({error}): run scripts/build-modules.sh --all",
            path.display()
        )
    });
    let manifest: serde_json::Value = serde_json::from_str(&text).expect("the package manifest");
    let entry = serde_json::json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("packages/{directory}/{directory}.wasm"),
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"],
        "variant": manifest["variant"],
    });
    p1_module_tests::lock_text(module, &entry)
}

/// S6.11 item 2: a member a lock selects is linked with the same host lists and grant check
/// as a host entry, so a locked `worker-start` declares the host's non-empty `tools` and
/// `environment` enums (D084) instead of empty ones.
#[tokio::test]
async fn a_locked_member_takes_the_hosts_lists() {
    let scratch = Scratch::new();
    std::fs::write(
        scratch.root.path().join("modules.lock"),
        lock_for("worker-start", "p1/worker-start"),
    )
    .expect("modules.lock");
    write_environment(
        &scratch.root.path().join("environments"),
        "locked",
        "fake-parent",
        "model-parent",
        &["worker-start"],
        "PARENT {{tool_names}}\n",
    );
    let fakes = Fakes::new(vec![text_response("parent done")], Vec::new(), Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "locked",
            "--workspace",
            scratch.workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = fakes.parent.requests();
    let first = requests.first().expect("the parent's first request");
    let grantable = schema_enum(first, "worker_start", "tools");
    assert!(!grantable.is_empty(), "the locked member names the tools");
    assert!(grantable.iter().any(|tool| tool == "read"), "{grantable:?}");
    assert!(
        grantable
            .iter()
            .all(|tool| tool != "finish" && !tool.starts_with("worker_")),
        "{grantable:?}"
    );
    assert!(
        !schema_enum(first, "worker_start", "environment").is_empty(),
        "the locked member names the environments"
    );
}

/// A workflow service that starts nothing: the case only needs a service present for the
/// workflow members to be registered at all.
struct NoRuns;

impl WorkflowService for NoRuns {
    fn start<'a>(&'a self, _request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async {
            Err(WorkflowError::Preflight(
                "this service starts nothing".into(),
            ))
        })
    }

    fn status<'a>(&'a self, _id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async { Err(WorkflowError::UnknownRun) })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a RunId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async { Err(WorkflowError::UnknownRun) })
    }

    fn cancel<'a>(&'a self, _id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async { Err(WorkflowError::UnknownRun) })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        Box::pin(async { Vec::new() })
    }
}

/// The built release, with the component `missing` dropped, written into a fresh directory:
/// the incomplete release a case asks the host to load. A release missing a member is refused
/// before any component is read, so nothing else of it needs to be present.
fn release_without(missing: &str) -> (tempfile::TempDir, PathBuf) {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules");
    let path = dir.join("manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the built release manifest {} is missing ({error}): run scripts/build-modules.sh --all",
            path.display()
        )
    });
    let mut manifest: serde_json::Value =
        serde_json::from_str(&text).expect("the release manifest");
    manifest["components"]
        .as_array_mut()
        .expect("components")
        .retain(|entry| entry["name"] != missing);
    let release = tempfile::tempdir().expect("release dir");
    let manifest_path = release.path().join("manifest.json");
    std::fs::write(&manifest_path, manifest.to_string()).expect("release manifest");
    (release, manifest_path)
}

/// S6.11 item 3: with a workflow service present, a release that does not ship
/// `p1/workflow-start` fails the catalog build with the loader error naming the package
/// (ADR-0079, the S7.10.3 contract), never a catalog silently built without the workflow
/// tools.
#[test]
fn a_release_without_the_workflow_member_fails_the_catalog_build() {
    let (_release, manifest) = release_without("p1/workflow-start");
    let mut catalog = Catalog::new();
    let error = p1_host::workflow::register_workflow_tools_from(
        &mut catalog,
        Some(Arc::new(NoRuns)),
        Some(&manifest),
    )
    .expect_err("a release without p1/workflow-start is refused");
    assert!(error.contains("p1/workflow-start"), "{error}");
    assert!(
        catalog.tool_keys().is_empty(),
        "no workflow tool is registered over an incomplete release"
    );
}
