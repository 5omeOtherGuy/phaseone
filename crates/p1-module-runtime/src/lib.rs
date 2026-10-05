//! The host side of p1's WebAssembly modules (ADR-0071): the wasmtime engine that compiles
//! module components ahead of use and runs them asynchronously.
//!
//! This crate is the only place the native host touches wasmtime:
//! - [`manifest`]: the release manifest, the one source modules load from;
//! - [`loader`]: verify-then-compile-same-bytes, the digest as identity (freeze item 6);
//! - [`capabilities`]: the host imports linked per the manifest's grants;
//! - [`file_services`]: the capability services of the file tools — the read side
//!   (`workspace`, `snapshot`), the walk (`list-files`, `search`) and the owned mutation
//!   (`workspace-mutation`) — a component is linked with, and [`file_walk`]: the walk itself,
//!   over the ripgrep crates (S7.10-R1, ADR-0095);
//! - [`completion`]: the `completion` import, linked to the host's completion hub;
//! - [`executor`]: the one owner of a module's async Stores (ADR-0015);
//! - [`process`]: the native process service and sandbox behind the `process` capability;
//! - [`outputs`]: the `tool-outputs` import and the host's store of what processes printed,
//!   written before the process output is cut (ADR-0109);
//! - [`restricted`]: the synchronous inspection path (freeze item 4);
//! - [`tool`]: `WasmTool`, the generic tool adapter (freeze item 12);
//! - [`workflow_decision`]: `WasmWorkflowDecisions`, a workflow's decisions as a component.

pub mod authorization_policy;
pub mod capabilities;
pub mod completion;
pub mod context_policy;
pub mod delegation;
pub mod executor;
pub mod file_services;
pub mod file_walk;
pub mod loader;
pub mod manifest;
pub mod outputs;
pub mod process;
pub mod provider;
pub mod questions;
pub mod restricted;
mod sha256;
pub mod tool;
pub mod workflow_decision;

pub use authorization_policy::{AuthorizationPolicyError, Verdict, WasmAuthorizationPolicy};
pub use capabilities::{
    CallScope, EntryKind, ExitStatus, FsError, LinkError, ProcessCommand, ProcessEvent,
    ProcessService, RunningProcess, Services, SnapshotObservation, SnapshotService, WorkspaceEntry,
    WorkspaceService,
};
pub use completion::CompletionService;
pub use context_policy::{
    ContextPolicyError, SummaryError, SummaryRequest, SummaryResponse, SummaryService,
    WasmContextPolicy,
};
pub use executor::ExecutionLimits;
pub use loader::{
    LINKABLE_CAPABILITIES, LoadError, LoadedModule, Loader, ManualEpochs, ModuleKind,
};
pub use manifest::{ComponentEntry, Digest, ManifestError, Precompiled, ReleaseManifest};
pub use outputs::{CallOutputs, OutputCaps, OutputStore, ToolOutputsService};
pub use provider::{ProviderError, ProviderSettings, WasmProvider};
pub use tool::{ToolError, WasmTool, wasm_tool};
pub use workflow_decision::{WasmWorkflowDecisions, WorkflowDecisionError};

use thiserror::Error;
use wasmtime::{Config, Engine};

/// Why the runtime could not be set up.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// wasmtime refused the engine configuration, for example because the host CPU lacks a
    /// feature the compiler needs.
    #[error("cannot create the wasmtime engine: {0:#}")]
    Engine(wasmtime::Error),
    /// The thread that advances the engine's epoch could not start.
    #[error("cannot start the module epoch thread: {0}")]
    Ticker(std::io::Error),
    /// A component could not be compiled ahead of time.
    #[error("cannot compile the component ahead of time: {0:#}")]
    Precompile(wasmtime::Error),
}

/// Builds the engine every module is compiled and instantiated with.
///
/// - The component model is on because modules are components, never core modules.
/// - Async support comes with wasmtime's `async` feature (wasmtime 49 has no switch for it)
///   because module calls run inside the host's async turn loop and must not block its
///   executor.
/// - Epoch interruption is on so the host can bound a call's wall time from outside, and fuel
///   consumption so it can bound the work a call does; either way a runaway module traps
///   instead of holding the agent.
pub fn engine() -> Result<Engine, RuntimeError> {
    Engine::new(&config()).map_err(RuntimeError::Engine)
}

fn config() -> Config {
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .epoch_interruption(true)
        .consume_fuel(true);
    config
}

/// The target a release's components are compiled ahead of time for (ADR-0113). Naming it
/// explicitly keeps wasmtime from adding the build machine's CPU features, so the compiled
/// code needs nothing beyond baseline x86-64 and runs on every x86-64 Linux host.
pub const PRECOMPILE_TARGET: &str = "x86_64-unknown-linux-gnu";

/// Compiles the component `bytes` ahead of time with [`engine`]'s configuration for
/// [`PRECOMPILE_TARGET`]: the `<package>.cwasm` a release ships beside `<package>.wasm`. The
/// loader deserializes it only when its digest is the release manifest's, and wasmtime
/// refuses it on a host or build whose engine it does not fit, which the loader answers by
/// compiling the verified component instead.
pub fn precompile(bytes: &[u8]) -> Result<Vec<u8>, RuntimeError> {
    let mut config = config();
    config
        .target(PRECOMPILE_TARGET)
        .map_err(RuntimeError::Precompile)?;
    let engine = Engine::new(&config).map_err(RuntimeError::Precompile)?;
    engine
        .precompile_component(bytes)
        .map_err(RuntimeError::Precompile)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn engine_is_built_with_fuel_consumption() {
        let engine = engine().expect("engine");
        // A store only accepts fuel when the engine was configured to consume it.
        let mut store = wasmtime::Store::new(&engine, ());
        store.set_fuel(1).expect("fuel is enabled");
        assert_eq!(store.get_fuel().expect("fuel is enabled"), 1);
    }
}
