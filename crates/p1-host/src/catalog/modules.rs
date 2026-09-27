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
//!
//! An official-release HOST ENTRY ([`HOST_ENTRIES`], [`register_host_entries`], D083b 2) is a
//! package of the same release that a compiled-in registration once carried: its catalog key is
//! fixed by the host, not by an environment or a lock, and it is loaded from the release
//! manifest and verified against it exactly as a lock-selected package is. A release that does
//! not hold it, or does not verify it, fails the catalog build naming the package — never a
//! silent fallback to a native tool.

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

/// The official-release host entries this host registers (D083b 2), as `(catalog key, package)`.
///
/// The key is what an environment selects the entry by and what an assembly identity names as
/// the module's `package`; the package name is what the release must ship. `read` is the one
/// tool entry: its native registration is gone (S1.8.1), and the release's `p1/read` component
/// is what a build without a user lock assembles. A user lock that names a key here still wins
/// ([`lock_selects`]), and S5.11's policy entries are one more list passed to the same step.
pub const HOST_ENTRIES: [(&str, &str); 1] = [("read", "p1/read")];

/// Whether a `modules.lock` beside `environment_dirs` names `key`. A lock that cannot be read
/// selects nothing here; the locked-module registration reports its error.
pub(super) fn lock_selects(environment_dirs: &[PathBuf], key: &str) -> bool {
    load_modules_lock(environment_dirs)
        .is_ok_and(|lock| lock.iter().any(|(module, _)| module == key))
}

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
    /// A host entry's release manifest could not be read, or claims one identity twice.
    ///
    /// A host entry is not selectable: a release that cannot be read, does not hold the package,
    /// or does not verify it must fail the catalog build, and the message always names both the
    /// catalog key and the release package, so the refusal is never read as a missing native
    /// tool.
    #[error("host entry `{module}` (release package `{package}`) from {}: {source}", path.display())]
    HostEntryRelease {
        /// The catalog key the entry registers under.
        module: String,
        /// The package the release must hold.
        package: String,
        /// The release manifest.
        path: PathBuf,
        /// Why the manifest could not be used.
        source: Box<ManifestError>,
    },
    /// The release refuses the package a host entry names: it is not there, its bytes are not
    /// the ones the manifest pins, or the runtime cannot speak it.
    #[error("host entry `{module}` (release package `{package}`) from {}: {source}", path.display())]
    HostEntryLoad {
        /// The catalog key the entry registers under.
        module: String,
        /// The package the release must hold.
        package: String,
        /// The release manifest.
        path: PathBuf,
        /// The loader's refusal.
        source: Box<LoadError>,
    },
    /// No release module set could be located for the host entries at all (the executable path
    /// is unknown), so the packages they need can be loaded from nowhere.
    #[error(
        "host entry `{module}` (release package `{package}`): cannot locate p1's release module set"
    )]
    HostEntryNoRelease {
        /// The catalog key the entry registers under.
        module: String,
        /// The package the release must hold.
        package: String,
    },
}

/// One verified, compiled package and the module name the lock gave it.
pub struct ModulePackage {
    /// The catalog key.
    pub module: String,
    /// What selected the package: the lock file, or the release manifest for a host entry.
    pub lock: PathBuf,
    /// The runtime's verified module.
    pub loaded: LoadedModule,
}

/// What an assembly identity needs of one package it names (ADR-0080): the manifest name, the
/// version the release pins, the digest the loader verified in the manifest's `sha256:` spelling
/// and the `<world>+<protocol>` ABI. A `modules.lock` resolution and a release host entry both
/// resolve to this, so the identity builder never reads either source itself.
#[derive(Clone)]
pub struct PackageIdentity {
    /// The package's manifest name, `<namespace>/<name>`.
    pub name: String,
    /// The release version the package runs at.
    pub version: String,
    /// `sha256:<64 lowercase hex>`, the digest of the package's `.wasm`.
    pub digest: String,
    /// `<world>+<protocol>`, the ABI the package speaks.
    pub abi: String,
}

