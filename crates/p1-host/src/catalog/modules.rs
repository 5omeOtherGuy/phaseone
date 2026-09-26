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
use p1_redact::MaskCounter;
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
/// assembles the module.
pub type ModuleServices = Arc<dyn Fn(&ToolServices) -> Services + Send + Sync>;

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
    let manifest =
        ReleaseManifest::read(release_manifest).map_err(|source| ModulesError::Release {
            path: release_manifest.to_owned(),
            source: Box::new(source),
        })?;
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
    // The agent's own redacting wrapper (`run.rs`) holds the count the turn reports; this
    // counter only satisfies `wasm_tool`, which never hands out an unwrapped module tool.
    let counter = Arc::new(MaskCounter::new());
    wasm_tool(
        &package.loaded,
        services(tool_services),
        ExecutionLimits::default(),
        &counter,
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
    let lock = load_modules_lock(&deps.environment_dirs).map_err(|error| error.to_string())?;
    if lock.is_empty() {
        return Ok(());
    }
    let release = official_release_manifest().ok_or_else(|| ModulesError::NoRelease.to_string())?;
    let packages = load_locked_modules(&lock, &release).map_err(|error| error.to_string())?;
    // No native service backs a module capability in the host yet (the shell's process
    // service is not bridged to the runtime's `ProcessService`), so a package granted one
    // fails its assembly with the runtime's `MissingService` rather than running unlinked.
    let services: ModuleServices = Arc::new(|_: &ToolServices| Services::default());
    register_modules(catalog, packages, services).map_err(|error| error.to_string())
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
}
