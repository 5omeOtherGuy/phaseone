//! Spike: the generic channel host. One WIT world (`modules/p1-bindings-channel/wit/channel.wit`)
//! carries every event (`handle(event, payload)`) and every host call
//! (`call(capability, method, payload)`) by name, with JSON payloads, and large data by handle
//! (`open(name)` -> `buffer`). The catalog (`catalog.rs`) says which names exist; the plugin's
//! manifest says which of them it wants; the environment's grant says which capabilities it
//! gets. Everything else fails closed.
//!
//! Each `emit` runs in its own `Store` and instance, with the store setup of the existing
//! runtime's executor (fuel, fuel yield interval, epoch deadline, hostcall fuel, a memory
//! limit), so its cost compares with `bench-baseline`'s `execute`.

pub mod catalog;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;

use p1_module_runtime::executor::{DEFAULT_FUEL, FUEL_YIELD_INTERVAL, HOSTCALL_FUEL};
use p1_module_runtime::loader::EPOCH_TICK;
use serde::Deserialize;
use thiserror::Error;
use wasmtime::component::{
    Component, ComponentExportIndex, InstancePre, Linker, Resource, ResourceTable, ResourceType,
    Val,
};
use wasmtime::{Engine, Store, StoreContextMut, StoreLimits, StoreLimitsBuilder, UpdateDeadline};

/// The one interface a channel plugin may import.
pub const HOST_INTERFACE: &str = "p1:channel/host@1.0.0";

/// The most bytes one `buffer.read` hands back, whatever the guest asks for.
pub const MAX_READ: u64 = 1 << 20;

/// The guest memory ceiling of one call, as the existing runtime's `MAX_GUEST_MEMORY`.
const MAX_GUEST_MEMORY: usize = 256 << 20;

/// What a host capability does with one call; the environment grants these by name.
pub trait Capability: Send + Sync {
    fn call(&self, method: &str, payload: &str) -> Result<String, String>;
}

/// The capabilities the environment grants, by catalog name.
pub type Grants = BTreeMap<String, Arc<dyn Capability>>;

#[derive(Debug, Error)]
pub enum LoadError {
    #[error("cannot compile the component: {0:#}")]
    Compile(wasmtime::Error),
    #[error("the component imports {0}, which is not {HOST_INTERFACE}")]
    Import(String),
    #[error("cannot link the component: {0:#}")]
    Link(wasmtime::Error),
    #[error("cannot read the manifest: {0}")]
    Manifest(String),
    #[error("the manifest names event {0:?}, which the catalog does not have")]
    UnknownEvent(String),
    #[error("the manifest wants event {event:?} at version {wanted}; the host emits {emitted}")]
    EventVersion {
        event: String,
        wanted: u32,
        emitted: u32,
    },
    #[error("the manifest names capability {0:?}, which the catalog does not have")]
    UnknownCapability(String),
}

#[derive(Debug, Error)]
pub enum CallError {
    #[error("event {0:?} is not in the catalog")]
    UnknownEvent(String),
    #[error("the plugin failed: {0:#}")]
    Trap(wasmtime::Error),
}

/// The plugin's own declaration; an unknown field makes it unreadable.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub events: BTreeMap<String, u32>,
    #[serde(default)]
    pub capabilities: BTreeSet<String>,
}

/// How the host calls `handle`: through wasmtime's typed lowering, or through `Val`s as the
/// existing runtime calls every export.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Calling {
    Typed,
    Dynamic,
}

/// Starts a thread that advances `engine`'s epoch every [`EPOCH_TICK`], as the existing
/// loader does, so a call's epoch deadline behaves as it does there. It ends with the engine.
pub fn start_epoch_ticker(engine: &Engine) {
    let weak = engine.weak();
    thread::spawn(move || {
        loop {
            thread::sleep(EPOCH_TICK);
            match weak.upgrade() {
                Some(engine) => engine.increment_epoch(),
                None => break,
            }
        }
    });
}

/// A loaded plugin: compiled, linked against the channel host, its manifest checked against
/// the catalog, and its capabilities fixed as manifest ∩ granted.
pub struct Plugin {
    engine: Engine,
    pre: InstancePre<CallState>,
    handle: ComponentExportIndex,
    manifest: Manifest,
    capabilities: Arc<BTreeMap<String, Arc<dyn Capability>>>,
    delivered: AtomicUsize,
}

