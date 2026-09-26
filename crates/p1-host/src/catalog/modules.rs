//! The WebAssembly package loader's catalog entry point (ADR-0071, ADR draft "Module
//! identity and verified loading").
//!
//! An environment names a module by its `[[tools]] module` key. A key no compiled-in tool
//! claims is resolved through `modules.lock` ([`p1_assembly::load_modules_lock`]) to one
//! package of p1's release manifest. Verifying and compiling the package is
//! [`p1_module_runtime::Loader`]'s (official source, kind, world, protocol, digest, grants);
//! this file adds only what the runtime cannot know:
//! - the lock entry must pin what the release ships: its digest, world and protocol are
//!   compared with the release manifest's entry, so an override lock cannot silently select
//!   other bytes or another ABI than it names;
//! - the granted capabilities must lie inside the class allocation of
//!   `modules/capabilities.toml` (freeze item 13);
//! - registration builds no instance: [`p1_module_runtime::wasm_tool`] runs only in the
//!   catalog factory, i.e. only when an environment assembles the key, so an installed but
//!   unselected package and an invented name both never dispatch (the assembly rule).
//!
//! Two components claiming one name or one digest are refused when the release manifest is
//! read ([`ManifestError::DuplicateIdentity`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_assembly::{
    Catalog, LockedModule, LockedProtocol, ModulesLock, ModulesLockError, ToolServices, ToolSpec,
    load_modules_lock,
};
use p1_contracts::Tool;
use p1_module_runtime::{
    ComponentEntry, ExecutionLimits, LoadError, LoadedModule, Loader, ManifestError, ModuleKind,
    ReleaseManifest, Services, wasm_tool,
};
use thiserror::Error;

use crate::HostDeps;

/// The release manifest's file name inside the module set (ADR-0079).
pub const RELEASE_MANIFEST_FILE: &str = "manifest.json";

/// The frozen per-class capability allocation (freeze item 13). Compiled in, so the host
/// checks against the table the boundary was frozen with, not a file an installation could
/// edit.
const ALLOCATION: &str = include_str!("../../../../modules/capabilities.toml");

/// Builds the capability services one instance of a module tool is linked with, from the
/// agent's own services. Called once per instantiation, so only when an environment
/// assembles the module. The first argument is the package's verified manifest name (its
/// module id, e.g. `p1/worker-start`), never the lock key an installation chose: a hook
/// that serves some members differently (the worker and workflow families, B-S6-9, D068)
/// must key on the identity the loader checked.
pub type ModuleServices = Arc<dyn Fn(&str, &ToolServices) -> Services + Send + Sync>;

/// Why the locked modules could not be loaded or registered. Each refusal has its own
/// variant; the runtime's refusals keep theirs inside [`ModulesError::Load`] and
/// [`ModulesError::Release`]. The runtime's errors are boxed: they are large, and every
/// `Result` of this file would otherwise carry their size.
#[derive(Debug, Error)]
pub enum ModulesError {
    /// A `modules.lock` could not be read.
    #[error(transparent)]
    Lock(#[from] ModulesLockError),
    /// The installed module set cannot be located.
    #[error("cannot locate p1's release module set: the executable path is unknown")]
    NoRelease,
    /// The release manifest could not be read, or claims one identity twice.
    #[error("{}: {source}", path.display())]
    Release {
        /// The release manifest file.
        path: PathBuf,
        /// Why.
        source: Box<ManifestError>,
    },
    /// The module runtime could not start (engine or epoch thread).
    #[error("cannot start the module runtime: {0}")]
    Runtime(Box<LoadError>),
    /// The runtime loader refused the package the lock selects.
    #[error("module `{module}` selected by {}: {source}", lock.display())]
    Load {
        /// The module name (the catalog key).
        module: String,
        /// The lock file that selected the package: the source of the selection.
        lock: PathBuf,
        /// The loader's refusal.
        source: Box<LoadError>,
    },
    /// The lock entry pins something other than what the release ships.
    #[error(
        "module `{module}` selected by {}: the lock's {field} is {locked}, \
         the release manifest has {released}",
        lock.display()
    )]
    LockMismatch {
        /// The module name.
        module: String,
        /// The lock file.
        lock: PathBuf,
        /// `digest`, `world` or `protocol`.
        field: &'static str,
        /// The lock's value.
        locked: String,
        /// The release manifest's value.
        released: String,
    },
    /// The manifest grants a capability its class is not allocated.
    #[error(
        "module `{module}`: {package} grants {capability}, which the {kind} class is not allocated"
    )]
    CapabilityNotAllocated {
        /// The module name.
        module: String,
        /// The package.
        package: String,
        /// The class as written.
        kind: String,
        /// The capability.
        capability: String,
    },
    /// Only tool packages have an adapter to register.
    #[error("module `{module}` is a {kind} package; only tool packages can be registered")]
    NotATool {
        /// The module name.
        module: String,
        /// Its class.
        kind: &'static str,
    },
    /// A module name that a compiled-in tool already has.
    #[error(
        "module `{module}` selected by {} collides with a compiled-in tool of the same name",
        lock.display()
    )]
    Collision {
        /// The module name.
        module: String,
        /// The lock file.
        lock: PathBuf,
    },
}