/// Where p1's own release keeps its module set: `<exe dir>/../share/p1/modules/manifest.json`
/// (ADR-0079), next to the shipped `environments/`. Never a configuration directory, so an
/// override lock can only select among what the release ships. A debug build has no install
/// to read, so when the share tree carries no manifest it falls back to the manifest
/// `scripts/build-modules.sh` writes beside the built packages (BLOCKERS S3-B6, D080), the
/// mirror of `main.rs`'s debug-only source-tree `environments/` fallback; a release binary
/// never does, because `cfg(debug_assertions)` is false there, so the official-source rule of
/// ADR-0079/ADR-0087 is unchanged. The choice is not logged here: this function holds no
/// [`HostDeps`], so [`register_locked_modules`] and [`register_host_entries`] write the one-line
/// notice on the host's own stderr channel, where the TUI's alternate screen and a test's
/// captured stderr both see it.
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

/// Loads and verifies the release's `package` for the catalog key `module`, with the steps
/// [`load_locked_modules`] takes: read the manifest, refuse duplicate identities, parse the
/// ABI, check the class allocation, start the loader and load the package by name.
fn load_host_entry(
    module: &str,
    package: &str,
    release: &Path,
) -> Result<ModulePackage, ModulesError> {
    let manifest = read_release(module, package, release)?;
    if let Some(entry) = manifest.entry(package) {
        check_allocation(module, entry)?;
    }
    // A package the manifest lacks is the loader's refusal to report: official source.
    let root = release.parent().unwrap_or(Path::new("."));
    let loader =
        Loader::new(manifest, root).map_err(|error| ModulesError::Runtime(Box::new(error)))?;
    let loaded = loader
        .load(package)
        .map_err(|source| ModulesError::HostEntryLoad {
            module: module.to_owned(),
            package: package.to_owned(),
            path: release.to_owned(),
            source: Box::new(source),
        })?;
    // The registration refuses a class with no catalog adapter (`register_modules`), so a
    // release that ships something other than a tool under this name fails naming the key.
    Ok(ModulePackage {
        module: module.to_owned(),
        lock: release.to_owned(),
        loaded,
    })
}

