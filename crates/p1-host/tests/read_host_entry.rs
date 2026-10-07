//! S1.8.1: `read` is the release's `p1/read` host entry (D083b 2), not a compiled-in tool.
//!
//! The catalog a run builds registers the entry by loading the release manifest a debug build
//! and the tests see (D080: the set `scripts/build-modules.sh --all` writes), verifying the
//! package against it, and the assembly identity the host writes names `read` as a PACKAGE row —
//! the manifest name, the loader-verified digest and the ABI — never as a native module with a
//! `null` digest. No case touches the network, a credential file or a real home.

mod common;

use std::path::Path;
use std::sync::Arc;

use common::{Harness, provider_hook};
use p1_assembly::{EnvironmentFile, Substitutions, ToolSpec, assemble};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CancellationToken, ModelOptions, ToolCall, ToolContext, ToolInput};
use p1_host::activity::CompletionHub;
use p1_host::catalog::build_catalog;
use p1_host::catalog::modules::module_sources;
use p1_host::cli::SandboxMode;
use p1_host::run::assembly_identity;
use p1_testkit::ScriptedProvider;

/// The catalog key an environment selects the tool by, and the release package behind it.
const KEY: &str = "read";
const PACKAGE: &str = "p1/read";

/// The built module set's manifest, as `scripts/build-modules.sh --all` wrote it beside the
/// packages: the release this build loads (D080).
fn built_manifest() -> Value {
    let path =
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules/manifest.json");
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the built module manifest {} is missing ({error}): run scripts/build-modules.sh --all first",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("the built manifest is JSON")
}

/// The built release's entry for `name`.
fn entry_of(name: &str) -> Value {
    built_manifest()["components"]
        .as_array()
        .expect("components")
        .iter()
        .find(|entry| entry["name"] == json!(name))
        .unwrap_or_else(|| panic!("the built release holds no {name}"))
        .clone()
}

/// One environment naming `read` alone, over the harness's scripted provider.
fn read_environment() -> EnvironmentFile {
    EnvironmentFile {
        name: "read-host-entry".into(),
        family: "test".into(),
        provider: "fake".into(),
        model: "fake-model".into(),
        profile: None,
        profile_text: None,
        options: ModelOptions::default(),
        tools: vec![ToolSpec {
            module: KEY.into(),
            name: None,
            description: None,
            variant: None,
        }],
        prompt_template: "tools: {{tool_names}}".into(),
        context: None,
        summarize_prompt: None,
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_default_read_is_the_release_host_entry_in_the_assembly_identity() {
    // An environment tree with no `modules.lock`: the shipped shape, where nothing selects a
    // package for the key and the release's host entry is what runs.
    let environments = tempfile::tempdir().expect("environments dir");
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![(
        "fake",
        ScriptedProvider::new(Vec::new()),
    )]));
    let completion = Arc::new(CompletionHub::new());
    let catalog = build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &[], &completion)
        .expect("the release's p1/read host entry registers");

    // The key is registered, and no compiled-in tool carries it any more: the native
    // registration is gone, so the entry is the only thing that answers to `read`.
    assert!(
        catalog.tool_keys().contains(&KEY.to_string()),
        "read is registered: {:?}",
        catalog.tool_keys()
    );

    let workspace = tempfile::tempdir().expect("workspace");
    std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").expect("file");
    let assembled = assemble(
        &catalog,
        &read_environment(),
        workspace.path(),
        &Substitutions {
            workspace: "/work".into(),
            date: "2026-01-01".into(),
            os: "linux".into(),
            scratch: String::new(),
        },
    )
    .expect("read assembles as the release's host entry");

    // The assembled tool IS the component: its identity implementation is the package name,
    // which the native crate (`p1-tool-read`) never is.
    assert_eq!(assembled.resolved.tools[0].identity.implementation, PACKAGE);

    // The identity row is a PACKAGE row: the manifest name, the digest the loader verified in
    // bare hex, and the ABI of the release entry. A native row would carry a null digest.
    let entry = entry_of(PACKAGE);
    let sources = module_sources(&harness.deps).expect("the release's host entries resolve");
    let identity = assembly_identity(&assembled, "fake", false, &sources);
    let row = serde_json::to_value(&identity).expect("the identity serializes")["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["package"] == json!(KEY))
        .expect("the identity names the read key")
        .clone();
    assert_eq!(row["kind"], "tool");
    assert_eq!(row["name"], PACKAGE);
    assert_eq!(
        row["digest"],
        json!(
            entry["digest"]
                .as_str()
                .expect("the release entry's digest")
                .strip_prefix("sha256:")
                .expect("the manifest spelling")
        ),
        "the digest the release pins, in the bare spelling"
    );
    assert_eq!(
        row["abi"],
        json!(format!(
            "{}+{}",
            entry["world"].as_str().expect("world"),
            entry["protocol"].as_str().expect("protocol")
        ))
    );
    assert!(!row["digest"].is_null(), "{row}");
    // The compiled-in provider and the two policies stay native rows.
    let provider = serde_json::to_value(&identity).expect("the identity serializes")["modules"]
        .as_array()
        .expect("modules")
        .iter()
        .find(|module| module["package"] == json!("fake"))
        .expect("the provider row")
        .clone();
    assert!(provider["digest"].is_null(), "{provider}");

    // And the entry RUNS: the component reads the scratch file, so the registration is a working
    // tool and not only an identity row.
    let outcome = p1_module_tests::within_deadline(
        "read host entry",
        assembled.tools[0].execute(
            &ToolCall {
                call_id: "c1".into(),
                name: KEY.into(),
                input: ToolInput::Json(json!({ "file_path": "notes.txt" }).to_string()),
            },
            ToolContext {
                cancel: CancellationToken::new(),
            },
        ),
    )
    .await;
    let text = format!("{:?}", outcome.content);
    assert!(
        text.contains("alpha") && text.contains("beta"),
        "{:?}: {text}",
        outcome.status
    );
}
