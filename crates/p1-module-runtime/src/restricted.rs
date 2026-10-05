//! The restricted synchronous path (freeze item 4): how `effect`, `describe` and
//! `describe-result` run.
//!
//! Those three are synchronous in `p1_contracts::Tool` and the host calls them from inside
//! async code on any Tokio flavour, so they must never wait on the executor or block on a
//! runtime. They run here instead: on a second instance of the module that this adapter
//! owns behind a `Mutex`, called on the caller's own thread, with no capability linked —
//! every import is defined by [`Linker::define_unknown_imports_as_traps`], so an import
//! called on this path traps — and with a tight fuel budget. The one exception (D085) is a
//! tool's two read-only `workers-observe` lists, see [`Restricted::with_worker_lists`].
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

use std::sync::{Arc, Mutex};

use wasmtime::component::{Component, Func, Instance, InstancePre, Linker, Val};
use wasmtime::{Engine, Store, Trap};

use crate::delegation::{WorkerLists, link_worker_lists};
use crate::executor::{BareStore, module_store};
use crate::loader::Epochs;

/// The fuel of one restricted call: enough for a module to parse its input and write a
/// description, far too little to hide real work behind "inspection".
pub const RESTRICTED_FUEL: u64 = 50_000_000;

/// The wall-clock bound of one restricted call, in ticks of the epoch clock: a backstop behind
/// the fuel, which is what normally ends a runaway inspection. It counts ticks only, so a call
/// cancelled meanwhile anywhere on the shared engine (see `Epochs::interrupt`) spends none.
pub const RESTRICTED_DEADLINE_TICKS: u64 = 200;

pub(crate) struct Restricted {
    engine: Engine,
    epochs: Arc<Epochs>,
    pre: InstancePre<BareStore>,
    live: Mutex<Option<Live>>,
}

struct Live {
    store: Store<BareStore>,
    instance: Instance,
}

impl Restricted {
    pub(crate) fn new(
        engine: &Engine,
        epochs: &Arc<Epochs>,
        component: &Component,
    ) -> wasmtime::Result<Self> {
        Self::with_worker_lists(engine, epochs, component, None)
    }

    /// As [`Restricted::new`], with the one exception D085 allows: a tool granted
    /// `workers-observe` gets that interface's `grantable` and `environments` answered from
    /// `lists`, the data fixed for its assembly, because its `declaration` builds its schema
    /// from them. Both are synchronous, take no argument and have no effect; every other
    /// import, the rest of `workers-observe` included, is still a trap.
    pub(crate) fn with_worker_lists(
        engine: &Engine,
        epochs: &Arc<Epochs>,
        component: &Component,
        lists: Option<&WorkerLists>,
    ) -> wasmtime::Result<Self> {
        let mut linker: Linker<BareStore> = Linker::new(engine);
        if let Some(lists) = lists {
            link_worker_lists(&mut linker, lists)?;
        }
        linker.define_unknown_imports_as_traps(component)?;
        let pre = linker.instantiate_pre(component)?;
        Ok(Self {
            engine: engine.clone(),
            epochs: epochs.clone(),
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
                let mut store = module_store(&self.engine, BareStore::default());
                limit(&mut store, &self.epochs)?;
                let instance = self.pre.instantiate(&mut store)?;
                live.insert(Live { store, instance })
            }
        };
        limit(&mut live.store, &self.epochs)?;
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
fn limit(store: &mut Store<BareStore>, epochs: &Epochs) -> wasmtime::Result<()> {
    store.set_fuel(RESTRICTED_FUEL)?;
    epochs.arm_deadline(store, RESTRICTED_DEADLINE_TICKS, || {
        wasmtime::Error::new(Trap::Interrupt)
    });
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn restricted_deadlines_count_ticks_not_interrupts() {
        for (ticks, expected) in [
            (0, Trap::OutOfFuel),
            (RESTRICTED_DEADLINE_TICKS, Trap::Interrupt),
        ] {
            let engine = crate::engine().unwrap();
            let epochs = Epochs::new(engine.clone());
            let restricted = Restricted {
                pre: crate::loader::tests::deadline_probe(&engine, &epochs, ticks),
                engine,
                epochs,
                live: Mutex::new(None),
            };
            let error = restricted
                .call_locked(
                    &mut None,
                    "spin",
                    &[Val::String(String::new()), Val::String(String::new())],
                )
                .expect_err("probe spins until a budget traps");
            assert_eq!(error.downcast_ref::<Trap>(), Some(&expected), "{error:#}");
        }
    }
}
