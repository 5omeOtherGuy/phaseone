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
use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolOutcome, ToolResultItem,
};
use p1_module_runtime::{
    ComponentEntry, ExecutionLimits, LoadError, LoadedModule, Loader, ManifestError, ModuleKind,
    ReleaseManifest, Services, wasm_tool,
};
use thiserror::Error;

use crate::HostDeps;
#[cfg(debug_assertions)]
use crate::run::write_stderr;

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
    /// A class this host cannot register: only tool packages have a catalog adapter, and
    /// policy packages are accepted as the session's host entries.
    #[error(
        "module `{module}` is a {kind} package; only tool and policy packages can be registered"
    )]
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
/// override lock can only select among what the release ships. A debug build has no install
/// to read, so when the share tree carries no manifest it falls back to the manifest
/// `scripts/build-modules.sh` writes beside the built packages (BLOCKERS S3-B6, D080), the
/// mirror of `main.rs`'s debug-only source-tree `environments/` fallback; a release binary
/// never does, because `cfg(debug_assertions)` is false there, so the official-source rule of
/// ADR-0079/ADR-0087 is unchanged. The choice is not logged here: this function holds no
/// [`HostDeps`], so [`register_locked_modules`] writes the one-line notice on the host's own
/// stderr channel, where the TUI's alternate screen and a test's captured stderr both see it.
pub fn official_release_manifest() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    let share = exe
        .parent()?
        .join("../share/p1/modules")
        .join(RELEASE_MANIFEST_FILE);
    // Compiled in, so the fallback is the checkout's own path, never a place an installation
    // could edit.
    let built = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules")
        .join(RELEASE_MANIFEST_FILE);
    Some(choose_release_manifest(share, built))
}

