//! #533 G3a-04: real worker files name the loader-verified assembly that executed them.
#![cfg(feature = "delegation")]

mod common;
#[cfg(feature = "workflows")]
mod workflow_common;

use std::path::Path;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::RecordBody;
use p1_journal::{AssemblyIdentity, Loaded, ModuleKind};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};

fn environments(root: &Path) {
    write_environment(
        root,
        "parent",
        "fake-parent",
        "parent-model",
        &["worker_start", "worker_result", "worker_continue"],
        "PARENT {{tool_names}}",
    );
    write_environment(
        root,
        "child",
        "fake-child",
        "child-model",
        &["read"],
        "CHILD {{tool_names}}",
    );
}

fn start() -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "start",
        "worker_start",
        r#"{"environment":"child","task":"first turn","tools":["read"]}"#,
    )])
}

fn wait() -> p1_testkit::Step {
    tool_call_response(vec![json_call(
        "wait",
        "worker_result",
        r#"{"id":"w1","wait":true}"#,
    )])
}

/// Independently load the shipped package: no unverified lock or hand-made digest.
fn verified_package(_harness: &Harness, identity: &AssemblyIdentity, key: &str, kind: ModuleKind) {
    use p1_module_runtime::{Loader, ReleaseManifest};
    use std::sync::OnceLock;

    static LOADER: OnceLock<Loader> = OnceLock::new();
    let loader = LOADER.get_or_init(|| {
        let path = p1_host::catalog::modules::official_release_manifest().unwrap();
        Loader::new(
            ReleaseManifest::read(&path).unwrap(),
            path.parent().unwrap(),
        )
        .unwrap()
    });
    let module = identity
        .modules
        .iter()
        .find(|module| module.package == key)
        .expect("assembly names the package");
    let expected_name = match key {
        "grep" => "p1/search".to_string(),
        key if key.starts_with("p1/") => key.to_string(),
        key => format!("p1/{key}"),
    };
    assert_eq!(module.name, expected_name);
    let verified = loader
        .load(&expected_name)
        .expect("loader verifies package bytes");
    assert_eq!(module.kind, kind);
    assert_eq!(module.name, verified.name());
    assert_eq!(module.version, env!("CARGO_PKG_VERSION"));
    let digest = verified.digest().to_string();
    assert_eq!(
        module.digest.as_deref(),
        Some(digest.strip_prefix("sha256:").unwrap())
    );
    assert_eq!(module.abi.as_deref(), Some(verified.abi()));
}

/// Check both the journal reader's boundary and physical line ordering.
fn load_worker(session: &Path) -> Loaded {
    let path = p1_host::session::worker_path(session, 1);
    let loaded = p1_journal::load(&path).expect("real worker journal loads");
    assert_eq!(loaded.version, 2);
    assert!(loaded.truncated_tail.is_none());
    let first = loaded
        .assemblies
        .first()
        .expect("worker has an assembly identity");
    assert_eq!(
        first.from_seq,
        loaded.records.first().expect("worker executed").seq
    );
    let bytes = std::fs::read_to_string(path).unwrap();
    let first_line: serde_json::Value =
        serde_json::from_str(bytes.lines().nth(1).unwrap()).unwrap();
    assert!(
        first_line.get("assembly").is_some(),
        "identity precedes first execution record"
    );
    loaded
}

