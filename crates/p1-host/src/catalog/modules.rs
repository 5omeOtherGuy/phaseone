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
//! - the registration declares each accepted tool package's semantic capabilities under the
//!   loader-built identity (S1.5's `capabilities::declare_package`), so a package tool is
//!   visible to the host's capability checks exactly as its native twin is (ADR-0083 rule 7);
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
//!
//! `notice: crates/p1-host/src/catalog/modules.rs (S1): S3.8 (D083b, D-XO-49) lists `p1/shell` and
//! `p1/finish` in [`HOST_ENTRIES`] beside `p1/read` and loads them through the one shared step —
//! every entry's package is loaded by [`register_host_entries`], verified against the release
//! manifest and handed to the registration that owns the key ([`HostEntryRegistration`]; the
//! shell's sandbox face and the finish's gate stay in `catalog/tools.rs`, ADR-0083 §1 and §2), so
//! the PR's own `load_release_module` is gone. The lock path is unchanged.`

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
    CallOutputs, ComponentEntry, ExecutionLimits, LoadError, LoadedModule, Loader, ManifestError,
    ModuleKind, OutputStore, ReleaseManifest, Services, wasm_tool,
};
use thiserror::Error;

use crate::HostDeps;
#[cfg(debug_assertions)]
use crate::run::write_stderr;

/// The callback a worker-registration path uses to announce an assembled host entry's face
/// (its declared tool name) to the worker service (`worker_result`, #448).
type ResultAnnouncement = Arc<dyn Fn(&str) + Send + Sync>;

/// The release manifest's file name inside the module set (ADR-0079).
pub const RELEASE_MANIFEST_FILE: &str = "manifest.json";

/// The official-release host entries this host registers (D083b 2), as `(catalog key, package)`.
///
/// The key is what an environment selects the entry by and what an assembly identity names as
/// the module's `package`; the package name is what the release must ship. The five file tools
/// are entries whose native registrations are gone (S1.8.1 for `read`, S7.10-R1 for the other
/// four, ADR-0091) and whose components the shared registration builds, so an environment that
/// names `edit`, `write`, `apply_patch` or `grep` runs the release's component with no lock and
/// no compiled-in tool behind it; `shell` and `finish` are S3.8's entries
/// ([`HOST_COMPOSED_ENTRIES`]), whose catalog tool the host composes around the loaded package.
/// A user lock that names a key here still wins ([`lock_selects`]), and S5.11's policy entries
/// are one more list passed to the same step. `read_output` (#511, ADR-0109) pages the run's
/// output store; the shared registration links it the store view ([`locked_module_services`]).
pub const HOST_ENTRIES: [(&str, &str); 8] = [
    ("read", "p1/read"),
    ("edit", "p1/edit"),
    ("write", "p1/write"),
    ("apply_patch", "p1/patch"),
    ("grep", "p1/search"),
    ("read_output", "p1/read-output"),
    ("shell", "p1/shell"),
    ("finish", "p1/finish"),
];

/// The [`HOST_ENTRIES`] keys whose catalog tool the HOST composes around the package the release
/// ships instead of the shared registration (S3.8, D-XO-49): the shell, over the sandboxed
/// process service and under the sandbox paragraph and the `+sandbox` variant (ADR-0083 §1), and
/// the finish, under the completion hub's gate and the declaration its policy and output contract
/// choose (§2) — both in `catalog/tools.rs`, where the native registrations stood.
///
/// The shared step loads their packages exactly as it loads every entry's — the release must hold
/// them, and a missing or unverified one fails the catalog build naming the key and the package —
/// and hands each to the [`HostEntryRegistration`] the caller gives for the key
/// ([`register_composed_host_entries`]). A caller that gives none (an entry list of its own)
/// registers nothing for them here: the host step that owns the key is the one that registers it.
const HOST_COMPOSED_ENTRIES: [&str; 2] = ["shell", "finish"];

/// How the host registers one of its own host entries (a [`HOST_COMPOSED_ENTRIES`] key): the
/// loaded, verified package of the key goes in, whatever the host builds around it comes out.
/// `catalog/tools.rs` supplies the shell's and the finish's.
pub type HostEntryRegistration =
    Box<dyn Fn(&mut Catalog, Arc<LoadedModule>) -> Result<(), String> + Send + Sync>;

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
#[derive(Clone, Debug)]
pub struct PackageIdentity {
    /// The package's manifest name, `<namespace>/<name>`.
    pub name: String,
    /// The release version the package runs at.
    pub version: String,
    /// `sha256:<64 lowercase hex>`, the digest of the package's `.wasm`.
    pub digest: String,
    /// `<world>+<protocol>`, the ABI the package speaks.
    pub abi: String,
    /// Semantic grants derived from this exact verified load's manifest, not a later
    /// declaration for another generation with identical component bytes.
    pub(crate) semantic: super::capabilities::Capabilities,
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
pub(crate) fn installed_module_root() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("../share/p1/modules"))
}

/// Pass only the running executable's installation root as authority for compiled copies.
/// Debug source-tree fallback and test release overrides are not trusted.
pub(crate) fn release_loader(manifest: ReleaseManifest, root: &Path) -> Result<Loader, LoadError> {
    Loader::for_installation(manifest, root, installed_module_root().as_deref())
}

pub fn official_release_manifest() -> Option<PathBuf> {
    let share = installed_module_root()?.join(RELEASE_MANIFEST_FILE);
    // Compiled in, so the fallback is the checkout's own path, never a place an installation
    // could edit.
    let built = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules")
        .join(RELEASE_MANIFEST_FILE);
    Some(choose_release_manifest(share, built))
}

/// A catalog build's release path. A test can substitute a scratch release without touching
/// the installed tree; production always resolves the executable's own release.
pub(crate) fn release_for_build(deps: &HostDeps) -> Option<PathBuf> {
    #[cfg(test)]
    if let Some(release) = &deps.release_manifest {
        return Some(release.clone());
    }
    #[cfg(not(test))]
    let _ = deps;
    official_release_manifest()
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
    load_locked_modules_except(lock, release_manifest, &[], None)
}

/// [`load_locked_modules`] without the keys in `skip`: the [`HOST_COMPOSED_ENTRIES`] a lock
/// names are loaded by the host-entry step, which hands them to the host's own registration.
fn load_locked_modules_except(
    lock: &ModulesLock,
    release_manifest: &Path,
    skip: &[&str],
    loaders: Option<&BuildLoaders>,
) -> Result<Vec<ModulePackage>, ModulesError> {
    let release_error = |source| ModulesError::Release {
        path: release_manifest.to_owned(),
        source: Box::new(source),
    };
    let manifest = match loaders {
        Some(loaders) => loaders.manifest_for(release_manifest),
        None => ReleaseManifest::read(release_manifest),
    }
    .map_err(release_error)?;
    manifest.check_unique_digests().map_err(release_error)?;
    let mut packages = Vec::new();
    // The loader starts an epoch thread; a release nothing selects needs none.
    if lock.iter().all(|(module, _)| skip.contains(&module)) {
        return Ok(packages);
    }
    let loader = match loaders {
        Some(loaders) => loaders
            .for_release(release_manifest, manifest.clone())
            .map_err(|error| ModulesError::Runtime(Box::new(error)))?,
        None => {
            let root = release_manifest.parent().unwrap_or(Path::new("."));
            Arc::new(
                release_loader(manifest.clone(), root)
                    .map_err(|error| ModulesError::Runtime(Box::new(error)))?,
            )
        }
    };
    for (module, locked) in lock.iter() {
        if skip.contains(&module) {
            continue;
        }
        packages.push(load_locked_entry(&loader, &manifest, module, locked)?);
    }
    Ok(packages)
}

