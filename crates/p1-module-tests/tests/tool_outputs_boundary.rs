//! The `tool-outputs` capability at the module boundary (ADR-0109 items 5 and 6, #510
//! definition of done 7): only a `tool` package may be granted it, a component importing it
//! without the grant does not load, a grant without the host's store does not link, and the
//! shipped `p1/shell` holds the grant and links only with the store beside its process
//! service.
//!
//! No shipped component calls `tool-outputs` yet (the shell's use of it and `read_output` are
//! #511), so the importing component here is the smallest one possible: a component whose one
//! import is the interface, built byte by byte below. The loader and the linker decide on the
//! import's name and the manifest's grant, never on what the component does with it.

use std::sync::Arc;

use p1_assembly::ModulesLock;
use p1_contracts::serde_json::{self, Value, json};
use p1_host::catalog::modules::{ModulePackage, ModulesError, load_locked_modules};
use p1_module_runtime::process::{ProcessCapability, ProcessService};
use p1_module_runtime::{
    Digest, ExecutionLimits, LINKABLE_CAPABILITIES, LinkError, LoadError, Services, ToolError,
    wasm_tool,
};
use p1_module_tests::{Release, lock_text, within_deadline};
use p1_redact::MaskCounter;

/// The interface under test, as the manifest and `modules/capabilities.toml` name it.
const INTERFACE: &str = "tool-outputs";
/// The module name the test lock gives the probe.
const MODULE: &str = "probe";
const NAME: &str = "p1/outputs-probe";

/// A component whose only import is an (empty) instance named
/// `p1:module/<interface>@1.0.0`, exactly `wasm-tools parse` of
/// `(component (import "p1:module/<interface>@1.0.0" (instance)))`.
fn importing(interface: &str) -> Vec<u8> {
    let name = format!("p1:module/{interface}@1.0.0");
    assert!(name.len() < 0x80, "one-byte LEB128 lengths only");
    let mut bytes = vec![0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00];
    // Type section: one instance type with no declarations.
    bytes.extend([0x07, 0x03, 0x01, 0x42, 0x00]);
    // Import section: one import, a plain name, of sort instance with type 0.
    let mut import = vec![0x01, 0x00, name.len() as u8];
    import.extend(name.as_bytes());
    import.extend([0x05, 0x00]);
    bytes.push(0x0a);
    bytes.push(import.len() as u8);
    bytes.extend(import);
    bytes
}

/// A release holding the probe as a package of `kind`, granted `capabilities`, and its entry.
fn probe_release(kind: &str, capabilities: Value) -> (Release, Value) {
    let bytes = importing(INTERFACE);
    let mut release = Release::empty();
    let entry = json!({
        "name": NAME,
        "digest": Digest::of(&bytes).to_string(),
        "path": "packages/p1-outputs-probe/p1-outputs-probe.wasm",
        "kind": kind,
        "world": format!("p1:module/{kind}@1.0.0"),
        "protocol": "1.0",
        "capabilities": capabilities,
        "variant": "default",
    });
    release.add(entry.clone(), &bytes);
    (release, entry)
}

/// What the host's lock path loads for a lock selecting `entry` from `release`.
fn load(release: &Release, entry: &Value) -> Result<Vec<ModulePackage>, ModulesError> {
    let lock = ModulesLock::parse(
        &release.root().join("modules.lock"),
        &lock_text(MODULE, entry),
    )
    .expect("the lock parses");
    load_locked_modules(&lock, &release.manifest_file())
}