#[tokio::test]
async fn worker_provenance_precedes_first_execution() {
    let workspace = tempfile::tempdir().unwrap();
    let envs = tempfile::tempdir().unwrap();
    environments(envs.path());
    let session = workspace.path().join("parent.jsonl");
    let mut harness = Harness::new(vec![envs.path().to_path_buf()], &[]);
    harness.deps.shell_env = Some(Vec::new());
    harness.deps.catalog_hook = Some(provider_hook(vec![
        (
            "fake-parent",
            ScriptedProvider::new(vec![
                start(),
                wait(),
                text_response("done"),
                text_response("noted"),
            ]),
        ),
        (
            "fake-child",
            ScriptedProvider::new(vec![text_response("worker done")]),
        ),
    ]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "parent",
            "--session",
            session.to_str().unwrap(),
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "{}", harness.stderr.text());
    let loaded = load_worker(&session);
    assert_eq!(loaded.assemblies.len(), 1);
    let identity = &loaded.assemblies[0].identity;
    assert_eq!(identity.environment, "child");
    verified_package(&harness, identity, "read", ModuleKind::Tool);
    verified_package(&harness, identity, "finish", ModuleKind::Tool);
    verified_package(
        &harness,
        identity,
        "p1/policy/full-access",
        ModuleKind::AuthorizationPolicy,
    );
    assert!(
        identity
            .modules
            .iter()
            .any(|module| module.package == "fake-child" && module.kind == ModuleKind::Provider)
    );
    assert!(
        !identity
            .modules
            .iter()
            .any(|module| module.package.starts_with("worker_"))
    );
}

#[tokio::test]
async fn worker_provenance_regrant_names_added_package_before_repaired_turn() {
    let workspace = tempfile::tempdir().unwrap();
    let envs = tempfile::tempdir().unwrap();
    environments(envs.path());
    let session = workspace.path().join("parent.jsonl");
    let mut harness = Harness::new(vec![envs.path().to_path_buf()], &[]);
    harness.deps.shell_env = Some(Vec::new());
    harness.deps.catalog_hook = Some(provider_hook(vec![
        (
            "fake-parent",
            ScriptedProvider::new(vec![
                start(),
                wait(),
                tool_call_response(vec![json_call(
                    "continue",
                    "worker_continue",
                    r#"{"id":"w1","message":"repair turn","add_tools":["edit"]}"#,
                )]),
                wait(),
                text_response("done"),
                text_response("noted"),
            ]),
        ),
        (
            "fake-child",
            ScriptedProvider::new(vec![text_response("need edit"), text_response("repaired")]),
        ),
    ]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "parent",
            "--session",
            session.to_str().unwrap(),
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "{}", harness.stderr.text());
    let loaded = load_worker(&session);
    assert_eq!(
        loaded.assemblies.len(),
        2,
        "regrant must name its new assembly"
    );
    let initial = &loaded.assemblies[0];
    let repaired = &loaded.assemblies[1];
    assert!(
        !initial
            .identity
            .modules
            .iter()
            .any(|module| module.package == "edit")
    );
    verified_package(&harness, &repaired.identity, "edit", ModuleKind::Tool);
    verified_package(&harness, &repaired.identity, "read", ModuleKind::Tool);
    assert!(repaired.from_seq > initial.from_seq);
    let first_repaired = loaded
        .records
        .iter()
        .find(|record| record.seq == repaired.from_seq)
        .unwrap();
    assert!(
        matches!(first_repaired.body, RecordBody::Environment { .. }),
        "identity precedes even reconfiguration's Environment"
    );
    let repair_input = loaded.records.iter().find(|record| matches!(&record.body, RecordBody::UserInput { text } if text == "repair turn")).unwrap();
    assert!(repaired.from_seq < repair_input.seq);
    let lines: Vec<serde_json::Value> =
        std::fs::read_to_string(p1_host::session::worker_path(&session, 1))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
    let position = lines
        .iter()
        .rposition(|line| line.get("assembly").is_some())
        .unwrap();
    assert_eq!(lines[position + 1]["seq"], repaired.from_seq);
}

#[cfg(feature = "workflows")]
#[tokio::test]
async fn worker_provenance_workflow_step_names_its_assembly() {
    use workflow_common::{Fakes, Scratch, done};

    let scratch = Scratch::new();
    let fakes = Fakes::new(Vec::new(), done("step done"), Vec::new());
    let mut harness = scratch.harness();
    harness.deps.catalog_hook = Some(fakes.hook());
    let script = scratch.script("step.rhai", r#"agent("step task").value"#);
    let session = scratch.session();
    let code = run_args(
        &mut harness,
        &[
            "workflow",
            "run",
            script.to_str().unwrap(),
            "--yes",
            "--session",
            session.to_str().unwrap(),
            "--workspace",
            scratch.workspace.path().to_str().unwrap(),
        ],
    )
    .await;
    assert_eq!(code, 0, "{}", harness.stderr.text());
    assert!(!fakes.main.requests().is_empty(), "workflow step ran");
    let loaded = load_worker(&session);
    assert_eq!(loaded.assemblies.len(), 1);
    let identity = &loaded.assemblies[0].identity;
    assert_eq!(identity.environment, "fake");
    verified_package(&harness, identity, "read", ModuleKind::Tool);
    verified_package(&harness, identity, "grep", ModuleKind::Tool);
    verified_package(&harness, identity, "finish", ModuleKind::Tool);
    verified_package(
        &harness,
        identity,
        "p1/policy/full-access",
        ModuleKind::AuthorizationPolicy,
    );
    assert!(
        !identity
            .modules
            .iter()
            .any(|module| module.package.starts_with("workflow_"))
    );
}