/// The manifest to load modules from: the share tree's when it is there, else — in a debug
/// build only — the built set's when the share tree has none, else the share path, so the
/// loader's error names the release it looked for.
fn choose_release_manifest(share: PathBuf, built: PathBuf) -> PathBuf {
    if share.is_file() {
        return share;
    }
    if cfg!(debug_assertions) && built.is_file() {
        return built;
    }
    share
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
///
/// A USER-selected provider package takes no catalog key at all: the lock keeps it under the
/// module name it gave it (`provider-anthropic`, `provider-openai`, `provider-openai-chat`,
/// the manifest name without the reserved `p1/` namespace, `docs/design/modules/package.md`)
/// and provider activation resolves that name in the same lock, so the module a user selected
/// serves the route in place of the release's host entry of that adapter — until #355's debug
/// discovery is on main, the release's own host entries stay the delivered path for the three
/// shipped providers (ANSWERS D083b).
pub fn register_modules(
    catalog: &mut Catalog,
    packages: Vec<ModulePackage>,
    services: ModuleServices,
) -> Result<(), ModulesError> {
    let existing = catalog.tool_keys();
    for package in packages {
        let kind = package.loaded.kind();
        match kind {
            ModuleKind::Tool => {}
            // Kept by module name in the lock the selection came from; never a tool here.
            ModuleKind::Provider => continue,
            // A policy package is the session's, not a catalog tool: the shipped policies are
            // official-release host entries the host loads by name (`policy.rs`,
            // `summary.rs`; D083b 2), so a lock selecting one registers no tool.
            ModuleKind::ContextPolicy | ModuleKind::AuthorizationPolicy => continue,
            // A class with no adapter here yet: a lock that selects one is refused, never
            // ignored.
            _ => {
                return Err(ModulesError::NotATool {
                    module: package.module,
                    kind: kind.name(),
                });
            }
        }
        if existing.contains(&package.module) {
            return Err(ModulesError::Collision {
                module: package.module,
                lock: package.lock,
            });
        }
        let key = package.module.clone();
        register_locked_entry(catalog, &key, Arc::new(package.loaded), services.clone());
    }
    Ok(())
}

/// Registers the verified module `loaded` under the catalog key `module`, built only when an
/// environment assembles the key, as [`register_modules`] registers a locked package. This is
/// the OFFICIAL-RELEASE HOST-ENTRY path (the worker and workflow members, S6.11, D083b): the
/// entries arrive with their fixed `worker_*`/`workflow_*` keys, verified against the release
/// manifest once per process and shared, so they arrive already loaded. An environment may give
/// a host entry a face (`name`, `description`, `variant`), exactly as it could the native member
/// the entry replaced.
pub fn register_host_entry(
    catalog: &mut Catalog,
    module: &str,
    loaded: Arc<LoadedModule>,
    services: ModuleServices,
) {
    register_entry(catalog, module, loaded, services, true);
}

/// Registers a package an installation selected through `modules.lock` ([`register_modules`]).
/// A locked package takes no face override: an extra module must never present a name,
/// description or variant other than its own, whatever `ToolSpec` an environment carries.
pub fn register_locked_entry(
    catalog: &mut Catalog,
    module: &str,
    loaded: Arc<LoadedModule>,
    services: ModuleServices,
) {
    register_entry(catalog, module, loaded, services, false);
}

/// Registers `loaded` under `module` for an environment to build, accepting a face override
/// only when `face` (a host entry) rather than refusing it (a locked package).
fn register_entry(
    catalog: &mut Catalog,
    module: &str,
    loaded: Arc<LoadedModule>,
    services: ModuleServices,
    face: bool,
) {
    let key = module.to_owned();
    catalog.tool(
        module,
        Box::new(move |spec: &ToolSpec, tool_services: &ToolServices| {
            instantiate(&key, &loaded, spec, &services, tool_services, face)
        }),
    );
}

/// Builds one agent's instance of the module tool `loaded`, registered as `module`.
fn instantiate(
    module: &str,
    loaded: &LoadedModule,
    spec: &ToolSpec,
    services: &ModuleServices,
    tool_services: &ToolServices,
    face: bool,
) -> Result<Arc<dyn Tool>, String> {
    let overridden = spec.name.is_some() || spec.description.is_some() || spec.variant.is_some();
    // `WasmTool` has no `ToolFace`; presenting a locked package under another face would need
    // one, and silently dropping the override would show the model a tool the environment did
    // not ask for. An official host entry is built by [`FacedEntry`] instead, below.
    if overridden && !face {
        return Err(format!(
            "module `{module}` cannot take a name, description or variant override"
        ));
    }
    // The turn's own counter is the assembling agent's, carried on `ToolServices`
    // (issue #142, one counter per agent): `wasm_tool` always wraps the module, and the
    // host's `assemble_with_cache_key` wraps the SAME counter around the assembled tools,
    // so a module tool's masking is what the turn's mask notice reports — never a
    // throwaway counter that always reads zero.
    let tool = wasm_tool(
        loaded,
        services(loaded.name(), tool_services),
        ExecutionLimits::default(),
        &tool_services.mask,
    )
    .map_err(|error| error.to_string())?;
    if !overridden {
        return Ok(tool);
    }
    let name = spec
        .name
        .clone()
        .unwrap_or_else(|| tool.declaration().name.clone());
    let description = spec
        .description
        .clone()
        .unwrap_or_else(|| tool.declaration().description.clone());
    let variant = spec
        .variant
        .clone()
        .unwrap_or_else(|| tool.identity().variant.clone());
    Ok(Arc::new(FacedEntry {
        declaration: ToolDeclaration {
            name,
            description,
            kind: tool.declaration().kind.clone(),
        },
        identity: ToolIdentity {
            implementation: tool.identity().implementation.clone(),
            variant,
        },
        inner: tool,
    }))
}

/// A host entry under the face an environment gave it: the component, its schema and its
/// semantics are unchanged, only the model-facing `name`, `description` and the identity
/// `variant` differ — what `apply_face!` gives a native tool. The native member registrations
/// this path replaced accepted those overrides, so a host entry must too (S6.11 review).
struct FacedEntry {
    inner: Arc<dyn Tool>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl Tool for FacedEntry {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        self.inner.execute(call, context)
    }
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
    // One line on the host's stderr, in a debug build only and once per process, so an
    // operator can see which module set a development binary loaded without a line per catalog
    // assembly. It goes through `write_stderr` (the injected channel), never `eprintln!`: a
    // line on the process's real stderr would land on the TUI's drawn screen, and a host test
    // that captures stderr would never see it.
    #[cfg(debug_assertions)]
    {
        static LOGGED: std::sync::Once = std::sync::Once::new();
        LOGGED.call_once(|| {
            write_stderr(
                deps,
                &format!(
                    "p1: debug build: loading modules from {}\n",
                    release.display()
                ),
            );
        });
    }
    let packages = load_locked_modules(&lock, &release).map_err(|error| error.to_string())?;
    // A member a lock selects is a member of its family all the same: it takes the host's
    // lists and grant check, as a host entry does, not only the family's scopes
    // (`catalog/delegation.rs`, D084).
    #[cfg(feature = "delegation")]
    let services = super::delegation::with_member_lists(
        locked_module_services(deps),
        super::delegation::worker_lists(catalog, deps)?,
    );
    #[cfg(not(feature = "delegation"))]
    let services = locked_module_services(deps);
    register_modules(catalog, packages, services).map_err(|error| error.to_string())
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

    #[test]
    fn the_debug_fallback_is_taken_only_when_the_share_tree_has_no_manifest() {
        let dir = tempfile::tempdir().expect("tempdir");
        let share = dir.path().join("share/p1/modules/manifest.json");
        let built = dir.path().join("modules/target/p1-modules/manifest.json");
        std::fs::create_dir_all(share.parent().expect("share parent")).expect("share dir");
        std::fs::create_dir_all(built.parent().expect("built parent")).expect("built dir");
        std::fs::write(&built, "{}\n").expect("write built manifest");

        // Only the built set exists: a debug build takes it, and a release build does not.
        #[cfg(debug_assertions)]
        assert_eq!(choose_release_manifest(share.clone(), built.clone()), built);
        #[cfg(not(debug_assertions))]
        assert_eq!(choose_release_manifest(share.clone(), built.clone()), share);

        // The share tree's manifest always wins, whichever else exists.
        std::fs::write(&share, "{}\n").expect("write share manifest");
        assert_eq!(choose_release_manifest(share.clone(), built.clone()), share);

        // Neither exists: the share path is returned, so the loader's error names the release.
        std::fs::remove_file(&share).expect("remove share manifest");
        std::fs::remove_file(&built).expect("remove built manifest");
        assert_eq!(choose_release_manifest(share.clone(), built), share);
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
