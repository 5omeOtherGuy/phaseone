//! The assembly rule over module packages (ADR-0071, S1.4 BRIEF §3/§4): a module the
//! environment does not name is never instantiated and cannot dispatch, whether it is an
//! installed package the environment did not select or a name nobody ships.
//!
//! The package is the real fixture, verified and compiled by the loader and registered
//! through the host's catalog entry point. The tool adapter is a counting stand-in for
//! S0's `WasmTool`: a call of it is an instantiation, so "never instantiated" is a count.

use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{
    AssemblyError, Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolSpec,
    assemble,
};
use p1_contracts::{ModelOptions, Provider, Tool};
use p1_host::catalog::modules::{LoadError, Release, load_locked_modules, register_modules};
use p1_module_tests::{CountingAdapter, FIXTURE_PACKAGE, ScratchRelease, fixture, lock_entry};
use p1_testkit::{FakeTool, ScriptedProvider};

const PROVIDER: &str = "scripted";

fn environment(tools: &[&str]) -> EnvironmentFile {
    EnvironmentFile {
        name: "modules-test".into(),
        family: "test".into(),
        provider: PROVIDER.into(),
        model: "test-model".into(),
        profile: None,
        options: ModelOptions::default(),
        tools: tools
            .iter()
            .map(|module| ToolSpec {
                module: (*module).into(),
                name: None,
                description: None,
                variant: None,
            })
            .collect(),
        prompt_template: "tools: {{tool_names}}".into(),
        context: None,
        summarize_prompt: None,
    }
}

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
    }
}

/// The `modules.lock` text resolving `fixture` to the built fixture package.
fn fixture_lock() -> ModulesLock {
    let text = fixture_lock_text_for("fixture", "p1/fixture");
    ModulesLock::parse(&PathBuf::from("modules.lock"), &text).expect("fixture lock")
}

/// A catalog with a scripted provider, the compiled-in stand-in tool `read`, and every
/// package `lock` resolves in `release`, registered through the host's entry point.
fn catalog(release: &ScratchRelease, lock: &ModulesLock, adapter: &CountingAdapter) -> Catalog {
    let mut catalog = Catalog::new();
    let provider = ScriptedProvider::new(Vec::new());
    catalog.provider(
        PROVIDER,
        Box::new(move |_spec: &ProviderSpec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
    );
    catalog.tool(
        "read",
        Box::new(|_spec: &ToolSpec, _services: &p1_assembly::ToolServices| {
            Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>)
        }),
    );
    let modules = load_locked_modules(lock, &release.modules_dir).expect("the fixture loads");
    register_modules(&mut catalog, modules, adapter.adapter()).expect("registration");
    catalog
}

fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect()
}

#[test]
fn a_selected_package_is_instantiated_once_and_dispatches_under_its_loader_built_identity() {
    // Control for the two refusals below: selecting the module does reach the adapter.
    let release = ScratchRelease::with_fixture();
    let adapter = CountingAdapter::default();
    let catalog = catalog(&release, &fixture_lock(), &adapter);
    let workspace = tempfile::tempdir().unwrap();

    let assembled = assemble(
        &catalog,
        &environment(&["read", "fixture"]),
        workspace.path(),
        &substitutions(),
    )
    .expect("assembles");
    assert_eq!(adapter.calls(), 1);
    assert_eq!(tool_names(&assembled.tools), ["read", "fixture"]);
    let resolved = &assembled.resolved.tools[1];
    assert_eq!(resolved.module, "fixture");
    assert_eq!(resolved.identity.implementation, "p1/fixture");
    assert_eq!(resolved.identity.variant, "default");
}

#[test]
fn an_installed_but_unselected_package_cannot_dispatch() {
    let release = ScratchRelease::with_fixture();
    let adapter = CountingAdapter::default();
    let catalog = catalog(&release, &fixture_lock(), &adapter);
    assert!(
        catalog.tool_keys().contains(&"fixture".to_string()),
        "the package is installed and resolved"
    );
    let workspace = tempfile::tempdir().unwrap();

    let assembled = assemble(
        &catalog,
        &environment(&["read"]),
        workspace.path(),
        &substitutions(),
    )
    .expect("assembles");

    assert_eq!(
        adapter.calls(),
        0,
        "an unselected module is never instantiated"
    );
    assert_eq!(tool_names(&assembled.tools), ["read"]);
    assert!(
        assembled
            .resolved
            .tools
            .iter()
            .all(|tool| tool.identity.implementation != "p1/fixture"),
        "nothing of the package is assembled"
    );
    assert!(
        !assembled.system_prompt.contains("fixture"),
        "the prompt does not offer it: {}",
        assembled.system_prompt
    );
}