/// Verifies the lock's pin and the class allocation of `module`'s package, then compiles it.
fn load_locked_entry(
    loader: &Loader,
    manifest: &ReleaseManifest,
    module: &str,
    locked: &LockedModule,
) -> Result<ModulePackage, ModulesError> {
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
    Ok(ModulePackage {
        module: module.to_owned(),
        lock: locked.source.clone(),
        loaded,
    })
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

/// Registers each verified tool package under its module name and declares its semantic
/// capabilities under the loader-built identity (S1.5), so a package tool is visible to the
/// host's capability checks exactly as a native one is. A name a registered tool already has
/// is refused: a package never silently replaces a compiled-in tool.
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
    register_modules_with_announcement(catalog, packages, services, None)
}

fn register_modules_with_announcement(
    catalog: &mut Catalog,
    packages: Vec<ModulePackage>,
    services: ModuleServices,
    announce_result: Option<ResultAnnouncement>,
) -> Result<(), ModulesError> {
    #[cfg(not(feature = "delegation"))]
    let _ = &announce_result;
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
        // S1.5's carrier, filled here: the registration that accepts a verified tool
        // package declares its semantic capabilities under the identity the loader built,
        // exactly as a native registration lists its `NativeDeclaration`. The call sits after
        // the refusals, so a package that is not registered declares nothing; a package
        // reaches `completion_policy` and `WorkerReportTap::retool` only through it
        // (ADR-0083 rule 7).
        super::capabilities::declare_package(&package.loaded);
        let key = package.module.clone();
        let loaded = Arc::new(package.loaded);
        #[cfg(feature = "delegation")]
        if key == "worker_result"
            && let Some(announce) = &announce_result
        {
            register_named_host_entry(catalog, &key, loaded, services.clone(), announce.clone());
            continue;
        }
        register_locked_entry(catalog, &key, loaded, services.clone());
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

/// Register a host entry whose assembled face must be announced to a service.
#[cfg(feature = "delegation")]
pub(crate) fn register_named_host_entry(
    catalog: &mut Catalog,
    module: &str,
    loaded: Arc<LoadedModule>,
    services: ModuleServices,
    announce: ResultAnnouncement,
) {
    let key = module.to_owned();
    catalog.tool(
        module,
        Box::new(move |spec: &ToolSpec, tool_services: &ToolServices| {
            let tool = instantiate(&key, &loaded, spec, &services, tool_services, true)?;
            announce(&tool.declaration().name);
            Ok(tool)
        }),
    );
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

    fn take_command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.take_command_exit_code(call_id)
    }

    fn command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.command_exit_code(call_id)
    }

    fn synthetic_command_result(&self) -> bool {
        self.inner.synthetic_command_result()
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
/// ABI, check the class allocation, start the loader and load the package by name. The ONE
/// loading step of every host entry, whichever registration takes the package
/// ([`register_entries_from`]): the class allocation check is applied to every entry here, so an
/// entry the host composes itself is checked exactly as a shared one.
#[cfg(test)]
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
        release_loader(manifest, root).map_err(|error| ModulesError::Runtime(Box::new(error)))?;
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
#[cfg(test)]
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
    register_locked_modules_from(catalog, deps, release_for_build(deps))
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
    // A lock-selected shell or finish still needs the host's process service or completion hub,
    // so the host-entry step hands it to the host's registration instead
    // ([`register_composed_host_entries`]).
    let packages = load_locked_modules_except(
        &lock,
        &release,
        &HOST_COMPOSED_ENTRIES,
        Some(&deps.build_loaders),
    )
    .map_err(|error| error.to_string())?;
    // A member a lock selects is a member of its family all the same: it takes the host's
    // lists and grant check, as a host entry does, not only the family's scopes
    // (`catalog/delegation.rs`, D084).
    #[cfg(feature = "delegation")]
    let services = super::delegation::with_member_lists(
        locked_module_services(deps),
        super::delegation::worker_lists_with_keys(
            catalog
                .tool_keys()
                .into_iter()
                .chain(packages.iter().map(|package| package.module.clone()))
                .collect(),
            deps,
        )?,
    );
    #[cfg(not(feature = "delegation"))]
    let services = locked_module_services(deps);
    for package in &packages {
        deps.verified_sources
            .record(&package.module, &package.loaded);
    }
    #[cfg(feature = "delegation")]
    let announce: Option<ResultAnnouncement> = deps.worker_service.as_ref().map(|service| {
        let service = service.clone();
        Arc::new(move |name: &str| service.set_result_tool_name(name)) as ResultAnnouncement
    });
    #[cfg(not(feature = "delegation"))]
    let announce = None;
    register_modules_with_announcement(catalog, packages, services, announce)
        .map_err(|error| error.to_string())
}

/// The catalog build path's step for the official-release host entries (D083b 2,
/// [`HOST_ENTRIES`]): each entry no user lock selects is loaded from the release, verified and
/// registered through the same path a lock-selected package takes. A refusal is an error naming
/// the package and the release — never a native fallback. The entries the host composes its own
/// tool for are the host's step's ([`register_composed_host_entries`], S3.8), loaded through this
/// same path.
pub(super) fn register_host_entries(catalog: &mut Catalog, deps: &HostDeps) -> Result<(), String> {
    register_host_entries_from(catalog, deps, &HOST_ENTRIES, release_for_build(deps))
}

/// [`register_host_entries`] over `entries`, loaded from the release whose manifest is
/// `release`. A slice that adds host entries (S5.11's policy packages) calls this with its own
/// list.
///
/// The entries of [`HOST_COMPOSED_ENTRIES`] are not registered here: the host builds their
/// catalog tools itself ([`register_composed_host_entries`], `catalog/tools.rs`), so this list's
/// entries are the shared registration's alone.
pub(crate) fn register_host_entries_from(
    catalog: &mut Catalog,
    deps: &HostDeps,
    entries: &[(&str, &str)],
    release: Option<PathBuf>,
) -> Result<(), String> {
    register_entries_from(catalog, deps, entries, release, &[])
}

/// Registers the host entries the HOST composes itself — the `(key, registration)` pairs of
/// [`HOST_COMPOSED_ENTRIES`], S3.8's shell and finish — through the one shared step: for each
/// key, its package is loaded from the official release, verified against it and handed to the
/// registration, which builds the catalog tool around it. A user lock that names the key leaves
/// the release's package unloaded, exactly as it does for `read`, and the lock's package is
/// handed to the same registration, so it is linked with the host's services
/// ([`register_locked_modules`] leaves these keys alone).
pub(crate) fn register_composed_host_entries(
    catalog: &mut Catalog,
    deps: &HostDeps,
    composed: &[(&str, HostEntryRegistration)],
) -> Result<(), String> {
    let entries: Vec<(&str, &str)> = HOST_ENTRIES
        .iter()
        .copied()
        .filter(|(key, _)| composed.iter().any(|(registered, _)| registered == key))
        .collect();
    register_entries_from(catalog, deps, &entries, release_for_build(deps), composed)
}