/// One verified, compiled package and the module name the lock gave it.
pub struct ModulePackage {
    /// The catalog key.
    pub module: String,
    /// The lock file that selected it.
    pub lock: PathBuf,
    /// The runtime's verified module.
    pub loaded: LoadedModule,
}

/// Where p1's own release keeps its module set: `<exe dir>/../share/p1/modules/manifest.json`
/// (ADR-0079), next to the shipped `environments/`. Never a configuration directory, so an
/// override lock can only select among what the release ships.
pub fn official_release_manifest() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(
        exe.parent()?
            .join("../share/p1/modules")
            .join(RELEASE_MANIFEST_FILE),
    )
}

/// Verifies and compiles every package `lock` resolves, from the release whose manifest is
/// `release_manifest`. Stops at the first refusal.
pub fn load_locked_modules(
    lock: &ModulesLock,
    release_manifest: &Path,
) -> Result<Vec<ModulePackage>, ModulesError> {
    let release_error = |source| ModulesError::Release {
        path: release_manifest.to_owned(),
        source: Box::new(source),
    };
    let manifest = ReleaseManifest::read(release_manifest).map_err(release_error)?;
    manifest.check_unique_digests().map_err(release_error)?;
    let mut packages = Vec::new();
    // The loader starts an epoch thread; a release nothing selects needs none.
    if lock.is_empty() {
        return Ok(packages);
    }
    let root = release_manifest.parent().unwrap_or(Path::new("."));
    let loader = Loader::new(manifest.clone(), root)
        .map_err(|error| ModulesError::Runtime(Box::new(error)))?;
    for (module, locked) in lock.iter() {
        if let Some(entry) = manifest.entry(&locked.package) {
            check_lock(module, locked, entry)?;
            check_allocation(module, entry)?;
        }
        // A package the manifest lacks is the loader's refusal to report: official source.
        let loaded = loader
            .load(&locked.package)
            .map_err(|source| ModulesError::Load {
                module: module.to_owned(),
                lock: locked.source.clone(),
                source: Box::new(source),
            })?;
        packages.push(ModulePackage {
            module: module.to_owned(),
            lock: locked.source.clone(),
            loaded,
        });
    }
    Ok(packages)
}

/// The lock pins the release's digest, world and protocol, or the selection is refused.
fn check_lock(
    module: &str,
    locked: &LockedModule,
    entry: &ComponentEntry,
) -> Result<(), ModulesError> {
    let mismatch = |field, locked_value: String, released: String| ModulesError::LockMismatch {
        module: module.to_owned(),
        lock: locked.source.clone(),
        field,
        locked: locked_value,
        released,
    };
    let digest = entry.digest.to_string();
    if locked.digest != digest {
        return Err(mismatch("digest", locked.digest.clone(), digest));
    }
    if locked.world != entry.world {
        return Err(mismatch("world", locked.world.clone(), entry.world.clone()));
    }
    if LockedProtocol::parse(&entry.protocol) != Some(locked.protocol) {
        return Err(mismatch(
            "protocol",
            locked.protocol.to_string(),
            entry.protocol.clone(),
        ));
    }
    Ok(())
}