/// The built `p1/shell` package and its manifest, as `scripts/build-modules.sh` published them.
fn shell_artifact() -> (Vec<u8>, Value) {
    let dir = p1_module_tests::fixture_dir()
        .parent()
        .expect("the publish directory")
        .join("p1-module-shell");
    let read = |file: &str| {
        let path = dir.join(file);
        std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "{} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    };
    let manifest = serde_json::from_slice(&read("p1-module-shell.manifest.json"))
        .expect("the package manifest is JSON");
    (read("p1-module-shell.wasm"), manifest)
}

#[test]
fn the_runtime_links_tool_outputs() {
    assert!(LINKABLE_CAPABILITIES.contains(&INTERFACE));
}

#[test]
fn a_tool_granted_tool_outputs_loads() {
    let (release, entry) = probe_release("tool", json!([INTERFACE]));
    let packages = load(&release, &entry).expect("a granted tool package loads");
    assert_eq!(packages[0].loaded.capabilities(), [INTERFACE]);
}

#[test]
fn an_import_of_tool_outputs_the_manifest_does_not_grant_is_refused() {
    let (release, entry) = probe_release("tool", json!([]));
    match load(&release, &entry) {
        Err(ModulesError::Load { source, .. }) => match *source {
            LoadError::UndeclaredImport { name, import } => {
                assert_eq!(name, NAME);
                assert_eq!(import, "p1:module/tool-outputs@1.0.0");
            }
            other => panic!("wrong refusal: {other}"),
        },
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a component importing an ungranted tool-outputs must not load"),
    }
}

#[test]
fn no_other_class_may_be_granted_tool_outputs() {
    for kind in [
        "provider",
        "context-policy",
        "authorization-policy",
        "workflow-implementation",
        "workflow-decision",
    ] {
        let (release, entry) = probe_release(kind, json!([INTERFACE]));
        match load(&release, &entry) {
            Err(ModulesError::CapabilityNotAllocated {
                capability,
                kind: refused,
                ..
            }) => {
                assert_eq!(capability, INTERFACE);
                assert_eq!(refused, kind);
            }
            Err(other) => panic!("{kind}: wrong error: {other}"),
            Ok(_) => panic!("a {kind} package must not be granted tool-outputs"),
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn granted_tool_outputs_without_a_service_is_a_missing_service() {
    within_deadline(
        "granted_tool_outputs_without_a_service_is_a_missing_service",
        async {
            let (release, entry) = probe_release("tool", json!([INTERFACE]));
            let packages = load(&release, &entry).expect("the probe loads");
            match wasm_tool(
                &packages[0].loaded,
                Services::default(),
                ExecutionLimits::default(),
                &Arc::new(MaskCounter::new()),
            ) {
                Err(ToolError::Link {
                    source: LinkError::MissingService(capability),
                    ..
                }) => assert_eq!(capability, INTERFACE),
                Err(other) => panic!("wrong error: {other}"),
                Ok(_) => panic!("tool-outputs must not link without a service"),
            }
        },
    )
    .await;
}

/// The shipped shell holds the grant (ADR-0109 item 6), so the host must give it the store:
/// its process service alone no longer links it.
#[tokio::test(flavor = "current_thread")]
async fn the_shipped_shell_is_granted_tool_outputs_and_needs_the_store() {
    within_deadline(
        "the_shipped_shell_is_granted_tool_outputs_and_needs_the_store",
        async {
            let (wasm, manifest) = shell_artifact();
            assert!(
                manifest["capabilities"]
                    .as_array()
                    .expect("capabilities")
                    .contains(&json!(INTERFACE)),
                "{manifest}"
            );
            let mut release = Release::empty();
            let name = manifest["name"]
                .as_str()
                .expect("the manifest name")
                .to_owned();
            release.add(
                json!({
                    "name": name,
                    "digest": manifest["digest"],
                    "path": "packages/p1-shell/p1-shell.wasm",
                    "kind": manifest["kind"],
                    "world": manifest["world"],
                    "protocol": manifest["protocol"],
                    "capabilities": manifest["capabilities"],
                    "variant": manifest["variant"],
                }),
                &wasm,
            );
            let module = release.loader().load(&name).expect("the shell loads");
            let workspace = tempfile::tempdir().expect("workspace");
            let service =
                Arc::new(ProcessService::new(workspace.path()).with_env_snapshot(Vec::new()));
            let jobs = Arc::new(p1_module_runtime::jobs::JobRegistry::new(
                service.clone(),
                Arc::new(p1_module_runtime::OutputStore::temporary(
                    p1_module_runtime::OutputCaps::DEFAULT,
                )),
                Default::default(),
            ));
            let process = Arc::new(ProcessCapability::new(service));
            let only_process = Services {
                process: Some(process.clone()),
                process_jobs: Some(jobs.clone()),
                ..Services::default()
            };
            match wasm_tool(
                &module,
                only_process,
                ExecutionLimits::default(),
                &Arc::new(MaskCounter::new()),
            ) {
                Err(ToolError::Link {
                    source: LinkError::MissingService(capability),
                    ..
                }) => assert_eq!(capability, INTERFACE),
                Err(other) => panic!("wrong error: {other}"),
                Ok(_) => panic!("the shell must not link without the output store"),
            }
            let store = Arc::new(p1_module_runtime::OutputStore::temporary(
                p1_module_runtime::OutputCaps::DEFAULT,
            ));
            let outputs = p1_module_runtime::CallOutputs::new(store, Default::default());
            let linked = Services {
                process: Some(process),
                process_jobs: Some(jobs),
                tool_outputs: Some(Arc::new(outputs)),
                ..Services::default()
            };
            wasm_tool(
                &module,
                linked,
                ExecutionLimits::default(),
                &Arc::new(MaskCounter::new()),
            )
            .expect("the shell links with its process service and the store");
        },
    )
    .await;
}