impl Plugin {
    pub async fn load(engine: &Engine, bytes: &[u8], grants: &Grants) -> Result<Self, LoadError> {
        let component = Component::from_binary(engine, bytes).map_err(LoadError::Compile)?;
        for (name, _) in component.component_type().imports(engine) {
            if name != HOST_INTERFACE {
                return Err(LoadError::Import(name.to_owned()));
            }
        }
        let mut linker = Linker::new(engine);
        link_host(&mut linker).map_err(LoadError::Link)?;
        let pre = linker
            .instantiate_pre(&component)
            .map_err(LoadError::Link)?;
        let (_, handle) = component
            .get_export(None, "handle")
            .ok_or_else(|| LoadError::Link(wasmtime::Error::msg("no handle export")))?;

        // The manifest is read in an instance with no capability at all.
        let mut store = call_store(engine, CallState::new(Arc::default(), HashMap::new()));
        let instance = pre
            .instantiate_async(&mut store)
            .await
            .map_err(LoadError::Link)?;
        let manifest_fn = instance
            .get_typed_func::<(), (String,)>(&mut store, "manifest")
            .map_err(LoadError::Link)?;
        let (text,) = manifest_fn
            .call_async(&mut store, ())
            .await
            .map_err(|error| LoadError::Manifest(format!("{error:#}")))?;
        let manifest: Manifest =
            serde_json::from_str(&text).map_err(|error| LoadError::Manifest(error.to_string()))?;

        for (event, &wanted) in &manifest.events {
            match catalog::event_version(event) {
                None => return Err(LoadError::UnknownEvent(event.clone())),
                Some(emitted) if emitted != wanted => {
                    return Err(LoadError::EventVersion {
                        event: event.clone(),
                        wanted,
                        emitted,
                    });
                }
                Some(_) => {}
            }
        }
        for capability in &manifest.capabilities {
            if catalog::methods(capability).is_none() {
                return Err(LoadError::UnknownCapability(capability.clone()));
            }
        }
        let capabilities = manifest
            .capabilities
            .iter()
            .filter_map(|name| Some((name.clone(), grants.get(name)?.clone())))
            .collect();

        Ok(Self {
            engine: engine.clone(),
            pre,
            handle,
            manifest,
            capabilities: Arc::new(capabilities),
            delivered: AtomicUsize::new(0),
        })
    }

    pub fn manifest(&self) -> &Manifest {
        &self.manifest
    }

    /// The capabilities this plugin's calls reach: manifest ∩ granted.
    pub fn capabilities(&self) -> impl Iterator<Item = &str> {
        self.capabilities.keys().map(String::as_str)
    }

    /// How many events reached the guest.
    pub fn delivered(&self) -> usize {
        self.delivered.load(Ordering::SeqCst)
    }

    /// Emits `event` with `payload` and the named data `data` (what `open` finds) to a fresh
    /// instance. An event the manifest does not list is not delivered: `Ok(None)` without
    /// running the guest.
    pub async fn emit(
        &self,
        event: &str,
        payload: &str,
        data: HashMap<String, Arc<[u8]>>,
    ) -> Result<Option<String>, CallError> {
        self.emit_with(Calling::Typed, event, payload, data).await
    }

    pub async fn emit_with(
        &self,
        calling: Calling,
        event: &str,
        payload: &str,
        data: HashMap<String, Arc<[u8]>>,
    ) -> Result<Option<String>, CallError> {
        if catalog::event_version(event).is_none() {
            return Err(CallError::UnknownEvent(event.to_owned()));
        }
        if !self.manifest.events.contains_key(event) {
            return Ok(None);
        }
        self.delivered.fetch_add(1, Ordering::SeqCst);
        let state = CallState::new(self.capabilities.clone(), data);
        let mut store = call_store(&self.engine, state);
        let instance = self
            .pre
            .instantiate_async(&mut store)
            .await
            .map_err(CallError::Trap)?;
        match calling {
            Calling::Typed => {
                let handle = instance
                    .get_typed_func::<(&str, &str), (Option<String>,)>(&mut store, &self.handle)
                    .map_err(CallError::Trap)?;
                let (answer,) = handle
                    .call_async(&mut store, (event, payload))
                    .await
                    .map_err(CallError::Trap)?;
                Ok(answer)
            }
            Calling::Dynamic => {
                let handle = instance
                    .get_func(&mut store, self.handle)
                    .ok_or_else(|| CallError::Trap(wasmtime::Error::msg("no handle export")))?;
                let mut results = [Val::Bool(false)];
                handle
                    .call_async(
                        &mut store,
                        &[
                            Val::String(event.to_owned()),
                            Val::String(payload.to_owned()),
                        ],
                        &mut results,
                    )
                    .await
                    .map_err(CallError::Trap)?;
                match results {
                    [Val::Option(None)] => Ok(None),
                    [Val::Option(Some(value))] => match *value {
                        Val::String(text) => Ok(Some(text)),
                        _ => Err(CallError::Trap(wasmtime::Error::msg(
                            "handle: not a string",
                        ))),
                    },
                    _ => Err(CallError::Trap(wasmtime::Error::msg(
                        "handle: not an option",
                    ))),
                }
            }
        }
    }
}

/// The host side of one call.
pub struct CallState {
    capabilities: Arc<BTreeMap<String, Arc<dyn Capability>>>,
    data: HashMap<String, Arc<[u8]>>,
    table: ResourceTable,
    limits: StoreLimits,
}