/// Every granted capability lies inside the class allocation. An unknown class is left to
/// the loader, which refuses it with its own error.
fn check_allocation(module: &str, entry: &ComponentEntry) -> Result<(), ModulesError> {
    let Some(allocated) = allocation(&entry.kind) else {
        return Ok(());
    };
    match entry
        .capabilities
        .iter()
        .find(|capability| !allocated.contains(capability))
    {
        Some(capability) => Err(ModulesError::CapabilityNotAllocated {
            module: module.to_owned(),
            package: entry.name.clone(),
            kind: entry.kind.clone(),
            capability: capability.clone(),
        }),
        None => Ok(()),
    }
}

/// The interfaces class `kind` may import, or `None` for a class the table does not list.
fn allocation(kind: &str) -> Option<Vec<String>> {
    let table: toml::Table = toml::from_str(ALLOCATION).expect("the frozen allocation is TOML");
    let imports = table.get(kind)?.get("imports")?.as_array()?;
    Some(
        imports
            .iter()
            .filter_map(|import| import.as_str().map(str::to_owned))
            .collect(),
    )
}

/// Registers each verified tool package under its module name. A name a registered tool
/// already has is refused: a package never silently replaces a compiled-in tool.
pub fn register_modules(
    catalog: &mut Catalog,
    packages: Vec<ModulePackage>,
    services: ModuleServices,
) -> Result<(), ModulesError> {
    let existing = catalog.tool_keys();
    for package in packages {
        let kind = package.loaded.kind();
        if kind != ModuleKind::Tool {
            // Provider and policy packages need S4's and S5's adapters, which the runtime
            // does not have yet; a lock that selects one is refused, never ignored.
            return Err(ModulesError::NotATool {
                module: package.module,
                kind: kind.name(),
            });
        }
        if existing.contains(&package.module) {
            return Err(ModulesError::Collision {
                module: package.module,
                lock: package.lock,
            });
        }
        let key = package.module.clone();
        let services = services.clone();
        let package = Arc::new(package);
        catalog.tool(
            &key,
            Box::new(move |spec: &ToolSpec, tool_services: &ToolServices| {
                instantiate(&package, spec, &services, tool_services)
            }),
        );
    }
    Ok(())
}

/// Builds one agent's instance of a module tool.
fn instantiate(
    package: &ModulePackage,
    spec: &ToolSpec,
    services: &ModuleServices,
    tool_services: &ToolServices,
) -> Result<Arc<dyn Tool>, String> {
    // `WasmTool` has no `ToolFace`; presenting a module under another face would need one,
    // and silently dropping the override would show the model a tool the environment did
    // not ask for.
    if spec.name.is_some() || spec.description.is_some() || spec.variant.is_some() {
        return Err(format!(
            "module `{}` cannot take a name, description or variant override",
            package.module
        ));
    }
    // The turn's own counter is the assembling agent's, carried on `ToolServices`
    // (issue #142, one counter per agent): `wasm_tool` always wraps the module, and the
    // host's `assemble_with_cache_key` wraps the SAME counter around the assembled tools,
    // so a module tool's masking is what the turn's mask notice reports — never a
    // throwaway counter that always reads zero.
    wasm_tool(
        &package.loaded,
        services(package.loaded.name(), tool_services),
        ExecutionLimits::default(),
        &tool_services.mask,
    )
    .map_err(|error| error.to_string())
}

/// The catalog build path's step: resolve the lock files next to the environment
/// directories, and load and register what they name. An empty lock (the shipped one
/// today) loads nothing and needs no release.
pub(super) fn register_locked_modules(
    catalog: &mut Catalog,
    deps: &HostDeps,
) -> Result<(), String> {
    register_locked_modules_from(catalog, deps, official_release_manifest())
}