/// Reads the release manifest at `release` and refuses one that claims an identity twice: the
/// step both the host-entry registration and an assembly identity's package rows take.
fn read_release(
    module: &str,
    package: &str,
    release: &Path,
) -> Result<ReleaseManifest, ModulesError> {
    let release_error = |source| ModulesError::HostEntryRelease {
        module: module.to_owned(),
        package: package.to_owned(),
        path: release.to_owned(),
        source: Box::new(source),
    };
    let manifest = ReleaseManifest::read(release).map_err(release_error)?;
    manifest.check_unique_digests().map_err(release_error)?;
    Ok(manifest)
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
    announce_release(deps, &release);
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

/// The catalog build path's step for the official-release host entries (D083b 2,
/// [`HOST_ENTRIES`]): each entry no user lock selects is loaded from the release, verified and
/// registered through the same path a lock-selected package takes. A refusal is an error naming
/// the package and the release — never a native fallback.
pub(super) fn register_host_entries(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    register_host_entries_from(catalog, deps, &HOST_ENTRIES, official_release_manifest())
}

/// [`register_host_entries`] over `entries`, loaded from the release whose manifest is
/// `release`. A slice that adds host entries (S5.11's policy packages) calls this with its own
/// list.
pub(crate) fn register_host_entries_from(
    catalog: &mut Catalog,
    deps: &HostDeps,
    entries: &[(&str, &str)],
    release: Option<PathBuf>,
) -> Result<(), String> {
    let mut packages = Vec::new();
    for (key, package) in entries {
        if lock_selects(&deps.environment_dirs, key) {
            // A user lock names this key: its package is what the locked-module registration
            // registers. The release entry must not be registered as well, or the two would
            // collide.
            continue;
        }
        let release = release.as_deref().ok_or_else(|| {
            ModulesError::HostEntryNoRelease {
                module: (*key).to_owned(),
                package: (*package).to_owned(),
            }
            .to_string()
        })?;
        // The release is loaded, so a debug build says so exactly as the locked path does.
        announce_release(deps, release);
        packages.push(load_host_entry(key, package, release).map_err(|error| error.to_string())?);
    }
    if packages.is_empty() {
        return Ok(());
    }
    register_modules(catalog, packages, locked_module_services(deps))
        .map_err(|error| error.to_string())
}

/// The package sources an assembly identity names (ADR-0080): the `modules.lock` the catalog's
/// module registration read, and the official-release host entries it registered (D083b 2). A
/// key is resolved by the lock first — a user lock's package is what runs — then by a host
/// entry; a key neither names is a native module.
#[derive(Clone)]
pub struct ModuleSources {
    lock: ModulesLock,
    host: Vec<(String, PackageIdentity)>,
}

impl ModuleSources {
    /// Only the lock's resolutions: an assembly of a release without host entries, and the
    /// journal-identity cases that drive the identity builder over a fixture lock.
    pub fn of_lock(lock: ModulesLock) -> Self {
        Self {
            lock,
            host: Vec::new(),
        }
    }

    /// What `key` resolves to, or `None` for a native module.
    pub fn resolve(&self, key: &str) -> Option<PackageIdentity> {
        if let Some(locked) = self.lock.resolve(key) {
            return Some(PackageIdentity {
                name: locked.package.clone(),
                version: locked.version.clone(),
                digest: locked.digest.clone(),
                abi: format!("{}+{}", locked.world, locked.protocol),
            });
        }
        self.host
            .iter()
            .find(|(entry, _)| entry == key)
            .map(|(_, package)| PackageIdentity {
                name: package.name.clone(),
                version: package.version.clone(),
                digest: package.digest.clone(),
                abi: package.abi.clone(),
            })
    }
}

/// The lock the catalog read and the host entries it registered, for the assembly identity the
/// host writes (ADR-0080). The release is read again here, never cached: ADR-0084 §3 has every
/// catalog build, a `/modules reload` included, load the release of that build, so the identity
/// names what that build registered.
pub fn module_sources(deps: &HostDeps) -> Result<ModuleSources, String> {
    module_sources_from(deps, &HOST_ENTRIES, official_release_manifest())
}

/// [`module_sources`] over `entries`, resolved against the release whose manifest is `release`.
fn module_sources_from(
    deps: &HostDeps,
    entries: &[(&str, &str)],
    release: Option<PathBuf>,
) -> Result<ModuleSources, String> {
    let lock = load_modules_lock(&deps.environment_dirs).map_err(|error| error.to_string())?;
    let mut host = Vec::new();
    for (key, package) in entries {
        // The same choice the registration makes: a key a user lock names is the lock's
        // package, and the release's entry for it is not what runs.
        if lock.resolve(key).is_some() {
            continue;
        }
        let release = release.as_deref().ok_or_else(|| {
            ModulesError::HostEntryNoRelease {
                module: (*key).to_owned(),
                package: (*package).to_owned(),
            }
            .to_string()
        })?;
        host.push((
            (*key).to_owned(),
            host_entry_identity(key, package, release).map_err(|error| error.to_string())?,
        ));
    }
    Ok(ModuleSources { lock, host })
}

/// What the release states of the package `package`, for the identity of the catalog key
/// `module`: the manifest name, the digest the loader verified, the ABI, and the versions the
/// release pins.
///
/// The digest is the manifest's own, not a second digest of the bytes: the loader verified the
/// component against that entry when the catalog registered it, so it is the loader-verified
/// digest, exactly as the lock's digest is for a lock-selected package. The version is the
/// host's own, because the release manifest is built with the binary in one pass (D083b 2) and a
/// package manifest carries no version of its own; the digest is what pins the bytes.
fn host_entry_identity(
    module: &str,
    package: &str,
    release: &Path,
) -> Result<PackageIdentity, ModulesError> {
    let manifest = read_release(module, package, release)?;
    let entry = manifest
        .entry(package)
        .ok_or_else(|| ModulesError::HostEntryLoad {
            module: module.to_owned(),
            package: package.to_owned(),
            path: release.to_owned(),
            source: Box::new(LoadError::NotInManifest {
                name: package.to_owned(),
            }),
        })?;
    check_allocation(module, entry)?;
    Ok(PackageIdentity {
        name: entry.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        digest: entry.digest.to_string(),
        abi: format!("{}+{}", entry.world, entry.protocol),
    })
}

/// The one line a debug build writes when it loads a module set from `release`, once per
/// process, so an operator can see which module set a development binary loaded without a line
/// per catalog assembly. It goes through `write_stderr` (the injected channel), never
/// `eprintln!`: a line on the process's real stderr would land on the TUI's drawn screen, and a
/// host test that captures stderr would never see it. Both paths that load the release — the
/// locked modules and the host entries — call it, and the first of the process to load anything
/// prints it.
#[cfg(debug_assertions)]
fn announce_release(deps: &HostDeps, release: &Path) {
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

/// A release build writes no such notice.
#[cfg(not(debug_assertions))]
fn announce_release(_deps: &HostDeps, _release: &Path) {}

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

    /// The built `p1-module-read` component's bytes, or a panic naming the build script.
    fn built_read_wasm() -> Vec<u8> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules/p1-module-read/p1-module-read.wasm");
        std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "the p1-module-read artifact {} is missing ({error}): run scripts/build-modules.sh --all first",
                path.display()
            )
        })
    }

    /// The release entry of the built `p1/read` component: the manifest name, the digest of the
    /// bytes `scripts/build-modules.sh` published, and the class, world, protocol, grants and
    /// variant of the package manifest the build wrote.
    fn read_entry() -> p1_contracts::serde_json::Value {
        use p1_contracts::serde_json::{Value, json};

        let manifest: Value = p1_contracts::serde_json::from_slice(
            &std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(
                "../../modules/target/p1-modules/p1-module-read/p1-module-read.manifest.json",
            ))
            .expect("the built p1-module-read package manifest reads"),
        )
        .expect("the package manifest is JSON");
        json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": "packages/p1-module-read/p1-module-read.wasm",
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        })
    }

    /// Writes a release under `dir` holding `entries` and the built `p1/read` bytes at each
    /// entry's path; returns its manifest path. A case that hands an entry another digest gets a
    /// release whose package the loader must refuse.
    fn write_read_release(dir: &Path, entries: &[p1_contracts::serde_json::Value]) -> PathBuf {
        use p1_contracts::serde_json::json;

        let wasm = built_read_wasm();
        for entry in entries {
            let component = dir.join(entry["path"].as_str().expect("entry path"));
            std::fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
            std::fs::write(&component, &wasm).expect("component");
        }
        let manifest = dir.join(RELEASE_MANIFEST_FILE);
        std::fs::write(
            &manifest,
            json!({ "format": "p1-release-manifest/1", "components": entries }).to_string(),
        )
        .expect("release manifest");
        manifest
    }

    /// A config tree whose environments directory carries no lock: the shipped shape, where a
    /// host entry is what a build registers.
    fn config_without_lock() -> (tempfile::TempDir, Vec<PathBuf>) {
        let config = tempfile::tempdir().expect("config dir");
        let environments = config.path().join("environments");
        std::fs::create_dir_all(&environments).expect("environments dir");
        (config, vec![environments])
    }

    /// The key a lock file names, over `dirs`, kept from the native-registration case this file
    /// inherited: a lock selects a key only when it names it.
    #[test]
    fn the_lock_selects_only_the_key_it_names() {
        let root = tempfile::tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let dirs = [environments];
        assert!(!lock_selects(&dirs, "read"), "no lock selects nothing");

        std::fs::write(
            root.path().join("modules.lock"),
            "format = \"p1-modules-lock/1\"\n\n[modules]\n",
        )
        .unwrap();
        assert!(!lock_selects(&dirs, "read"), "the shipped empty lock");

        let entry = read_entry();
        std::fs::write(
            root.path().join("modules.lock"),
            p1_module_tests::lock_text("read", &entry),
        )
        .unwrap();
        assert!(lock_selects(&dirs, "read"));
        assert!(!lock_selects(&dirs, "grep"));
    }

    /// S1.8.1: a release that holds no `p1/read` refuses the catalog build, naming the package and
    /// the manifest it looked in — never a silent native fallback.
    #[test]
    fn a_release_without_the_read_package_is_refused_by_name() {
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_read_release(release.path(), &[]);
        let (_config, dirs) = config_without_lock();
        let deps = quiet_deps(dirs);

        let mut catalog = Catalog::new();
        let error =
            register_host_entries_from(&mut catalog, &deps, &HOST_ENTRIES, Some(manifest.clone()))
                .expect_err("a release without p1/read is refused");
        assert!(error.contains("p1/read"), "{error}");
        assert!(error.contains(&manifest.display().to_string()), "{error}");
        assert!(
            catalog.tool_keys().is_empty(),
            "nothing is registered: {error}"
        );
    }

    /// S1.8.1: a `p1/read` whose bytes are not the ones its manifest pins refuses the catalog
    /// build, naming the package and the manifest.
    #[test]
    fn a_read_package_whose_bytes_do_not_verify_is_refused_by_name() {
        let release = tempfile::tempdir().expect("release dir");
        let mut entry = read_entry();
        entry["digest"] = p1_contracts::serde_json::json!(format!("sha256:{}", "0".repeat(64)));
        let manifest = write_read_release(release.path(), &[entry]);
        let (_config, dirs) = config_without_lock();
        let deps = quiet_deps(dirs);

        let mut catalog = Catalog::new();
        let error =
            register_host_entries_from(&mut catalog, &deps, &HOST_ENTRIES, Some(manifest.clone()))
                .expect_err("bytes that do not verify are refused");
        assert!(error.contains("p1/read"), "{error}");
        assert!(error.contains(&manifest.display().to_string()), "{error}");
        assert!(
            catalog.tool_keys().is_empty(),
            "nothing is registered: {error}"
        );
    }

    /// S1.8.1: a user lock that names `read` still wins. The host entry is not loaded at all (the
    /// release it is offered holds no `p1/read`, and nothing is refused), the locked-module
    /// registration registers the LOCK's package under the key with no collision, and the
    /// identity resolves the key through the lock.
    #[test]
    fn a_user_lock_naming_read_wins_over_the_host_entry() {
        let fixture = p1_module_tests::Release::with_fixture();
        let entry = fixture.fixture_entry(p1_module_tests::FIXTURE_NAME);
        let (config, dirs) = config_without_lock();
        std::fs::write(
            config.path().join("modules.lock"),
            p1_module_tests::lock_text("read", &entry),
        )
        .expect("lock");
        let deps = quiet_deps(dirs);

        // An empty release: had the host entry been loaded, this build would have failed naming
        // p1/read.
        let empty = tempfile::tempdir().expect("empty release dir");
        let empty_manifest = write_read_release(empty.path(), &[]);
        let mut catalog = Catalog::new();
        register_host_entries_from(
            &mut catalog,
            &deps,
            &HOST_ENTRIES,
            Some(empty_manifest.clone()),
        )
        .expect("a lock-selected key leaves the host entry alone");
        assert!(
            catalog.tool_keys().is_empty(),
            "the host entry registers nothing"
        );

        // The lock's package registers under the key, with no collision.
        register_locked_modules_from(&mut catalog, &deps, Some(fixture.manifest_file()))
            .expect("the lock's package registers");
        assert_eq!(catalog.tool_keys(), ["read"]);

        // And the identity names the lock's package, not the release's.
        let sources = module_sources_from(&deps, &HOST_ENTRIES, Some(empty_manifest))
            .expect("a lock-selected key needs no release");
        let resolved = sources.resolve("read").expect("the lock resolves the key");
        assert_eq!(resolved.name, p1_module_tests::FIXTURE_NAME);
        assert_eq!(resolved.digest, entry["digest"].as_str().expect("digest"));
    }

    /// S1.8.1: with no lock, the host entry registers the release's package under the key, its
    /// catalog key is the one an environment selects, and the identity names it as a PACKAGE row
    /// (manifest name, loader-verified digest, ABI) — never as a native module.
    #[test]
    fn the_host_entry_registers_the_release_package_and_the_identity_names_it() {
        let entry = read_entry();
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_read_release(release.path(), std::slice::from_ref(&entry));
        let (_config, dirs) = config_without_lock();
        let deps = quiet_deps(dirs);

        let mut catalog = Catalog::new();
        register_host_entries_from(&mut catalog, &deps, &HOST_ENTRIES, Some(manifest.clone()))
            .expect("the release's p1/read registers");
        assert_eq!(catalog.tool_keys(), ["read"]);

        let sources =
            module_sources_from(&deps, &HOST_ENTRIES, Some(manifest)).expect("the release reads");
        let resolved = sources
            .resolve("read")
            .expect("the host entry resolves the key");
        assert_eq!(resolved.name, "p1/read");
        assert_eq!(resolved.digest, entry["digest"].as_str().expect("digest"));
        assert_eq!(
            resolved.abi,
            format!(
                "{}+{}",
                entry["world"].as_str().expect("world"),
                entry["protocol"].as_str().expect("protocol")
            )
        );
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
        use p1_contracts::serde_json::json;
        use p1_contracts::{
            CancellationToken, ModelOptions, Provider, ToolCall, ToolContext, ToolInput,
        };

        let entry = read_entry();

        // The release, and a lock next to the environments directory selecting `p1/read`
        // under the key `read`.
        let release = tempfile::tempdir().expect("release dir");
        let release_manifest = write_read_release(release.path(), std::slice::from_ref(&entry));
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