#[test]
fn an_installed_package_no_lock_resolves_cannot_dispatch() {
    // Installed in the release, but no lock names it: it is not even a catalog key, and
    // neither its module name nor its package name can be assembled.
    let release = ScratchRelease::with_fixture();
    let adapter = CountingAdapter::default();
    let catalog = catalog(&release, &ModulesLock::default(), &adapter);
    let workspace = tempfile::tempdir().unwrap();

    for name in ["fixture", "p1/fixture", FIXTURE_PACKAGE] {
        let error = assemble(
            &catalog,
            &environment(&["read", name]),
            workspace.path(),
            &substitutions(),
        )
        .expect_err("an unresolved package cannot be assembled");
        assert!(
            matches!(&error, AssemblyError::UnknownToolModule { module, .. } if module == name),
            "{name}: {error:?}"
        );
    }
    assert_eq!(adapter.calls(), 0);
}

#[test]
fn an_invented_name_cannot_dispatch() {
    let release = ScratchRelease::with_fixture();
    let adapter = CountingAdapter::default();
    let catalog = catalog(&release, &fixture_lock(), &adapter);
    let workspace = tempfile::tempdir().unwrap();

    let error = assemble(
        &catalog,
        &environment(&["read", "no_such_module"]),
        workspace.path(),
        &substitutions(),
    )
    .expect_err("an invented name is refused");
    match &error {
        AssemblyError::UnknownToolModule { module, available } => {
            assert_eq!(module, "no_such_module");
            assert!(available.contains(&"fixture".to_string()));
        }
        other => panic!("expected UnknownToolModule, got {other:?}"),
    }
    assert_eq!(
        adapter.calls(),
        0,
        "nothing is instantiated for a refused environment"
    );
}

#[test]
fn a_lock_resolving_an_invented_package_is_refused_before_registration() {
    // An override lock can only select what the release ships: a package name the
    // release does not have never becomes a catalog key.
    let release = ScratchRelease::with_fixture();
    let text = fixture_lock_text_for("ghost", "p1/ghost");
    let lock = ModulesLock::parse(&PathBuf::from("modules.lock"), &text).unwrap();
    let error = load_locked_modules(&lock, &release.modules_dir).expect_err("no such package");
    assert!(
        matches!(&error, LoadError::PackageNotFound { module, package, .. }
            if module == "ghost" && package == "p1/ghost"),
        "{error:?}"
    );
    // And the release itself has only the fixture.
    let opened = Release::open(&release.modules_dir).unwrap();
    assert_eq!(opened.package_names(), ["p1/fixture"]);
}

#[test]
fn a_package_cannot_replace_a_compiled_in_tool() {
    let release = ScratchRelease::with_fixture();
    let text = fixture_lock_text_for("read", "p1/fixture");
    let lock = ModulesLock::parse(&PathBuf::from("modules.lock"), &text).unwrap();
    let modules = load_locked_modules(&lock, &release.modules_dir).expect("loads");
    let mut catalog = Catalog::new();
    catalog.tool(
        "read",
        Box::new(|_spec: &ToolSpec, _services: &p1_assembly::ToolServices| {
            Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>)
        }),
    );
    let adapter = CountingAdapter::default();
    let error = register_modules(&mut catalog, modules, adapter.adapter())
        .expect_err("a collision is refused");
    assert!(error.contains("`read`"), "{error}");
}

/// A lock resolving `module` to `package` with the fixture's digest and ABI.
fn fixture_lock_text_for(module: &str, package: &str) -> String {
    let entry = lock_entry(&fixture().manifest);
    format!(
        "format = \"p1-modules-lock/1\"\n\n[modules.{module}]\npackage = \"{package}\"\n\
         version = \"{}\"\ndigest = \"{}\"\nworld = \"{}\"\nprotocol = \"{}\"\n",
        entry.version, entry.digest, entry.world, entry.protocol
    )
}