/// The step every host-entry registration takes: each of `entries` that no user lock selects is
/// loaded from `release` and verified against it — the manifest read, the duplicate-identity and
/// class-allocation checks, then compiles the package by name ([`load_host_entry`]) — and
/// registered under its catalog key, through the registration `composed` gives for the key when
/// the host owns its tool, through the shared [`register_host_entry`] otherwise. A
/// [`HOST_COMPOSED_ENTRIES`] key with no registration is the host's own: the step that owns it
/// registers it, so nothing is loaded or registered for it here.
fn register_entries_from(
    catalog: &mut Catalog,
    deps: &HostDeps,
    entries: &[(&str, &str)],
    release: Option<PathBuf>,
    composed: &[(&str, HostEntryRegistration)],
) -> Result<(), String> {
    let mut packages = Vec::new();
    // A catalog batch shares one loader. The same verified
    // manifest decides every entry in the batch, even if an installation swaps the
    // on-disk manifest while the batch is being registered.
    let mut release_loader: Option<(ReleaseManifest, Arc<Loader>)> = None;
    for (key, package) in entries {
        let registration = composed
            .iter()
            .find(|(registered, _)| registered == key)
            .map(|(_, registration)| registration);
        if lock_selects(&deps.environment_dirs, key) {
            // A user lock names this key: its package is what runs, never the release entry's,
            // or the two would collide. A key the host composes takes the lock's package
            // through the host's registration, which links the services the shared one lacks;
            // any other key is the locked-module registration's.
            if let Some(registration) = registration {
                let locked = load_locked_host_entry(deps, key, release.as_deref())?;
                deps.verified_sources.record(key, &locked.loaded);
                registration(catalog, Arc::new(locked.loaded))?;
            }
            continue;
        }
        if registration.is_none() && HOST_COMPOSED_ENTRIES.contains(key) {
            // The host's own entry: it is loaded and registered by the step that owns its tool
            // key ([`register_composed_host_entries`]), never by the shared registration.
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
        if release_loader.is_none() {
            let manifest = match deps.build_loaders.manifest_for(release) {
                Ok(manifest) => {
                    manifest
                        .check_unique_digests()
                        .map_err(|error| error.to_string())?;
                    manifest
                }
                Err(error) => return Err(error.to_string()),
            };
            let loader = deps
                .build_loaders
                .for_release(release, manifest.clone())
                .map_err(|error| ModulesError::Runtime(Box::new(error)).to_string())?;
            release_loader = Some((manifest, loader));
        }
        let (manifest, loader) = release_loader.as_ref().expect("loader initialized above");
        if let Some(entry) = manifest.entry(package) {
            check_allocation(key, entry).map_err(|error| error.to_string())?;
        }
        let loaded = loader.load(package).map_err(|source| {
            ModulesError::HostEntryLoad {
                module: (*key).to_owned(),
                package: (*package).to_owned(),
                path: release.to_owned(),
                source: Box::new(source),
            }
            .to_string()
        })?;
        let loaded = ModulePackage {
            module: (*key).to_owned(),
            lock: release.to_owned(),
            loaded,
        };
        deps.verified_sources.record(key, &loaded.loaded);
        match registration {
            Some(registration) => registration(catalog, Arc::new(loaded.loaded))?,
            None => packages.push(loaded),
        }
    }
    if packages.is_empty() {
        return Ok(());
    }
    register_modules(catalog, packages, locked_module_services(deps))
        .map_err(|error| error.to_string())
}

/// The package a user lock selects for the host-composed key `key`, verified against `release`
/// exactly as [`load_locked_modules`] verifies it.
fn load_locked_host_entry(
    deps: &HostDeps,
    key: &str,
    release: Option<&Path>,
) -> Result<ModulePackage, String> {
    let lock = load_modules_lock(&deps.environment_dirs).map_err(|error| error.to_string())?;
    let locked = lock
        .resolve(key)
        .ok_or_else(|| format!("modules.lock no longer names `{key}`"))?;
    let release = release.ok_or_else(|| ModulesError::NoRelease.to_string())?;
    announce_release(deps, release);
    let release_error = |source| {
        ModulesError::Release {
            path: release.to_owned(),
            source: Box::new(source),
        }
        .to_string()
    };
    let manifest = deps
        .build_loaders
        .manifest_for(release)
        .map_err(release_error)?;
    manifest.check_unique_digests().map_err(release_error)?;
    let loader = deps
        .build_loaders
        .for_release(release, manifest.clone())
        .map_err(|error| ModulesError::Runtime(Box::new(error)).to_string())?;
    load_locked_entry(&loader, &manifest, key, locked).map_err(|error| error.to_string())
}

/// One release loader per catalog build. Every retained module holds the loader's engine
/// and epoch clock, so a later build can read the same path without replacing running bytes.
#[derive(Default)]
pub struct BuildLoaders {
    loaders: std::sync::Mutex<std::collections::HashMap<PathBuf, (ReleaseManifest, Arc<Loader>)>>,
    /// The policy host entries of each release, verified and compiled once per build.
    host_entries: std::sync::Mutex<std::collections::HashMap<PathBuf, crate::policy::HostEntries>>,
}

impl BuildLoaders {
    pub fn for_release(
        &self,
        path: &Path,
        manifest: ReleaseManifest,
    ) -> Result<Arc<Loader>, LoadError> {
        let mut loaders = self.loaders.lock().expect("build loaders");
        if let Some((_, loader)) = loaders.get(path) {
            return Ok(loader.clone());
        }
        let root = path.parent().unwrap_or(Path::new("."));
        let loader = Arc::new(release_loader(manifest.clone(), root)?);
        loaders.insert(path.to_owned(), (manifest, loader.clone()));
        Ok(loader)
    }

    /// Read once per build; a later install at this path cannot change validation for
    /// packages loaded by the already-created loader.
    pub fn manifest_for(&self, path: &Path) -> Result<ReleaseManifest, ManifestError> {
        if let Some((manifest, _)) = self.loaders.lock().expect("build loaders").get(path) {
            return Ok(manifest.clone());
        }
        ReleaseManifest::read(path)
    }

    /// The build's loader for the release at `path`, created from the build's own manifest
    /// snapshot when nothing loaded from it yet.
    pub(crate) fn build_release(&self, path: &Path) -> Result<crate::policy::BuildRelease, String> {
        let unreadable =
            |error: String| format!("cannot load the release {}: {error}", path.display());
        let manifest = self
            .manifest_for(path)
            .map_err(|error| unreadable(error.to_string()))?;
        let loader = self
            .for_release(path, manifest)
            .map_err(|error| unreadable(error.to_string()))?;
        Ok(crate::policy::BuildRelease {
            path: path.to_owned(),
            loader,
        })
    }

    /// The policy host entries of the release at `path`, loaded through this build's loader
    /// on the first call and shared by every later one: the session's generation and the
    /// agents it assembles use the bytes this build verified, not a second read of the path.
    /// The lock is held while loading, so concurrent callers compile them once.
    pub(crate) fn host_entries(&self, path: &Path) -> Result<crate::policy::HostEntries, String> {
        let mut entries = self.host_entries.lock().expect("build host entries");
        if let Some(loaded) = entries.get(path) {
            return Ok(loaded.clone());
        }
        let loaded = crate::policy::load_host_entries_in(self, path)?;
        entries.insert(path.to_owned(), loaded.clone());
        Ok(loaded)
    }

    pub fn clear(&self) {
        self.loaders.lock().expect("build loaders").clear();
        self.host_entries
            .lock()
            .expect("build host entries")
            .clear();
    }
}

/// Verified package identities for one catalog build. A snapshot is taken after provider
/// activation so the journal never consults a mutable release manifest for provenance.
#[derive(Default)]
pub struct VerifiedSources(std::sync::Mutex<std::collections::HashMap<String, PackageIdentity>>);

impl VerifiedSources {
    pub fn record(&self, key: &str, loaded: &LoadedModule) {
        self.0.lock().expect("verified sources").insert(
            key.to_owned(),
            PackageIdentity {
                name: loaded.name().to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
                digest: loaded.digest().to_string(),
                abi: loaded.abi().to_owned(),
                semantic: super::capabilities::package_capabilities(loaded),
            },
        );
    }

    pub fn clear(&self) {
        self.0.lock().expect("verified sources").clear();
    }

    pub fn resolve(&self, key: &str) -> Option<PackageIdentity> {
        self.0.lock().expect("verified sources").get(key).cloned()
    }

    /// Resolve a tool's loader-built implementation even if its catalog key is a face.
    #[cfg(test)]
    pub(crate) fn set_digest_for_test(&self, key: &str, name: &str, digest: &str) {
        self.0.lock().expect("verified sources").insert(
            key.to_owned(),
            PackageIdentity {
                name: name.to_owned(),
                version: "test".to_owned(),
                digest: digest.to_owned(),
                abi: "test".to_owned(),
                semantic: super::capabilities::Capabilities::NONE,
            },
        );
    }

    pub fn digest_for_implementation(&self, implementation: &str) -> Option<String> {
        self.0
            .lock()
            .expect("verified sources")
            .values()
            .find(|package| package.name == implementation)
            .map(|package| package.digest.clone())
    }
}

/// Package sources used by the identity writer. Production takes a snapshot of verified
/// loads; `of_lock` remains available for fixture identity tests.
#[derive(Clone)]
pub struct ModuleSources {
    lock: ModulesLock,
    host: Vec<(String, PackageIdentity)>,
    live: Option<std::sync::Arc<VerifiedSources>>,
}

impl ModuleSources {
    /// Registry used by the running catalog, if this is not a fixture lock snapshot.
    pub fn verified(&self) -> Option<&VerifiedSources> {
        self.live.as_deref()
    }

    /// Only the lock's resolutions: an assembly of a release without host entries, and the
    /// journal-identity cases that drive the identity builder over a fixture lock.
    pub fn of_lock(lock: ModulesLock) -> Self {
        Self {
            lock,
            host: Vec::new(),
            live: None,
        }
    }

    /// What `key` resolves to, or `None` for a native module.
    pub fn resolve(&self, key: &str) -> Option<PackageIdentity> {
        if let Some(live) = &self.live {
            return live.resolve(key);
        }
        if let Some(locked) = self.lock.resolve(key) {
            return Some(PackageIdentity {
                name: locked.package.clone(),
                version: locked.version.clone(),
                digest: locked.digest.clone(),
                abi: format!("{}+{}", locked.world, locked.protocol),
                semantic: super::capabilities::Capabilities::NONE,
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
                semantic: package.semantic,
            })
    }
}

/// The lock the catalog read and the host entries it registered, for the assembly identity the
/// host writes (ADR-0080). The release is read again here, never cached: ADR-0084 §3 has every
/// catalog build, a `/modules reload` included, load the release of that build, so the identity
/// names what that build registered.
pub fn module_sources(deps: &HostDeps) -> Result<ModuleSources, String> {
    Ok(ModuleSources {
        lock: ModulesLock::default(),
        host: Vec::new(),
        live: Some(deps.verified_sources.clone()),
    })
}

/// [`module_sources`] over `entries`, resolved against the release whose manifest is `release`.
///
/// An entry the release carries no package for is not one of ITS packages: the key is not a
/// package row here (the lock's package, or a native module). The catalog build is where a
/// release missing an entry it must hold is refused ([`register_entries_from`] names the key and
/// the package), so an identity asked for after a successful build always finds the entries that
/// build registered; this step answers for the caller's list, not for the release's completeness.
#[cfg(test)]
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
        if let Some(identity) =
            host_entry_identity(key, package, release).map_err(|error| error.to_string())?
        {
            host.push(((*key).to_owned(), identity));
        }
    }
    Ok(ModuleSources {
        lock,
        host,
        live: None,
    })
}