/// [`register_locked_modules`] over the release whose manifest is `release`.
fn register_locked_modules_from(
    catalog: &mut Catalog,
    deps: &HostDeps,
    release: Option<PathBuf>,
) -> Result<(), String> {
    let lock = load_modules_lock(&deps.environment_dirs).map_err(|error| error.to_string())?;
    if lock.is_empty() {
        return Ok(());
    }
    let release = release.ok_or_else(|| ModulesError::NoRelease.to_string())?;
    let packages = load_locked_modules(&lock, &release).map_err(|error| error.to_string())?;
    register_modules(catalog, packages, locked_module_services(deps))
        .map_err(|error| error.to_string())
}

/// The hook every locked package is linked with. The base is the agent's own: the read
/// side of its workspace and its observations (`super::tools::module_services`, S1.8), for
/// every module. The worker and workflow families install `deps.module_services` for their
/// own members (`catalog/delegation.rs`, `catalog/workflow.rs`; B-S6-9, D068); their hooks
/// give every module they do not serve `Services::default()`, so the base fills the
/// `workspace` and `snapshot` a family hook left empty, and a module that is no member (the
/// `p1/read` component) links exactly as without the families. No other native service
/// backs a module capability in the host yet (the shell's process service is not bridged
/// to the runtime's `ProcessService`), so a package granted one fails its assembly with the
/// runtime's `MissingService` rather than running unlinked.
fn locked_module_services(deps: &HostDeps) -> ModuleServices {
    let base = super::tools::module_services(deps);
    let Some(family) = deps.module_services.clone() else {
        return base;
    };
    Arc::new(move |module: &str, services: &ToolServices| {
        let mut linked = family(module, services);
        if linked.workspace.is_none() || linked.snapshot.is_none() {
            let base = base(module, services);
            linked.workspace = linked.workspace.or(base.workspace);
            linked.snapshot = linked.snapshot.or(base.snapshot);
        }
        linked
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_allocation_is_the_frozen_table() {
        let tool = allocation("tool").expect("tool class");
        for capability in ["control", "clock", "random", "process", "workers-start"] {
            assert!(tool.iter().any(|c| c == capability), "{capability}");
        }
        // The worker capabilities are three grants, never one (freeze item 13).
        assert!(!tool.iter().any(|c| c == "workers"));
        let provider = allocation("provider").expect("provider class");
        assert!(!provider.iter().any(|c| c == "process"));
        assert!(allocation("plugin").is_none());
    }

    /// Nothing interrupts the catalog cases.
    struct NoInterrupt;

    impl crate::InterruptSource for NoInterrupt {
        fn recv<'a>(
            &'a self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }

    /// Host dependencies over `environment_dirs` that touch no terminal, network or home.
    fn quiet_deps(environment_dirs: Vec<PathBuf>) -> HostDeps {
        let sink = || -> crate::SharedWriter {
            Arc::new(std::sync::Mutex::new(Box::new(std::io::sink())))
        };
        let mut deps = HostDeps::new(
            sink(),
            sink(),
            Arc::new(crate::ReaderLines::from_reader(tokio::io::empty())),
            Arc::new(p1_provider_http::testing::ScriptedTransport::new(Vec::new())),
            "2026-01-02".to_string(),
            Arc::new(NoInterrupt),
            environment_dirs,
            false,
        );
        deps.home = None;
        deps
    }

    /// S1-N12: with a family hook installed that serves only a worker member and gives every
    /// other module `Services::default()` (as `catalog/delegation.rs` installs it), a lock
    /// that selects `p1/read` still builds the component with `workspace` and `snapshot`
    /// linked through `register_locked_modules`, and a read returns the file. The same hook
    /// alone, without the base, is the `MissingService` this merge must not reintroduce.
    #[tokio::test]
    async fn the_read_component_resolves_with_a_delegation_hook_installed() {
        use p1_assembly::{EnvironmentFile, ProviderSpec, Substitutions, assemble};
        use p1_contracts::serde_json::{Value, json};
        use p1_contracts::{
            CancellationToken, ModelOptions, Provider, ToolCall, ToolContext, ToolInput,
        };

        const PACKAGE: &str = "p1-module-read";
        let built = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules")
            .join(PACKAGE);
        let read = |path: PathBuf| {
            std::fs::read(&path).unwrap_or_else(|error| {
                panic!(
                    "the {PACKAGE} artifact {} is missing ({error}): run scripts/build-modules.sh first",
                    path.display()
                )
            })
        };
        let wasm = read(built.join(format!("{PACKAGE}.wasm")));
        let manifest: Value = p1_contracts::serde_json::from_slice(&read(
            built.join(format!("{PACKAGE}.manifest.json")),
        ))
        .expect("the package manifest is JSON");
        let entry = json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("packages/{PACKAGE}/{PACKAGE}.wasm"),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        });

        // The release, and a lock next to the environments directory selecting `p1/read`
        // under the key `read`.
        let release = tempfile::tempdir().expect("release dir");
        let component = release.path().join(entry["path"].as_str().expect("path"));
        std::fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
        std::fs::write(&component, &wasm).expect("component");
        let release_manifest = release.path().join(RELEASE_MANIFEST_FILE);
        std::fs::write(
            &release_manifest,
            json!({ "format": "p1-release-manifest/1", "components": [entry.clone()] }).to_string(),
        )
        .expect("release manifest");
        let config = tempfile::tempdir().expect("config dir");
        let environments = config.path().join("environments");
        std::fs::create_dir_all(&environments).expect("environments dir");
        std::fs::write(
            config.path().join("modules.lock"),
            p1_module_tests::lock_text("read", &entry),
        )
        .expect("lock");

        // The family hook: a worker member gets its services, every other module none.
        let asked = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let recorded = asked.clone();
        let family: ModuleServices = Arc::new(move |module: &str, _: &ToolServices| {
            recorded.lock().unwrap().push(module.to_owned());
            match module {
                "p1/worker-start" => Services {
                    workers: Some(Default::default()),
                    ..Services::default()
                },
                _ => Services::default(),
            }
        });
        let mut deps = quiet_deps(vec![environments]);
        deps.module_services = Some(family.clone());

        let workspace = tempfile::tempdir().expect("workspace");
        std::fs::write(workspace.path().join("notes.txt"), "alpha\nbeta\n").expect("file");
        let environment = EnvironmentFile {
            name: "read-module".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "test-model".into(),
            profile: None,
            options: ModelOptions::default(),
            tools: vec![ToolSpec {
                module: "read".into(),
                name: None,
                description: None,
                variant: None,
            }],
            prompt_template: "tools: {{tool_names}}".into(),
            context: None,
            summarize_prompt: None,
        };
        let substitutions = Substitutions {
            workspace: "/work".into(),
            date: "2026-01-01".into(),
            os: "linux".into(),
        };
        let catalog_with = |register: &dyn Fn(&mut Catalog) -> Result<(), String>| {
            let mut catalog = Catalog::new();
            let provider = p1_testkit::ScriptedProvider::new(Vec::new());
            catalog.provider(
                "scripted",
                Box::new(move |_spec: &ProviderSpec| {
                    Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)
                }),
            );
            register(&mut catalog).expect("registration");
            catalog
        };

        let catalog = catalog_with(&|catalog| {
            register_locked_modules_from(catalog, &deps, Some(release_manifest.clone()))
        });
        let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions)
            .unwrap_or_else(|error| panic!("p1/read assembles under the family hook: {error}"));
        assert_eq!(asked.lock().unwrap().as_slice(), ["p1/read"]);
        let tool = &assembled.tools[0];
        assert_eq!(
            assembled.resolved.tools[0].identity.implementation,
            "p1/read"
        );
        let outcome = p1_module_tests::within_deadline(
            "read under a family hook",
            tool.execute(
                &ToolCall {
                    call_id: "c1".into(),
                    name: "read".into(),
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

        // The family hook alone leaves `workspace` and `snapshot` unlinked.
        let bare = catalog_with(&|catalog| {
            let packages = load_locked_modules(
                &load_modules_lock(&deps.environment_dirs).expect("lock"),
                &release_manifest,
            )
            .expect("p1/read loads");
            register_modules(catalog, packages, family.clone()).map_err(|error| error.to_string())
        });
        let refused = assemble(&bare, &environment, workspace.path(), &substitutions)
            .expect_err("the family hook alone cannot link p1/read");
        assert!(refused.to_string().contains("workspace"), "{refused}");
    }
}
