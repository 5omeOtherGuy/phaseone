//! The restricted synchronous path (freeze item 4): how `effect`, `describe` and
//! `describe-result` run.
//!
//! Those three are synchronous in `p1_contracts::Tool` and the host calls them from inside
//! async code on any Tokio flavour, so they must never wait on the executor or block on a
//! runtime. They run here instead: on a second instance of the module that this adapter
//! owns behind a `Mutex`, called on the caller's own thread, with no capability linked —
//! every import is defined by [`Linker::define_unknown_imports_as_traps`], so an import
//! called on this path traps — and with a tight fuel budget.
//!
//! The choice between the two ways of calling synchronously: this path uses a Store that
//! never sees an asynchronous definition, and calls it with wasmtime's synchronous
//! [`Func::call`]. In wasmtime 49 async support is not an engine switch; a Store needs the
//! `*_async` entry points only once something asynchronous is linked into it (an async
//! host function, an async yield), and this Store's linker holds nothing but synchronous
//! trap stubs. So the same engine (a component is compiled for one engine) serves both
//! paths, the call runs on the caller's stack with no fiber and no future, and there is no
//! poll loop to reason about. The alternative — `call_async` polled with
//! `Waker::noop()` — would be correct only as long as nothing on the path could ever be
//! pending, an invariant a future change could break silently into a spin; here such a
//! change fails loudly, because wasmtime refuses a synchronous call on a Store that needs
//! async.
//!
//! A trap (an import, `unreachable`, fuel, a deadline) leaves a component instance that may
//! not be entered again, so the instance is dropped and the next call builds a fresh one.

use std::sync::Mutex;

use wasmtime::component::{Component, Func, Instance, InstancePre, Linker, Val};
use wasmtime::{Engine, Store};

/// The fuel of one restricted call: enough for a module to parse its input and write a
/// description, far too little to hide real work behind "inspection".
pub const RESTRICTED_FUEL: u64 = 50_000_000;

/// The wall-clock bound of one restricted call, in epoch ticks: a backstop behind the fuel,
/// which is what normally ends a runaway inspection. It is the engine's own epoch, so any
/// call cancelled meanwhile (see `Epochs::interrupt`) spends one of these ticks.
pub const RESTRICTED_DEADLINE_TICKS: u64 = 200;

pub(crate) struct Restricted {
    engine: Engine,
    pre: InstancePre<()>,
    live: Mutex<Option<Live>>,
}

struct Live {
    store: Store<()>,
    instance: Instance,
}

impl Restricted {
    pub(crate) fn new(engine: &Engine, component: &Component) -> wasmtime::Result<Self> {
        let mut linker: Linker<()> = Linker::new(engine);
        linker.define_unknown_imports_as_traps(component)?;
        let pre = linker.instantiate_pre(component)?;
        Ok(Self {
            engine: engine.clone(),
            pre,
            live: Mutex::new(None),
        })
    }

    /// Calls export `name` with `params`, returning its results, or `None` when the call
    /// trapped or the instance could not be built.
    pub(crate) fn call(&self, name: &str, params: &[Val]) -> Option<Vec<Val>> {
        // A panic while the lock was held cannot leave the Store half-updated in a way that
        // matters: the instance is rebuilt on any failure, so a poisoned lock is recovered.
        let mut live = self
            .live
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcome = self.call_locked(&mut live, name, params);
        if outcome.is_err() {
            *live = None;
        }
        outcome.ok()
    }

    fn call_locked(
        &self,
        live: &mut Option<Live>,
        name: &str,
        params: &[Val],
    ) -> wasmtime::Result<Vec<Val>> {
        let live = match live {
            Some(live) => live,
            None => {
                let mut store = Store::new(&self.engine, ());
                limit(&mut store)?;
                let instance = self.pre.instantiate(&mut store)?;
                live.insert(Live { store, instance })
            }
        };
        limit(&mut live.store)?;
        let func: Func = live
            .instance
            .get_func(&mut live.store, name)
            .ok_or_else(|| wasmtime::format_err!("the module exports no {name}"))?;
        let mut results = vec![Val::Bool(false); func.ty(&live.store).results().len()];
        func.call(&mut live.store, params, &mut results)?;
        Ok(results)
    }
}

/// Every restricted call starts with the full budget, so one inspection cannot starve the
/// next.
fn limit(store: &mut Store<()>) -> wasmtime::Result<()> {
    store.set_fuel(RESTRICTED_FUEL)?;
    store.epoch_deadline_trap();
    store.set_epoch_deadline(RESTRICTED_DEADLINE_TICKS);
    Ok(())
}