impl CallState {
    fn new(
        capabilities: Arc<BTreeMap<String, Arc<dyn Capability>>>,
        data: HashMap<String, Arc<[u8]>>,
    ) -> Self {
        Self {
            capabilities,
            data,
            table: ResourceTable::new(),
            limits: StoreLimitsBuilder::new()
                .memory_size(MAX_GUEST_MEMORY)
                .build(),
        }
    }

    /// The one dispatcher: a capability this instance was not given, or a method the catalog
    /// does not list for it, is refused before anything runs.
    fn dispatch(&self, capability: &str, method: &str, payload: &str) -> Result<String, String> {
        let Some(service) = self.capabilities.get(capability) else {
            return Err(format!(
                "capability {capability:?} is not granted to this plugin"
            ));
        };
        if !catalog::methods(capability).is_some_and(|methods| methods.contains(&method)) {
            return Err(format!(
                "capability {capability:?} has no method {method:?}"
            ));
        }
        service.call(method, payload)
    }
}

/// Bytes of the current call, read by the guest through the `buffer` resource.
pub struct Buffer(Arc<[u8]>);

fn link_host(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut root = linker.root();
    let mut host = root.instance(HOST_INTERFACE)?;
    host.resource(
        "buffer",
        ResourceType::host::<Buffer>(),
        |mut store: StoreContextMut<'_, CallState>, rep| {
            store
                .data_mut()
                .table
                .delete(Resource::<Buffer>::new_own(rep))?;
            Ok(())
        },
    )?;
    host.func_wrap(
        "[method]buffer.len",
        |store: StoreContextMut<'_, CallState>, (this,): (Resource<Buffer>,)| {
            let buffer = store.data().table.get(&this)?;
            Ok((buffer.0.len() as u64,))
        },
    )?;
    host.func_wrap(
        "[method]buffer.read",
        |store: StoreContextMut<'_, CallState>,
         (this, offset, len): (Resource<Buffer>, u64, u64)| {
            let bytes = &store.data().table.get(&this)?.0;
            let size = bytes.len() as u64;
            let start = offset.min(size);
            let end = start.saturating_add(len.min(MAX_READ)).min(size);
            Ok((bytes[start as usize..end as usize].to_vec(),))
        },
    )?;
    host.func_wrap(
        "call",
        |store: StoreContextMut<'_, CallState>,
         (capability, method, payload): (String, String, String)| {
            Ok((store.data().dispatch(&capability, &method, &payload),))
        },
    )?;
    host.func_wrap(
        "open",
        |mut store: StoreContextMut<'_, CallState>, (name,): (String,)| {
            let state = store.data_mut();
            let Some(bytes) = state.data.get(&name).cloned() else {
                return Ok((Err(format!("no data named {name:?} for this event")),));
            };
            Ok((Ok(state.table.push(Buffer(bytes))?),))
        },
    )?;
    host.func_wrap(
        "cancelled",
        |_store: StoreContextMut<'_, CallState>, (): ()| Ok((false,)),
    )?;
    Ok(())
}

/// A Store with no capability and no data, set up as [`call_store`]: what the bench runs the
/// existing fixture's `execute` in, so both worlds share the store setup.
pub fn bare_store(engine: &Engine) -> Store<CallState> {
    call_store(engine, CallState::new(Arc::default(), HashMap::new()))
}

/// A Store set up as the existing executor sets up one `execute`.
fn call_store(engine: &Engine, state: CallState) -> Store<CallState> {
    let mut store = Store::new(engine, state);
    store.set_hostcall_fuel(HOSTCALL_FUEL);
    store.limiter(|state| &mut state.limits);
    store
        .set_fuel(DEFAULT_FUEL)
        .expect("the engine consumes fuel");
    store
        .fuel_async_yield_interval(Some(FUEL_YIELD_INTERVAL))
        .expect("the engine consumes fuel");
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(|_| Ok(UpdateDeadline::Continue(1)));
    store
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Echo;

    impl Capability for Echo {
        fn call(&self, method: &str, payload: &str) -> Result<String, String> {
            Ok(format!("{method}:{payload}"))
        }
    }

    #[test]
    fn a_method_the_catalog_does_not_list_is_refused() {
        let process: Arc<dyn Capability> = Arc::new(Echo);
        let state = CallState::new(
            Arc::new([("process".to_owned(), process)].into()),
            HashMap::new(),
        );
        assert_eq!(
            state.dispatch("process", "spawn", "x"),
            Ok("spawn:x".to_owned())
        );
        assert!(
            state
                .dispatch("process", "kill", "x")
                .unwrap_err()
                .contains("no method")
        );
        assert!(
            state
                .dispatch("clock", "now", "")
                .unwrap_err()
                .contains("not granted")
        );
    }
}
