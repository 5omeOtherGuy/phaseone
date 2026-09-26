//! The assembly rule over module packages (ADR-0071, S1.4): a module the environment does
//! not name is never instantiated and cannot dispatch, whether it is an installed package
//! the environment did not select or a name nobody ships.
//!
//! The package is the real fixture, verified and compiled by the runtime loader, registered
//! through the host's catalog entry point and built by `WasmTool` when assembled. The
//! registration's services hook runs once per instantiation, so "never instantiated" is a
//! count of its calls.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_assembly::{
    AssemblyError, Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions,
    ToolServices, ToolSpec, assemble, assemble_with_route_options,
};
use p1_contracts::{CancellationToken, ModelOptions, Provider, Tool, ToolContext, ToolStatus};
use p1_host::catalog::modules::{
    ModuleServices, ModulesError, load_locked_modules, register_modules,
};
use p1_module_runtime::{ProcessService, Services};
use p1_module_tests::{FIXTURE_NAME, FakeProcesses, Release, fake_processes, lock_text};
use p1_redact::MaskCounter;
use p1_testkit::{FakeTool, ScriptedProvider};

const PROVIDER: &str = "scripted";
/// The module name the fixture lock gives the fixture package.
const MODULE: &str = "fixture";

fn tool_spec(module: &str) -> ToolSpec {
    ToolSpec {
        module: module.into(),
        name: None,
        description: None,
        variant: None,
    }
}

fn environment(tools: Vec<ToolSpec>) -> EnvironmentFile {
    EnvironmentFile {
        name: "modules-test".into(),
        family: "test".into(),
        provider: PROVIDER.into(),
        model: "test-model".into(),
        profile: None,
        options: ModelOptions::default(),
        tools,
        prompt_template: "tools: {{tool_names}}".into(),
        context: None,
        summarize_prompt: None,
    }
}

fn environment_of(modules: &[&str]) -> EnvironmentFile {
    environment(modules.iter().map(|module| tool_spec(module)).collect())
}

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
    }
}

/// The lock resolving `module` to the fixture package of `release`.
fn fixture_lock(release: &Release, module: &str) -> ModulesLock {
    let entry = release.fixture_entry(FIXTURE_NAME);
    ModulesLock::parse(
        &release.root().join("modules.lock"),
        &lock_text(module, &entry),
    )
    .expect("fixture lock")
}

/// Counts instantiations: the registration calls its services hook once per instance.
struct Instantiations {
    count: Arc<AtomicUsize>,
    services: ModuleServices,
    /// The test's side of the fake `process` service, kept so the service stays usable.
    _processes: FakeProcesses,
}

impl Instantiations {
    fn new() -> Self {
        let count = Arc::new(AtomicUsize::new(0));
        let (process, processes) = fake_processes();
        let process: Arc<dyn ProcessService> = process;
        let counted = count.clone();
        let services: ModuleServices = Arc::new(move |_: &ToolServices| {
            counted.fetch_add(1, Ordering::SeqCst);
            Services {
                process: Some(process.clone()),
            }
        });
        Self {
            count,
            services,
            _processes: processes,
        }
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

/// A catalog with a scripted provider, the compiled-in stand-in tool `read`, and every
/// package `lock` resolves in `release`, registered through the host's entry point.
fn catalog(release: &Release, lock: &ModulesLock, instantiations: &Instantiations) -> Catalog {
    let mut catalog = Catalog::new();
    let provider = ScriptedProvider::new(Vec::new());
    catalog.provider(
        PROVIDER,
        Box::new(move |_spec: &ProviderSpec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
    );
    catalog.tool(
        "read",
        Box::new(|_spec: &ToolSpec, _services: &ToolServices| {
            Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>)
        }),
    );
    let packages = load_locked_modules(lock, &release.manifest_file()).expect("the fixture loads");
    register_modules(&mut catalog, packages, instantiations.services.clone())
        .expect("registration");
    catalog
}

fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect()
}

fn assemble_in(
    catalog: &Catalog,
    environment: &EnvironmentFile,
    workspace: &Path,
) -> Result<p1_assembly::Assembled, AssemblyError> {
    assemble(catalog, environment, workspace, &substitutions())
}

// `WasmTool` starts its executor on the current Tokio runtime, so the case that
// instantiates runs inside one, as the host's assembly does.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_selected_package_is_instantiated_once_under_its_loader_built_identity() {
    // Control for the refusals below: selecting the module does instantiate it.
    let release = Release::with_fixture();
    let instantiations = Instantiations::new();
    let catalog = catalog(&release, &fixture_lock(&release, MODULE), &instantiations);
    let workspace = tempfile::tempdir().unwrap();

    let assembled = assemble_in(
        &catalog,
        &environment_of(&["read", MODULE]),
        workspace.path(),
    )
    .expect("assembles");
    assert_eq!(instantiations.count(), 1);
    assert_eq!(tool_names(&assembled.tools), ["read", "fixture"]);
    let resolved = &assembled.resolved.tools[1];
    assert_eq!(resolved.module, MODULE);
    assert_eq!(resolved.identity.implementation, FIXTURE_NAME);
    assert_eq!(resolved.identity.variant, "default");
}