/// What the release states of the package `package`, for the identity of the catalog key
/// `module`: the manifest name, the digest the loader verified, the ABI, and the versions the
/// release pins. `None` when the release carries no such package: the key is not one of the
/// release's packages, and the catalog build is where a missing one is refused
/// ([`register_entries_from`]).
///
/// The digest is the manifest's own, not a second digest of the bytes: the loader verified the
/// component against that entry when the catalog registered it, so it is the loader-verified
/// digest, exactly as the lock's digest is for a lock-selected package. The version is the
/// host's own, because the release manifest is built with the binary in one pass (D083b 2) and a
/// package manifest carries no version of its own; the digest is what pins the bytes.
#[cfg(test)]
fn host_entry_identity(
    module: &str,
    package: &str,
    release: &Path,
) -> Result<Option<PackageIdentity>, ModulesError> {
    let manifest = read_release(module, package, release)?;
    let Some(entry) = manifest.entry(package) else {
        return Ok(None);
    };
    check_allocation(module, entry)?;
    Ok(Some(PackageIdentity {
        name: entry.name.clone(),
        version: env!("CARGO_PKG_VERSION").to_string(),
        digest: entry.digest.to_string(),
        abi: format!("{}+{}", entry.world, entry.protocol),
        semantic: super::capabilities::Capabilities::NONE,
    }))
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
/// `workspace`, `snapshot` and `workspace-mutation` a family hook left empty, with the call
/// scope that builds them fresh per call (ADR-0092), and a module that is no member (the
/// `p1/read`, `p1/edit`, `p1/write`, `p1/patch` or `p1/search` component) links exactly as
/// without the families. No other native service
/// backs a module capability in the host yet (the shell's process service is not bridged
/// to the runtime's `ProcessService`), so a package granted one fails its assembly with the
/// runtime's `MissingService` rather than running unlinked. The run's output store backs
/// `tool-outputs` for every package (ADR-0109): it is linked only where a manifest grants it
/// (`read_output`, #511), and reads the store without producing anything, so its `produced`
/// is empty.
fn locked_module_services(deps: &HostDeps) -> ModuleServices {
    let outputs = deps.tool_outputs.clone();
    let base = with_tool_outputs(super::tools::module_services(deps), outputs);
    let Some(family) = deps.module_services.clone() else {
        return base;
    };
    Arc::new(move |module: &str, services: &ToolServices| {
        let mut linked = family(module, services);
        if linked.workspace.is_none() || linked.snapshot.is_none() {
            let base = base(module, services);
            linked.workspace = linked.workspace.or(base.workspace);
            linked.snapshot = linked.snapshot.or(base.snapshot);
            linked.workspace_mutation = linked.workspace_mutation.or(base.workspace_mutation);
            linked.call_scope = linked.call_scope.or(base.call_scope);
        }
        if linked.tool_outputs.is_none() {
            linked.tool_outputs = base(module, services).tool_outputs;
        }
        linked
    })
}

/// `hook` with the store view of `outputs` added to what it links, masked with the agent's own
/// credentials.
fn with_tool_outputs(hook: ModuleServices, outputs: Arc<OutputStore>) -> ModuleServices {
    Arc::new(move |module: &str, services: &ToolServices| {
        let mut linked = hook(module, services);
        linked.tool_outputs = Some(Arc::new(CallOutputs::new(
            outputs.clone(),
            services.mask.secrets().clone(),
        )));
        linked
    })
}

/// Host dependencies over `environment_dirs` that touch no terminal, network or home: what the
/// catalog cases build, and what a sibling file's cases drive this step over.
#[cfg(test)]
pub(crate) fn quiet_deps(environment_dirs: Vec<PathBuf>) -> HostDeps {
    /// Nothing interrupts the catalog cases.
    struct NoInterrupt;
    impl crate::InterruptSource for NoInterrupt {
        fn recv<'a>(
            &'a self,
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + 'a>> {
            Box::pin(std::future::pending())
        }
    }

    let sink =
        || -> crate::SharedWriter { Arc::new(std::sync::Mutex::new(Box::new(std::io::sink()))) };
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

#[cfg(test)]
mod tests {
    use p1_assembly::{EnvironmentFile, ProviderSpec, Substitutions, assemble};
    use p1_contracts::{ModelOptions, Provider, ToolIdentity};
    use p1_finish_guest::CompletionPolicy;
    use p1_module_runtime::ProcessService;
    use p1_module_tests::{FIXTURE_NAME, FakeProcesses, Release, fake_processes, lock_text};
    use p1_testkit::{FakeTool, ScriptedProvider};

    use crate::catalog::capabilities::{Capabilities, SemanticCapability, carries, declared};

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

    /// Under a family hook that serves only its members, a mutating component still gets its
    /// row's mutation and the call scope that builds each call's read record (ADR-0092): with
    /// only `workspace` and `snapshot` filled in, a locked `p1/edit` failed its assembly on
    /// `MissingService("workspace-mutation")` in every run that installs the worker family.
    #[test]
    fn a_family_hook_keeps_the_mutation_and_the_call_scope_of_a_non_member() {
        let root = tempfile::tempdir().unwrap();
        let mut deps = quiet_deps(vec![root.path().join("environments")]);
        let family: ModuleServices = Arc::new(|module: &str, _: &ToolServices| match module {
            "p1/worker-start" => Services {
                workers: Some(Default::default()),
                ..Services::default()
            },
            _ => Services::default(),
        });
        deps.module_services = Some(family);
        let hook = locked_module_services(&deps);
        let services = ToolServices {
            workspace: p1_workspace::Workspace::new(root.path()).unwrap(),
            observed: p1_workspace::ObservedFiles::new(),
            mask: Arc::new(p1_redact::MaskCounter::new()),
            agent: None,
        };

        let edit = hook("p1/edit", &services);
        assert!(edit.workspace.is_some() && edit.snapshot.is_some());
        assert!(edit.workspace_mutation.is_some(), "edit keeps its mutation");
        assert!(edit.call_scope.is_some(), "and its per-call read record");
        let read = hook("p1/read", &services);
        assert!(read.workspace_mutation.is_none(), "read's row grants none");
        let member = hook("p1/worker-start", &services);
        assert!(
            member.workers.is_some(),
            "a member keeps its family's services"
        );
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

    /// The child completion policy (ADR-0051 item 1) exactly as `catalog/children.rs`
    /// computes it over assembled tools: `RecordedCommands` iff one of them carries
    /// `records-command-evidence`. That function is private to the delegation module, so its
    /// one-line rule is restated here; the cases below are about the registration's
    /// declaration, which is what the rule reads.
    fn child_policy(tools: &[Arc<dyn Tool>]) -> CompletionPolicy {
        let can_run_commands = tools
            .iter()
            .any(|tool| carries(tool.as_ref(), SemanticCapability::RecordsCommandEvidence));
        if can_run_commands {
            CompletionPolicy::RecordedCommands
        } else {
            CompletionPolicy::ReportToParent
        }
    }

    /// A `modules.lock` resolving `module` to the fixture package of `release`.
    fn fixture_lock(release: &Release, module: &str) -> ModulesLock {
        let entry = release.fixture_entry(FIXTURE_NAME);
        ModulesLock::parse(
            &release.root().join("modules.lock"),
            &lock_text(module, &entry),
        )
        .expect("fixture lock")
    }

    /// A catalog built the host's way: a scripted provider, and every package `lock` selects
    /// registered through [`register_modules`] with a fake `process` service (the fixture
    /// imports `process`). The fake's receiving end comes back with the catalog so the caller
    /// keeps it alive for the assembled tool's life.
    fn registered_catalog(release: &Release, lock: &ModulesLock) -> (Catalog, FakeProcesses) {
        let mut catalog = Catalog::new();
        let provider = ScriptedProvider::new(Vec::new());
        catalog.provider(
            "scripted",
            Box::new(move |_spec: &ProviderSpec| {
                Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)
            }),
        );
        let (process, processes) = fake_processes();
        let process: Arc<dyn ProcessService> = process;
        let services: ModuleServices = Arc::new(move |_: &str, _: &ToolServices| Services {
            process: Some(process.clone()),
            ..Services::default()
        });
        let packages =
            load_locked_modules(lock, &release.manifest_file()).expect("the package loads");
        register_modules(&mut catalog, packages, services).expect("registration");
        (catalog, processes)
    }

    /// An environment naming `modules`, assembled in a scratch workspace.
    fn assemble_modules(
        catalog: &Catalog,
        modules: &[&str],
    ) -> Result<p1_assembly::Assembled, p1_assembly::AssemblyError> {
        let environment = EnvironmentFile {
            name: "package-capabilities-test".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "test-model".into(),
            profile: None,
            profile_text: None,
            options: ModelOptions::default(),
            tools: modules
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
        };
        let workspace = tempfile::tempdir().expect("scratch workspace");
        assemble(
            catalog,
            &environment,
            workspace.path(),
            &Substitutions {
                workspace: "/work".into(),
                date: "2026-01-01".into(),
                os: "linux".into(),
            },
        )
    }

    /// The built tool package in `dir` (manifest name `name`) as a release entry and its
    /// bytes, laid out as `scripts/build-modules.sh` publishes it.
    fn built_package(dir: &str, name: &str) -> (serde_json::Value, Vec<u8>) {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules")
            .join(dir);
        let wasm_path = root.join(format!("{dir}.wasm"));
        let wasm = std::fs::read(&wasm_path).unwrap_or_else(|error| {
            panic!(
                "the built package {} is missing ({error}): run scripts/build-modules.sh",
                wasm_path.display()
            )
        });
        let manifest_path = root.join(format!("{dir}.manifest.json"));
        let manifest: serde_json::Value = serde_json::from_str(
            &std::fs::read_to_string(&manifest_path)
                .unwrap_or_else(|error| panic!("cannot read {}: {error}", manifest_path.display())),
        )
        .expect("the built manifest is JSON");
        let entry = serde_json::json!({
            "name": name,
            "digest": manifest["digest"],
            "path": format!("packages/{dir}/{dir}.wasm"),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        });
        (entry, wasm)
    }

    #[cfg(feature = "delegation")]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn named_host_entry_announces_assembled_face_not_catalog_key() {
        let release = Release::with_fixture();
        let module = Arc::new(
            release
                .loader()
                .load(FIXTURE_NAME)
                .expect("verified fixture"),
        );
        let mut catalog = Catalog::new();
        let provider = ScriptedProvider::new(Vec::new());
        catalog.provider(
            "scripted",
            Box::new(move |_| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
        );
        let (process, _processes) = fake_processes();
        let process: Arc<dyn ProcessService> = process;
        let services: ModuleServices = Arc::new(move |_, _| Services {
            process: Some(process.clone()),
            ..Services::default()
        });
        let names = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let capture = names.clone();
        register_named_host_entry(
            &mut catalog,
            "worker_result",
            module,
            services,
            Arc::new(move |name| capture.lock().unwrap().push(name.to_owned())),
        );
        let environment = EnvironmentFile {
            name: "face-test".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "test-model".into(),
            profile: None,
            profile_text: None,
            options: ModelOptions::default(),
            tools: vec![ToolSpec {
                module: "worker_result".into(),
                name: Some("read_worker_report".into()),
                description: None,
                variant: None,
            }],
            prompt_template: String::new(),
            context: None,
            summarize_prompt: None,
        };
        let workspace = tempfile::tempdir().expect("scratch");
        let assembled = assemble(
            &catalog,
            &environment,
            workspace.path(),
            &Substitutions {
                workspace: "/work".into(),
                date: "2026-01-01".into(),
                os: "linux".into(),
            },
        )
        .expect("host entry assembles");
        assert_eq!(assembled.tools[0].declaration().name, "read_worker_report");
        assert_eq!(*names.lock().unwrap(), ["read_worker_report"]);
    }

    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn lock_selected_worker_result_announces_its_assembled_face() {
        let release = Release::with_fixture();
        let loaded = release.loader().load(FIXTURE_NAME).expect("fixture");
        let (process, _processes) = fake_processes();
        let process: Arc<dyn ProcessService> = process;
        let services: ModuleServices = Arc::new(move |_, _| Services {
            process: Some(process.clone()),
            ..Services::default()
        });
        let announced = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let capture = announced.clone();
        let mut catalog = Catalog::new();
        catalog.provider(
            "scripted",
            Box::new(|_| Ok(Arc::new(ScriptedProvider::new(Vec::new())) as Arc<dyn Provider>)),
        );
        register_modules_with_announcement(
            &mut catalog,
            vec![ModulePackage {
                module: "worker_result".into(),
                lock: "scratch/modules.lock".into(),
                loaded,
            }],
            services,
            Some(Arc::new(move |name| {
                capture.lock().unwrap().push(name.to_owned())
            })),
        )
        .expect("lock-selected registration");
        let environment = EnvironmentFile {
            name: "selected-result".into(),
            family: "test".into(),
            provider: "scripted".into(),
            model: "test-model".into(),
            profile: None,
            profile_text: None,
            options: ModelOptions::default(),
            tools: vec![ToolSpec {
                module: "worker_result".into(),
                name: Some("read_worker_report".into()),
                description: None,
                variant: None,
            }],
            prompt_template: String::new(),
            context: None,
            summarize_prompt: None,
        };
        let workspace = tempfile::tempdir().unwrap();
        let assembled = assemble(
            &catalog,
            &environment,
            workspace.path(),
            &Substitutions {
                workspace: "/work".into(),
                date: "2026-01-01".into(),
                os: "linux".into(),
            },
        )
        .expect("selected result assembles with face");
        assert_eq!(assembled.tools[0].declaration().name, "read_worker_report");
        assert_eq!(*announced.lock().unwrap(), ["read_worker_report"]);
    }

    /// S1.5.1: the real registration declares a tool package's verified grants under the
    /// identity the loader built, so a child assembled with the package is treated exactly as
    /// one assembled with its native twin. The fixture package grants `process`, so its
    /// assembled tool carries `records-command-evidence` and a child's completion policy is
    /// the strict `CompletionPolicy::RecordedCommands` (ADR-0051 item 1, ADR-0083 rule 7).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn registering_a_process_package_makes_it_a_command_tool_for_a_child() {
        let release = Release::with_fixture();
        let (catalog, _processes) =
            registered_catalog(&release, &fixture_lock(&release, "fixture"));
        let assembled = assemble_modules(&catalog, &["fixture"]).expect("the package assembles");

        assert_eq!(
            assembled.tools.len(),
            1,
            "the granted package, and nothing else"
        );
        let module_tool = assembled.tools[0].clone();
        assert_eq!(module_tool.identity().implementation, FIXTURE_NAME);
        assert!(
            carries(
                module_tool.as_ref(),
                SemanticCapability::RecordsCommandEvidence
            ),
            "the registration declared the package's verified `process` grant"
        );
        assert_eq!(
            child_policy(&assembled.tools),
            CompletionPolicy::RecordedCommands
        );
    }

    /// S1.5.1: a tool package WITHOUT the `process` grant gets no capability, so a child
    /// assembled with it stays on `CompletionPolicy::ReportToParent`. The package is a real
    /// built tool component (`p1/worker-result`, granted `control` and `workers-observe`)
    /// registered through the same host entry point, so the case is the registration's
    /// declaration and not a hand-written one.
    #[test]
    fn registering_a_package_without_process_is_not_a_command_tool() {
        let mut release = Release::empty();
        let (entry, bytes) = built_package("p1-module-worker-result", "p1/worker-result");
        release.add(entry.clone(), &bytes);
        let lock = ModulesLock::parse(
            &release.root().join("modules.lock"),
            &lock_text("worker_result", &entry),
        )
        .expect("lock");
        let mut catalog = Catalog::new();
        let packages =
            load_locked_modules(&lock, &release.manifest_file()).expect("the package loads");
        let services: ModuleServices = Arc::new(|_: &str, _: &ToolServices| Services::default());
        register_modules(&mut catalog, packages, services).expect("registration");

        let identity = ToolIdentity {
            implementation: "p1/worker-result".into(),
            variant: "default".into(),
        };
        assert_eq!(
            declared(&identity),
            Capabilities::NONE,
            "no `process` grant, so no `records-command-evidence`"
        );
        let tool: Arc<dyn Tool> = Arc::new(
            FakeTool::new("worker_result")
                .with_identity(&identity.implementation, &identity.variant),
        );
        assert_eq!(child_policy(&[tool]), CompletionPolicy::ReportToParent);
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
        built_entry("p1-module-read")
    }

    /// The release entry of the built package `dir` (`p1-module-read`, `p1-module-shell`, …): the
    /// name, digest, class, world, protocol, grants and variant of the package manifest
    /// `scripts/build-modules.sh` wrote beside its component.
    fn built_entry(dir: &str) -> p1_contracts::serde_json::Value {
        use p1_contracts::serde_json::{Value, json};

        let manifest: Value = p1_contracts::serde_json::from_slice(
            &std::fs::read(Path::new(env!("CARGO_MANIFEST_DIR")).join(format!(
                "../../modules/target/p1-modules/{dir}/{dir}.manifest.json"
            )))
            .unwrap_or_else(|error| {
                panic!(
                    "the built {dir} package manifest is missing ({error}): run \
                     scripts/build-modules.sh --all first"
                )
            }),
        )
        .expect("the package manifest is JSON");
        json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("packages/{dir}/{dir}.wasm"),
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
        let wasm = built_read_wasm();
        for entry in entries {
            let component = dir.join(entry["path"].as_str().expect("entry path"));
            std::fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
            std::fs::write(&component, &wasm).expect("component");
        }
        write_release_manifest(dir, entries)
    }

    /// Writes a release manifest under `dir` holding `entries`: enough for the cases that read
    /// the release's IDENTITIES (`module_sources_from`) and compile no package.
    fn write_release_manifest(dir: &Path, entries: &[p1_contracts::serde_json::Value]) -> PathBuf {
        use p1_contracts::serde_json::json;

        let manifest = dir.join(RELEASE_MANIFEST_FILE);
        std::fs::write(
            &manifest,
            json!({ "format": "p1-release-manifest/1", "components": entries }).to_string(),
        )
        .expect("release manifest");
        manifest
    }

    /// S3.8 (D083b, D-XO-49): a release that does not carry the package a host entry names fails
    /// the build with the loader's own refusal, naming the key, the package and the manifest it
    /// looked in — never a silent native fallback. The shell and the finish are loaded by this
    /// same step (`load_host_entry`), so their refusals name them exactly as `read`'s does.
    #[test]
    fn a_package_the_release_does_not_carry_fails_naming_the_key() {
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_release_manifest(release.path(), &[]);
        let error = match load_host_entry("shell", "p1/shell", &manifest) {
            Err(error) => error.to_string(),
            Ok(_) => panic!("a release that does not carry p1/shell must refuse"),
        };
        assert!(error.contains("shell"), "{error}");
        assert!(error.contains("p1/shell"), "{error}");
        assert!(error.contains(&manifest.display().to_string()), "{error}");
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
            &[("read", "p1/read")],
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
        let sources = module_sources_from(&deps, &[("read", "p1/read")], Some(empty_manifest))
            .expect("a lock-selected key needs no release");
        let resolved = sources.resolve("read").expect("the lock resolves the key");
        assert_eq!(resolved.name, p1_module_tests::FIXTURE_NAME);
        assert_eq!(resolved.digest, entry["digest"].as_str().expect("digest"));
    }

    /// A user lock that names `shell` hands the LOCK's package to the host's own registration,
    /// which links the process service the shared locked-module services lack; the locked-module
    /// registration leaves the key alone, so the two never collide.
    #[test]
    fn a_lock_selected_shell_reaches_the_host_registration() {
        let entry = built_entry("p1-module-shell");
        let release = tempfile::tempdir().expect("release dir");
        let wasm = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules/p1-module-shell/p1-module-shell.wasm");
        let component = release
            .path()
            .join(entry["path"].as_str().expect("entry path"));
        std::fs::create_dir_all(component.parent().expect("package dir")).expect("package dir");
        std::fs::copy(&wasm, &component).expect("the built p1-module-shell component");
        let manifest = write_release_manifest(release.path(), std::slice::from_ref(&entry));
        let (config, dirs) = config_without_lock();
        std::fs::write(
            config.path().join("modules.lock"),
            p1_module_tests::lock_text("shell", &entry),
        )
        .expect("lock");
        let deps = quiet_deps(dirs);

        let handed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let recorded = handed.clone();
        let registration: HostEntryRegistration =
            Box::new(move |_catalog: &mut Catalog, module: Arc<LoadedModule>| {
                recorded.lock().unwrap().push(module);
                Ok(())
            });
        let mut catalog = Catalog::new();
        register_entries_from(
            &mut catalog,
            &deps,
            &[("shell", "p1/shell")],
            Some(manifest.clone()),
            &[("shell", registration)],
        )
        .expect("the lock's p1/shell loads");
        let handed = handed.lock().unwrap();
        assert_eq!(handed.len(), 1, "the lock's package is handed over once");
        assert_eq!(handed[0].identity().implementation, "p1/shell");

        register_locked_modules_from(&mut catalog, &deps, Some(manifest))
            .expect("the locked-module registration leaves the key alone");
        assert!(catalog.tool_keys().is_empty(), "no shared registration");
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
        register_host_entries_from(
            &mut catalog,
            &deps,
            &[("read", "p1/read")],
            Some(manifest.clone()),
        )
        .expect("the release's p1/read registers");
        assert_eq!(catalog.tool_keys(), ["read"]);

        let sources = module_sources_from(&deps, &[("read", "p1/read")], Some(manifest))
            .expect("the release reads");
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

    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn lock_selected_worker_member_keeps_its_selected_package_identity() {
        let manifest = official_release_manifest().expect("official release");
        let value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(manifest).unwrap()).unwrap();
        let entry = value["components"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["name"] == "p1/read")
            .unwrap();
        let (config, dirs) = config_without_lock();
        std::fs::write(
            config.path().join("modules.lock"),
            lock_text("worker_start", entry),
        )
        .unwrap();
        let mut deps = quiet_deps(dirs);
        deps.worker_service = Some(p1_workers::InProcessWorkers::new(
            Arc::new(|_| Err("unused worker factory".to_owned())),
            1,
        ));
        let completion = Arc::new(crate::activity::CompletionHub::new());
        let catalog = super::super::build_catalog(
            &deps,
            crate::cli::SandboxMode::Off,
            &[],
            &[],
            &[],
            &completion,
        )
        .expect("selected member catalog");
        assert!(catalog.tool_keys().contains(&"worker_start".to_owned()));
        let source = deps
            .verified_sources
            .resolve("worker_start")
            .expect("selected source");
        assert_eq!(source.name, "p1/read");
        assert_eq!(source.digest, entry["digest"].as_str().unwrap());
    }

    #[cfg(feature = "delegation")]
    #[tokio::test]
    async fn catalog_worker_schema_includes_a_real_lock_selected_tool() {
        let path = official_release_manifest().expect("official release path");
        let manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(path).expect("release")).expect("manifest JSON");
        let entry = manifest["components"]
            .as_array()
            .expect("components")
            .iter()
            .find(|entry| entry["name"] == "p1/read")
            .expect("released read");
        let (config, dirs) = config_without_lock();
        std::fs::write(
            config.path().join("modules.lock"),
            lock_text("extra_locked_tool", entry),
        )
        .expect("real modules.lock");
        let mut deps = quiet_deps(dirs);
        deps.worker_service = Some(p1_workers::InProcessWorkers::new(
            Arc::new(|_| Err("unused worker factory".to_owned())),
            1,
        ));
        let provider = Arc::new(ScriptedProvider::new(Vec::new()));
        deps.catalog_hook = Some(Box::new(move |catalog| {
            let provider = provider.clone();
            catalog.provider(
                "scripted",
                Box::new(move |_| Ok(provider.clone() as Arc<dyn Provider>)),
            );
        }));
        let completion = Arc::new(crate::activity::CompletionHub::new());
        let catalog = super::super::build_catalog(
            &deps,
            crate::cli::SandboxMode::Off,
            &[],
            &[],
            &[],
            &completion,
        )
        .expect("catalog with locked tool and worker member");
        let assembled = assemble_modules(&catalog, &["worker_start"])
            .expect("worker_start assembles with its grant list");
        let p1_contracts::DeclarationKind::Function { input_schema } =
            &assembled.tools[0].declaration().kind
        else {
            panic!("worker_start has a function schema")
        };
        let tools = &input_schema["properties"]["tools"]["items"]["enum"];
        assert!(
            tools
                .as_array()
                .expect("grantable schema enum")
                .iter()
                .any(|tool| tool == "extra_locked_tool"),
            "locked key missing: {tools}"
        );
    }

    #[cfg(feature = "delegation")]
    #[test]
    fn a_real_lock_selected_tool_enters_worker_grantable_keys() {
        let mut release = Release::empty();
        let (entry, bytes) = built_package("p1-module-read", "p1/read");
        release.add(entry.clone(), &bytes);
        let (config, dirs) = config_without_lock();
        std::fs::write(
            config.path().join("modules.lock"),
            lock_text("extra_locked_tool", &entry),
        )
        .expect("real modules.lock entry");
        let deps = quiet_deps(dirs);
        let mut catalog = Catalog::new();
        register_locked_modules_from(&mut catalog, &deps, Some(release.manifest_file()))
            .expect("locked package is registered before worker lists");
        let lists = super::super::delegation::worker_lists(&catalog, &deps).expect("worker lists");
        assert!(lists.grantable.contains(&"extra_locked_tool".to_owned()));
    }

    #[test]
    fn replacing_release_in_place_loads_new_bytes_in_new_generation() {
        let release = tempfile::tempdir().expect("release dir");
        let first_entry = read_entry();
        let manifest = write_read_release(release.path(), &[first_entry]);
        let (_config, dirs) = config_without_lock();
        let old = quiet_deps(dirs.clone());
        register_host_entries_from(
            &mut Catalog::new(),
            &old,
            &[("read", "p1/read")],
            Some(manifest.clone()),
        )
        .expect("first generation");
        let before = module_sources(&old)
            .unwrap()
            .resolve("read")
            .unwrap()
            .digest;

        let mut next_entry = built_entry("p1-module-write");
        next_entry["name"] = "p1/read".into();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules/p1-module-write/p1-module-write.wasm");
        let target = release.path().join(next_entry["path"].as_str().unwrap());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, target).expect("replacement bytes");
        write_release_manifest(release.path(), &[next_entry.clone()]);
        let new = quiet_deps(dirs);
        register_host_entries_from(
            &mut Catalog::new(),
            &new,
            &[("read", "p1/read")],
            Some(manifest),
        )
        .expect("new generation");
        let after = module_sources(&new)
            .unwrap()
            .resolve("read")
            .unwrap()
            .digest;
        assert_ne!(before, after);
        assert_eq!(after, next_entry["digest"].as_str().unwrap());
        assert_eq!(
            module_sources(&old)
                .unwrap()
                .resolve("read")
                .unwrap()
                .digest,
            before
        );
    }

    #[test]
    fn same_build_rejects_a_lock_from_a_replaced_manifest_snapshot() {
        let release = tempfile::tempdir().expect("scratch release");
        let old = read_entry();
        let path = write_read_release(release.path(), &[old]);
        let loaders = BuildLoaders::default();
        let snapshot = loaders.manifest_for(&path).expect("first snapshot");
        let loader = loaders
            .for_release(&path, snapshot.clone())
            .expect("first loader");
        let mut replacement = built_entry("p1-module-write");
        replacement["name"] = "p1/read".into();
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules/p1-module-write/p1-module-write.wasm");
        let target = release.path().join(replacement["path"].as_str().unwrap());
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::copy(source, target).unwrap();
        write_release_manifest(release.path(), &[replacement.clone()]);
        let lock = ModulesLock::parse(
            &release.path().join("modules.lock"),
            &lock_text("read", &replacement),
        )
        .unwrap();
        let pinned = loaders.manifest_for(&path).expect("same build snapshot");
        assert_eq!(pinned, snapshot);
        assert!(
            matches!(
                load_locked_entry(&loader, &pinned, "read", lock.resolve("read").unwrap()),
                Err(ModulesError::LockMismatch { .. })
            ),
            "B lock cannot validate against A loader"
        );
    }

    #[test]
    fn a_build_reuses_one_loader_but_a_new_build_gets_a_new_one() {
        let entry = read_entry();
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_read_release(release.path(), &[entry]);
        let parsed = ReleaseManifest::read(&manifest).expect("manifest");
        let build = BuildLoaders::default();
        let first = build
            .for_release(&manifest, parsed.clone())
            .expect("loader");
        let second = build
            .for_release(&manifest, parsed.clone())
            .expect("reused loader");
        assert!(
            Arc::ptr_eq(&first, &second),
            "one loader, and so one manifest snapshot, for this build"
        );
        let old = first.load("p1/read").expect("old module");
        let next = BuildLoaders::default();
        let replacement = next
            .for_release(&manifest, parsed)
            .expect("next build loader");
        assert!(!Arc::ptr_eq(&first, &replacement));
        assert_eq!(
            old.digest(),
            replacement.load("p1/read").expect("next module").digest()
        );
    }

    #[test]
    fn verified_identity_survives_manifest_replacement_after_load() {
        let entry = read_entry();
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_read_release(release.path(), std::slice::from_ref(&entry));
        let (_config, dirs) = config_without_lock();
        let deps = quiet_deps(dirs);
        let mut catalog = Catalog::new();
        register_host_entries_from(
            &mut catalog,
            &deps,
            &[("read", "p1/read")],
            Some(manifest.clone()),
        )
        .expect("verified load");
        std::fs::write(&manifest, "{\"components\":[]}").expect("replace manifest");
        let resolved = module_sources(&deps)
            .expect("verified sources")
            .resolve("read")
            .expect("loaded read");
        assert_eq!(resolved.digest, entry["digest"].as_str().expect("digest"));
        assert_eq!(
            resolved.abi,
            format!(
                "{}+{}",
                entry["world"].as_str().unwrap(),
                entry["protocol"].as_str().unwrap()
            )
        );
    }

    /// S7.10-R1 (ADR-0095): the five file tools are release host entries — `edit`, `write`,
    /// `apply_patch` and `grep` beside `read` — so a key no lock names runs the release's
    /// component, and a release that does not carry one fails the build naming the key and the
    /// package, exactly as a missing `read` does. No compiled-in registration answers for any of
    /// them any more, so such a release has nothing to fall back to.
    #[test]
    fn the_file_tool_keys_are_release_host_entries() {
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_release_manifest(release.path(), &[]);

        for (key, package) in [
            ("read", "p1/read"),
            ("edit", "p1/edit"),
            ("write", "p1/write"),
            ("apply_patch", "p1/patch"),
            ("grep", "p1/search"),
        ] {
            assert!(
                HOST_ENTRIES.contains(&(key, package)),
                "`{key}` is the release host entry of `{package}`"
            );
            let error = match load_host_entry(key, package, &manifest) {
                Err(error) => error.to_string(),
                Ok(_) => panic!("a release that does not carry {package} must refuse"),
            };
            assert!(error.contains(key), "{error}");
            assert!(error.contains(package), "{error}");
            assert!(error.contains(&manifest.display().to_string()), "{error}");
        }
    }

    /// S3.8 (D083b, D-XO-49): `shell` and `finish` are HOST ENTRIES beside `read`, so the identity
    /// builder names the release package that ran for each key — the manifest name, the digest the
    /// release pins and the version the host runs at — exactly as it does for `read`. Before this
    /// slice both keys were journaled as native modules with a null digest although the release's
    /// `p1/shell` and `p1/finish` components were what executed.
    #[test]
    fn the_shell_and_finish_host_entries_name_their_release_packages() {
        let package = |key: &str| {
            HOST_ENTRIES
                .iter()
                .find(|(entry, _)| *entry == key)
                .unwrap_or_else(|| panic!("`{key}` is a host entry"))
                .1
        };
        let mut entries = Vec::new();
        for (key, dir) in [("shell", "p1-module-shell"), ("finish", "p1-module-finish")] {
            let entry = built_entry(dir);
            assert_eq!(entry["name"], package(key), "the key's release package");
            entries.push(entry);
        }
        let release = tempfile::tempdir().expect("release dir");
        let manifest = write_release_manifest(release.path(), &entries);
        let (_config, dirs) = config_without_lock();
        let deps = quiet_deps(dirs);

        let sources =
            module_sources_from(&deps, &HOST_ENTRIES, Some(manifest)).expect("the release reads");
        for (key, entry) in [("shell", &entries[0]), ("finish", &entries[1])] {
            let resolved = sources
                .resolve(key)
                .expect("the host entry resolves its key");
            assert_eq!(
                resolved.name,
                entry["name"].as_str().expect("name"),
                "{key}"
            );
            assert_eq!(
                resolved.digest,
                entry["digest"].as_str().expect("digest"),
                "{key}"
            );
            assert_eq!(resolved.version, env!("CARGO_PKG_VERSION"), "{key}");
            assert_eq!(
                resolved.abi,
                format!(
                    "{}+{}",
                    entry["world"].as_str().expect("world"),
                    entry["protocol"].as_str().expect("protocol")
                ),
                "{key}"
            );
        }
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
            profile_text: None,
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
