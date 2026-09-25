//! The host side of p1's WebAssembly modules (ADR-0071): the wasmtime engine that compiles
//! module components ahead of use and runs them asynchronously.
//!
//! This crate is the only place the native host touches wasmtime; the loader, linker and
//! call paths build on the [`engine`] configured here.

use thiserror::Error;
use wasmtime::{Config, Engine};

/// Why the runtime could not be set up.
#[derive(Debug, Error)]
pub enum RuntimeError {
    /// wasmtime refused the engine configuration, for example because the host CPU lacks a
    /// feature the compiler needs.
    #[error("cannot create the wasmtime engine: {0:#}")]
    Engine(wasmtime::Error),
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
    let mut config = Config::new();
    config
        .wasm_component_model(true)
        .epoch_interruption(true)
        .consume_fuel(true);
    Engine::new(&config).map_err(RuntimeError::Engine)
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
