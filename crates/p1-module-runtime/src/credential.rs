//! `WasmCredentialMethod`: the adapter from one loaded `credential-method` component (world
//! `p1:module/credential-method`, ADR-0212) to the five pure functions the host's credential
//! broker asks it: what it provides, what a login file holds, the refresh request, what a
//! refresh response grants and the login file with a refresh written into it.
//!
//! The class imports nothing: no network, no files, no clock, no randomness. The loader refuses
//! a component that imports anything (`ImportsExceedAllocation`), so the linker here is empty.
//! Every call is synchronous, on a fresh instance, bounded by fuel and an epoch deadline, the way
//! [`crate::workflow_decision`] runs its decisions: a call is pure and short, and nothing in it
//! waits. Answers are passed through as the component wrote them; reading them is the broker's.

use std::sync::Arc;

use thiserror::Error;
use wasmtime::Engine;
use wasmtime::component::{ComponentNamedList, ComponentType, InstancePre, Lift, Linker, Lower};

use crate::executor::ExecutionLimits;
use crate::loader::{EPOCH_TICK, Epochs, LoadedModule, ModuleKind};
use crate::manifest::Digest;

/// Why a loaded module could not become a credential method.
#[derive(Debug, Error)]
pub enum CredentialMethodError {
    /// The module is not of the `credential-method` class.
    #[error("module {name} is a {kind} module, not a credential method")]
    NotACredentialMethod {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// The component does not fit the `credential-method` world.
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

/// The adapter over one loaded credential-method component.
pub struct WasmCredentialMethod {
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

impl WasmCredentialMethod {
    /// The credential method of `module`, linked with nothing.
    pub fn new(
        module: &LoadedModule,
        limits: ExecutionLimits,
    ) -> Result<Self, CredentialMethodError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::CredentialMethod {
            return Err(CredentialMethodError::NotACredentialMethod {
                name,
                kind: module.kind().name(),
            });
        }
        let pre = Linker::<CallData>::new(&module.engine)
            .instantiate_pre(&module.component)
            .map_err(|error| CredentialMethodError::Instantiate {
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

    /// The package's manifest name, e.g. `p1/codex-oauth`.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The digest of the component's verified bytes: its identity.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The https origins the verified manifest lets this method's refresh requests go to.
    pub fn token_origins(&self) -> &[String] {
        &self.token_origins
    }

    /// What the module provides, as the JSON text it wrote.
    pub fn describe(&self) -> Result<String, String> {
        self.call::<(), (String,)>("describe", ())
            .map(|(text,)| text)
    }

    /// The credential `file` holds (JSON), or the module's reason it holds none usable.
    pub fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String> {
        self.call::<(Vec<u8>, Option<String>), (Result<String, String>,)>(
            "read-login",
            (file.to_vec(), key.map(str::to_owned)),
        )?
        .0
    }

    /// The refresh request (JSON) that spends `refresh_token`, read from `file` when it is a
    /// borrowed login.
    pub fn refresh_request(
        &self,
        refresh_token: &str,
        file: Option<&[u8]>,
    ) -> Result<String, String> {
        self.call::<(String, Option<Vec<u8>>), (Result<String, String>,)>(
            "refresh-request",
            (refresh_token.to_owned(), file.map(<[u8]>::to_vec)),
        )?
        .0
    }

    /// What the refresh response `body` grants (JSON), its expiry counted from `sent_at_ms`.
    pub fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> Result<String, String> {
        self.call::<(Vec<u8>, u64), (String,)>("refresh-response", (body.to_vec(), sent_at_ms))
            .map(|(text,)| text)
    }

    /// The login `file` with the refresh response `body` written into it.
    pub fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        self.call::<(Vec<u8>, Vec<u8>, u64, u64, bool), (Result<Vec<u8>, String>,)>(
            "merge-login",
            (
                file.to_vec(),
                body.to_vec(),
                sent_at_ms,
                now_ms,
                keep_access,
            ),
        )?
        .0
    }

    /// Calls `export` once on a fresh instance. A trap, running out of fuel or time, or an
    /// export of another type is the call's error, naming the module.
    fn call<Params, Results>(&self, export: &str, params: Params) -> Result<Results, String>
    where
        Params: ComponentNamedList + Lower + Send + Sync,
        Results: ComponentNamedList + Lift + ComponentType + Send + Sync,
    {
        let failed = |error: wasmtime::Error| {
            format!(
                "credential module {} failed in {export}: {error:#}",
                self.name
            )
        };
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