#[test]
fn an_installed_but_unselected_package_cannot_dispatch() {
    let release = Release::with_fixture();
    let instantiations = Instantiations::new();
    let catalog = catalog(&release, &fixture_lock(&release, MODULE), &instantiations);
    assert!(
        catalog.tool_keys().contains(&MODULE.to_owned()),
        "the package is installed and resolved"
    );
    let workspace = tempfile::tempdir().unwrap();

    let assembled =
        assemble_in(&catalog, &environment_of(&["read"]), workspace.path()).expect("assembles");

    assert_eq!(
        instantiations.count(),
        0,
        "an unselected module is never instantiated"
    );
    assert_eq!(tool_names(&assembled.tools), ["read"]);
    assert!(
        assembled
            .resolved
            .tools
            .iter()
            .all(|tool| tool.identity.implementation != FIXTURE_NAME),
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
    // neither a module name nor its package name can be assembled.
    let release = Release::with_fixture();
    let instantiations = Instantiations::new();
    let catalog = catalog(&release, &ModulesLock::default(), &instantiations);
    let workspace = tempfile::tempdir().unwrap();

    for name in [MODULE, FIXTURE_NAME] {
        let error = assemble_in(&catalog, &environment_of(&["read", name]), workspace.path())
            .expect_err("an unresolved package cannot be assembled");
        assert!(
            matches!(&error, AssemblyError::UnknownToolModule { module, .. } if module == name),
            "{name}: {error:?}"
        );
    }
    assert_eq!(instantiations.count(), 0);
}

#[test]
fn an_invented_name_cannot_dispatch() {
    let release = Release::with_fixture();
    let instantiations = Instantiations::new();
    let catalog = catalog(&release, &fixture_lock(&release, MODULE), &instantiations);
    let workspace = tempfile::tempdir().unwrap();

    let error = assemble_in(
        &catalog,
        &environment_of(&["read", "no_such_module"]),
        workspace.path(),
    )
    .expect_err("an invented name is refused");
    match &error {
        AssemblyError::UnknownToolModule { module, available } => {
            assert_eq!(module, "no_such_module");
            assert!(available.contains(&MODULE.to_owned()));
        }
        other => panic!("expected UnknownToolModule, got {other:?}"),
    }
    assert_eq!(
        instantiations.count(),
        0,
        "nothing is instantiated for a refused environment"
    );
}

#[test]
fn a_lock_resolving_an_invented_package_registers_nothing() {
    // An override lock can only select what the release ships: a package the release does
    // not have never becomes a catalog key.
    let release = Release::with_fixture();
    let mut entry = release.fixture_entry(FIXTURE_NAME);
    entry["name"] = "p1/ghost".into();
    let lock = ModulesLock::parse(
        &release.root().join("modules.lock"),
        &lock_text("ghost", &entry),
    )
    .unwrap();
    let error = match load_locked_modules(&lock, &release.manifest_file()) {
        Ok(_) => panic!("no such package"),
        Err(error) => error,
    };
    assert!(
        matches!(&error, ModulesError::Load { module, .. } if module == "ghost"),
        "{error:?}"
    );
}

#[test]
fn a_package_cannot_replace_a_compiled_in_tool() {
    let release = Release::with_fixture();
    let packages = load_locked_modules(&fixture_lock(&release, "read"), &release.manifest_file())
        .expect("loads");
    let mut catalog = Catalog::new();
    catalog.tool(
        "read",
        Box::new(|_spec: &ToolSpec, _services: &ToolServices| {
            Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>)
        }),
    );
    let instantiations = Instantiations::new();
    let error = register_modules(&mut catalog, packages, instantiations.services.clone())
        .expect_err("a collision is refused");
    assert!(
        matches!(&error, ModulesError::Collision { module, .. } if module == "read"),
        "{error:?}"
    );
}

#[test]
fn a_face_override_on_a_module_is_refused_before_instantiation() {
    let release = Release::with_fixture();
    let instantiations = Instantiations::new();
    let catalog = catalog(&release, &fixture_lock(&release, MODULE), &instantiations);
    let workspace = tempfile::tempdir().unwrap();
    let mut renamed = tool_spec(MODULE);
    renamed.name = Some("renamed".into());

    let error = assemble_in(&catalog, &environment(vec![renamed]), workspace.path())
        .expect_err("WasmTool has no face to apply");
    assert!(error.to_string().contains("override"), "{error}");
    assert_eq!(instantiations.count(), 0);
}

// Issue #142/S1.4: the counter a module tool masks into is the assembling agent's OWN
// (`ToolServices::mask`), the one the turn's notice reads — not a throwaway the factory
// created and left behind. `wasm_tool` always wraps the module; the counter it binds must
// be the caller's, so its `take` sees what the module masked.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_module_tools_masking_is_counted_in_the_agents_own_counter() {
    let release = Release::with_fixture();
    let instantiations = Instantiations::new();
    let catalog = catalog(&release, &fixture_lock(&release, MODULE), &instantiations);
    let workspace = tempfile::tempdir().unwrap();
    // The one counter the assembling agent would report through.
    let mask = Arc::new(MaskCounter::new());

    let assembled = assemble_with_route_options(
        &catalog,
        &environment_of(&["read", MODULE]),
        workspace.path(),
        &substitutions(),
        &mask,
        |_| ModelOptions::default(),
    )
    .expect("assembles");
    let tool = assembled
        .tools
        .iter()
        .find(|tool| tool.declaration().name == "fixture")
        .expect("the selected module is assembled")
        .clone();

    // A key-shaped value built at runtime, the shape p1-redact's own tests use.
    let secret = format!("sk-{}", "A".repeat(24));
    let outcome = tool
        .execute(
            &p1_module_tests::call(&format!("echo:{secret}")),
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;

    assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
    assert!(!outcome.content.contains(&secret));
    assert_eq!(outcome.content, "<redacted:sk-:24 chars>");
    assert_eq!(
        mask.take(),
        1,
        "the agent's counter carries the module tool's masking"
    );
    assert_eq!(mask.take(), 0);
}
