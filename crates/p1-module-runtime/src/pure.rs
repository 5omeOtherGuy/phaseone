//! `PureComponent`: one loaded component of a class that imports nothing (a credential method,
//! ADR-0212; a usage probe, ADR-0214), called as a set of pure functions.
//!
//! The loader refuses a component of such a class that imports anything
//! (`ImportsExceedAllocation`), so the linker here is empty. Every call is synchronous, on a
//! fresh instance, bounded by fuel and an epoch deadline, the way [`crate::workflow_decision`]
//! runs its decisions: a call is pure and short, and nothing in it waits. Answers are passed
//! through as the component wrote them; reading them is the caller's.

use std::sync::Arc;

use thiserror::Error;
use wasmtime::Engine;
use wasmtime::component::{ComponentNamedList, ComponentType, InstancePre, Lift, Linker, Lower};

use crate::executor::ExecutionLimits;
use crate::loader::{EPOCH_TICK, Epochs, LoadedModule, ModuleKind};
use crate::manifest::Digest;

/// Why a loaded module could not become a pure component of its class.
#[derive(Debug, Error)]
pub enum PureModuleError {
    /// The module is not of the class asked for.
    #[error("module {name} is a {kind} module, not a {expected} module")]
    WrongClass {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
        /// The class asked for.
        expected: &'static str,
    },
    /// The component does not fit its world.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
}

/// What one call's Store holds: only the memory limits, since nothing is imported.
struct CallData {
    limits: crate::executor::MemoryLimiter,
}

impl crate::executor::LimitedStore for CallData {
    fn limits(&mut self) -> &mut crate::executor::MemoryLimiter {
        &mut self.limits
    }
}

/// One loaded component of a class that imports nothing.
pub(crate) struct PureComponent {
    name: String,
    digest: Digest,
    token_origins: Vec<String>,
    engine: Engine,
    pre: InstancePre<CallData>,
    fuel: u64,
    deadline_ticks: u64,
    /// The clock the deadlines count; held, because the adapter may outlive its loader.
    epochs: Arc<Epochs>,
}

impl PureComponent {
    /// The component of `module`, which must be of class `kind`, linked with nothing.
    pub(crate) fn new(
        module: &LoadedModule,
        kind: ModuleKind,
        limits: ExecutionLimits,
    ) -> Result<Self, PureModuleError> {
        let name = module.name().to_owned();
        if module.kind() != kind {
            return Err(PureModuleError::WrongClass {
                name,
                kind: module.kind().name(),
                expected: kind.name(),
            });
        }
        let pre = Linker::<CallData>::new(&module.engine)
            .instantiate_pre(&module.component)
            .map_err(|error| PureModuleError::Instantiate {
                name: name.clone(),
                reason: format!("{error:#}"),
            })?;
        // At least one tick: a zero deadline would stop every call before it starts.
        let deadline_ticks = u64::try_from(limits.deadline.as_millis() / EPOCH_TICK.as_millis())
            .unwrap_or(u64::MAX)
            .max(1);
        Ok(Self {
            name,
            digest: module.digest(),
            token_origins: module.token_origins().to_vec(),
            engine: module.engine.clone(),
            pre,
            fuel: limits.fuel,
            deadline_ticks,
            epochs: module.epochs.clone(),
        })
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn digest(&self) -> Digest {
        self.digest
    }

    pub(crate) fn token_origins(&self) -> &[String] {
        &self.token_origins
    }

    /// Calls `export` once on a fresh instance. A trap, running out of fuel or time, or an
    /// export of another type is the call's error, naming the module and its class.
    pub(crate) fn call<Params, Results>(
        &self,
        export: &str,
        params: Params,
    ) -> Result<Results, String>
    where
        Params: ComponentNamedList + Lower + Send + Sync,
        Results: ComponentNamedList + Lift + ComponentType + Send + Sync,
    {
        let failed =
            |error: wasmtime::Error| format!("module {} failed in {export}: {error:#}", self.name);
        let mut store = crate::executor::module_store(
            &self.engine,
            CallData {
                limits: crate::executor::store_limits(),
            },
        );
        store.set_fuel(self.fuel).map_err(failed)?;
        self.epochs
            .arm_deadline(&mut store, self.deadline_ticks, || {
                wasmtime::Error::new(wasmtime::Trap::Interrupt)
            });
        let instance = self.pre.instantiate(&mut store).map_err(failed)?;
        let func = instance
            .get_typed_func::<Params, Results>(&mut store, export)
            .map_err(failed)?;
        func.call(&mut store, params).map_err(failed)
    }
}
