//! The WebAssembly package loader's catalog entry point (ADR-0071, ADR draft "Module
//! identity and verified loading").
//!
//! An environment names a module by its `[[tools]] module` key. A key no compiled-in
//! tool claims is resolved through `modules.lock` ([`p1_assembly::load_modules_lock`]) to
//! one official package of p1's release; the [`loader`] verifies and compiles it, and
//! [`register_modules`] registers it in the catalog under that key. Registration builds no
//! instance: the catalog's factory runs only when an environment assembles the key, so an
//! installed but unselected package and an invented name both never dispatch (the
//! assembly rule).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_assembly::{Catalog, ModulesLock, ToolServices, ToolSpec, load_modules_lock};
use p1_contracts::Tool;

use crate::HostDeps;

pub mod loader;

pub use loader::{
    DigestRecord, LoadError, PackageManifest, RELEASE_MANIFEST_FILE, RELEASE_MANIFEST_FORMAT,
    Release, VerifiedModule,
};

/// Turns one verified module into a tool for one agent. Called only by the catalog
/// factory, i.e. only when an environment assembles the module.
pub type ToolAdapter = Arc<
    dyn Fn(&VerifiedModule, &ToolSpec, &ToolServices) -> Result<Arc<dyn Tool>, String>
        + Send
        + Sync,
>;

/// The generic tool adapter the host uses for tool packages.
// DRAFT(tag): the tag must provide S0's generic tool adapter `WasmTool` in
// `p1-module-runtime` (freeze item 12): built from the verified `Component`, the
// loader-built `ToolIdentity`, the manifest's granted capabilities (the host links only
// those) and the agent's `ToolServices`, with the `ToolSpec` face applied. This seam
// then becomes a call to it; until then selecting a module package fails assembly with
// this message, and nothing is instantiated.
pub fn wasm_tool_adapter() -> ToolAdapter {
    Arc::new(
        |module: &VerifiedModule, _spec: &ToolSpec, _services: &ToolServices| {
            Err(format!(
                "module `{}` ({} {}) is verified, but this build has no WebAssembly tool \
                 adapter (WasmTool, freeze item 12)",
                module.module, module.manifest.name, module.digest
            ))
        },
    )
}

/// Where p1's own release keeps its module set: `<exe dir>/../share/p1/modules`, next to
/// the shipped `environments/`. Never a configuration directory, so an override can only
/// select among what the release ships.
// DRAFT(tag): S7 (ADR-0079) fixes the installed layout; if the tag or S7 moves the module
// root or passes it in (e.g. through `HostDeps`), take it from there.
pub fn official_release_dir() -> Option<PathBuf> {
    let exe = std::env::current_exe().ok()?;
    Some(exe.parent()?.join("../share/p1/modules"))
}

/// Verify and compile every package `lock` resolves, from `release_dir`. Stops at the
/// first refusal.
pub fn load_locked_modules(
    lock: &ModulesLock,
    release_dir: &Path,
) -> Result<Vec<VerifiedModule>, LoadError> {
    let engine = p1_module_runtime::engine().map_err(|error| LoadError::Engine {
        message: error.to_string(),
    })?;
    let release = Release::open(release_dir)?;
    lock.iter()
        .map(|(module, locked)| release.load(&engine, module, locked))
        .collect()
}

/// Register each verified module under its module name. A name a registered tool
/// already has is refused: a package never silently replaces a compiled-in tool. Only
/// `tool` packages are registered here.
pub fn register_modules(
    catalog: &mut Catalog,
    modules: Vec<VerifiedModule>,
    adapter: ToolAdapter,
) -> Result<(), String> {
    let existing = catalog.tool_keys();
    for module in modules {
        if module.manifest.kind != "tool" {
            // DRAFT(tag): provider and policy packages register through S4's and S5's
            // adapters once they exist; until then a lock naming one is refused.
            return Err(format!(
                "module `{}` is a `{}` package; only tool packages can be registered yet",
                module.module, module.manifest.kind
            ));
        }
        if existing.contains(&module.module) {
            return Err(format!(
                "module `{}` from modules.lock collides with a compiled-in tool of the same name",
                module.module
            ));
        }
        let key = module.module.clone();
        let adapter = adapter.clone();
        let module = Arc::new(module);
        catalog.tool(
            &key,
            Box::new(move |spec: &ToolSpec, services: &ToolServices| {
                adapter(&module, spec, services)
            }),
        );
    }
    Ok(())
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
    let release_dir = official_release_dir().ok_or_else(|| {
        "cannot locate p1's release module set: the executable path is unknown".to_string()
    })?;
    let modules = load_locked_modules(&lock, &release_dir).map_err(|error| error.to_string())?;
    register_modules(catalog, modules, wasm_tool_adapter())
}
