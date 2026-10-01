//! The executor (ADR-0015): the one owner of a module's asynchronous Stores.
//!
//! Callers never hold a Store. They send a request with a oneshot reply over a channel and
//! await the reply, so every public async method is a `Send` boxed future whatever the
//! caller's Tokio flavour. The executor task receives the requests and runs each call as a
//! task of its own `JoinSet`: calls from different callers run concurrently, and dropping
//! the executor (its channel closes when the adapter is dropped) aborts whatever still runs.
//!
//! Each call gets a fresh Store and a fresh instance from the pre-linked component, so a
//! trap, a deadline or an abandoned call poisons nothing for the next one; the Store (and
//! with it every resource the call held, a running process included) is dropped when the
//! call ends, however it ends. Nothing the host did for the call is rolled back then: a trap
//! never undoes a native effect.
//!
//! How a call is bounded (freeze item 4):
//! - **Hostcall fuel.** What one import call may make the host allocate while lifting its
//!   arguments is [`HOSTCALL_FUEL`]: a whole file of up to [`MAX_TRANSFER_BYTES`] and no
//!   more (ADR-0092).
//! - **Fuel.** Each call starts with [`ExecutionLimits::fuel`]; running out traps as
//!   `FuelExhausted`. The guest also yields to the Tokio scheduler every
//!   [`FUEL_YIELD_INTERVAL`] of fuel, so a busy guest never starves the caller's runtime,
//!   current-thread included, and the call's task keeps watching its deadline and its
//!   cancellation while the guest computes.
//! - **Deadline.** [`ExecutionLimits::deadline`], counted on the engine's epoch clock, bounds
//!   the whole call, host waits included: past it, the guest's epoch callback traps, or the
//!   call's task abandons a call that waits in a host import. Either is `DeadlineExceeded`.
//! - **Cancellation.** When `ToolContext.cancel` fires, `control.cancelled()` answers true,
//!   every blocked host import returns as its WIT contract says, and the call's task
//!   interrupts the engine's epoch so a guest in a CPU loop reaches its epoch callback at
//!   once. The callback cuts the call's fuel to [`CANCEL_GRACE_FUEL`]: enough to return the
//!   `cancelled` status cooperatively, and a loop that never checks runs out of it, which the
//!   executor answers as `Cancelled`, not as a fuel or deadline failure.

use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use p1_contracts::CancellationToken;
use p1_module_protocol::ModuleFailure;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use wasmtime::component::{InstancePre, Val};
use wasmtime::{Engine, Store, Trap, UpdateDeadline};

use crate::capabilities::{CallState, Services};
use crate::loader::{EPOCH_TICK, Epochs};

/// The fuel of one `execute` unless the caller sets its own: a generous bound on pure
/// computation (host waits cost none), so only a runaway loop meets it.
pub const DEFAULT_FUEL: u64 = 10_000_000_000;

/// The wall-clock deadline of one `execute` unless the caller sets its own.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(300);

/// The fuel a cancelled call keeps to return its `cancelled` status cooperatively
/// (`control.cancelled`, or a blocking import answering the cancellation) before it is
/// stopped: enough to serialize an outcome, far too little to hide a loop behind.
pub const CANCEL_GRACE_FUEL: u64 = 50_000_000;

/// How much fuel a guest consumes between two yields to the Tokio scheduler: small against
/// every budget above, so the call's task looks at its deadline and cancellation often.
pub const FUEL_YIELD_INTERVAL: u64 = 1_000_000;

/// The largest byte list a guest may hand the host in one import call: a whole file a
/// component passes `snapshot.check`, `snapshot.observe` or a `workspace-mutation` write
/// (ADR-0092). The workspace sets no file size of its own, so this is the file size a
/// component can edit, write or patch; the native tools have none.
pub const MAX_TRANSFER_BYTES: usize = 16 << 20;

/// The hostcall fuel of one call's Store: what wasmtime lets one import call allocate on the
/// host while lifting its arguments. The imports are linked dynamically (`Val`, the typed
/// forms need `unsafe impl`s), and a dynamic lift charges one `Val` per list element, so
/// the default of 128 MiB stops a byte list at about three megabytes. Sized for
/// [`MAX_TRANSFER_BYTES`] as `Val`s plus the default for the rest of the call's arguments,
/// it still bounds what one import call can make the host allocate.
pub const HOSTCALL_FUEL: usize = MAX_TRANSFER_BYTES * size_of::<Val>() + (128 << 20);

/// Limit dynamic lifting, guest linear memory and reference tables at every Store
/// construction site. Tables have a separate element budget derived from this ceiling.
pub(crate) const MAX_GUEST_MEMORY: usize = 256 << 20;

/// The Store limits of one module call: the [`wasmtime::StoreLimits`] defaults for
/// instances, tables and memory count, a per-memory and aggregate linear-memory ceiling
/// of `limit`, and a separate per-table and aggregate budget of `limit / size_of::<usize>()`
/// elements. Bounding linear memory alone leaves reference-table allocations unbounded.
///
/// [`wasmtime::StoreLimitsBuilder::memory_size`] bounds each memory on its own, so a
/// component with several core instances could spend the ceiling once per memory. This
/// limiter keeps the running total, so a Store's guest memory never exceeds `limit` however
/// many memories hold it.
pub(crate) struct MemoryLimiter {
    inner: wasmtime::StoreLimits,
    /// The ceiling on each memory and on the total.
    limit: usize,
    /// The total bytes this Store's linear memories hold. A growth the inner limiter
    /// permitted but the allocator then failed stays counted, so the bound is only ever seen
    /// as stricter, never looser.
    total: usize,
    table_limit: usize,
    /// As for memory, failed allocations stay charged conservatively.
    table_total: usize,
}

impl MemoryLimiter {
    pub(crate) fn new(limit: usize) -> Self {
        let table_limit = limit / size_of::<usize>();
        Self {
            inner: wasmtime::StoreLimitsBuilder::new()
                .memory_size(limit)
                .table_elements(table_limit)
                .build(),
            limit,
            total: 0,
            table_limit,
            table_total: 0,
        }
    }
}

impl wasmtime::ResourceLimiter for MemoryLimiter {
    fn memory_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        // `total` already counts `current`: what the whole Store holds after this growth.
        let aggregated = self.total.saturating_sub(current).saturating_add(desired);
        if aggregated > self.limit {
            return Ok(false);
        }
        if wasmtime::ResourceLimiter::memory_growing(&mut self.inner, current, desired, maximum)? {
            self.total = aggregated;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn memory_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        wasmtime::ResourceLimiter::memory_grow_failed(&mut self.inner, error)
    }

    fn table_growing(
        &mut self,
        current: usize,
        desired: usize,
        maximum: Option<usize>,
    ) -> wasmtime::Result<bool> {
        let aggregated = self
            .table_total
            .saturating_sub(current)
            .saturating_add(desired);
        if aggregated > self.table_limit {
            return Ok(false);
        }
        if wasmtime::ResourceLimiter::table_growing(&mut self.inner, current, desired, maximum)? {
            self.table_total = aggregated;
            Ok(true)
        } else {
            Ok(false)
        }
    }

    fn table_grow_failed(&mut self, error: wasmtime::Error) -> wasmtime::Result<()> {
        wasmtime::ResourceLimiter::table_grow_failed(&mut self.inner, error)
    }

    fn instances(&self) -> usize {
        wasmtime::ResourceLimiter::instances(&self.inner)
    }

    fn tables(&self) -> usize {
        wasmtime::ResourceLimiter::tables(&self.inner)
    }

    fn memories(&self) -> usize {
        wasmtime::ResourceLimiter::memories(&self.inner)
    }
}

pub(crate) fn store_limits() -> MemoryLimiter {
    MemoryLimiter::new(MAX_GUEST_MEMORY)
}

pub(crate) trait LimitedStore {
    fn limits(&mut self) -> &mut MemoryLimiter;
}

pub(crate) struct BareStore {
    limits: MemoryLimiter,
}

impl Default for BareStore {
    fn default() -> Self {
        Self {
            limits: store_limits(),
        }
    }
}

impl LimitedStore for BareStore {
    fn limits(&mut self) -> &mut MemoryLimiter {
        &mut self.limits
    }
}

pub(crate) fn module_store<T: LimitedStore + 'static>(engine: &Engine, data: T) -> Store<T> {
    let mut store = Store::new(engine, data);
    store.set_hostcall_fuel(HOSTCALL_FUEL);
    store.limiter(|data| data.limits());
    store
}

/// The per-call limits of `execute`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExecutionLimits {
    /// Fuel for one call.
    pub fuel: u64,
    /// Wall-clock bound of one call, counted in [`EPOCH_TICK`]s.
    pub deadline: Duration,
}

impl Default for ExecutionLimits {
    fn default() -> Self {
        Self {
            fuel: DEFAULT_FUEL,
            deadline: DEFAULT_DEADLINE,
        }
    }
}

impl ExecutionLimits {
    fn deadline_ticks(&self) -> u64 {
        let ticks = self.deadline.as_nanos() / EPOCH_TICK.as_nanos();
        u64::try_from(ticks).unwrap_or(u64::MAX).max(1)
    }
}

/// Why the epoch callback stopped a call; carried through wasmtime's error to the mapping.
#[derive(Debug, Error)]
enum Stopped {
    #[error("the call ran past its deadline")]
    Deadline,
}

struct Request {
    export: &'static str,
    params: Vec<Val>,
    cancel: CancellationToken,
    reply: oneshot::Sender<Result<Vec<Val>, ModuleFailure>>,
}

/// The handle callers hold: a channel to the executor task.
pub(crate) struct Executor {
    requests: mpsc::UnboundedSender<Request>,
}

/// What every call of one module is built from.
struct Setup {
    engine: Engine,
    epochs: Arc<Epochs>,
    pre: InstancePre<CallState>,
    services: Services,
    limits: ExecutionLimits,
    prelude: Option<Prelude>,
}

/// An export called on each fresh instance before the requested one, inside the same call
/// and under the same limits: how a context policy is `configure`d (S5, GO S5-B7). A trap
/// in it fails the call as the export's own trap would; an `err` result fails it too, and
/// the requested export is then never called.
#[derive(Clone)]
pub(crate) struct Prelude {
    pub(crate) export: &'static str,
    pub(crate) params: Vec<Val>,
}

impl Executor {
    /// Starts the executor task on the Tokio runtime of `handle`.
    pub(crate) fn start(
        handle: &tokio::runtime::Handle,
        engine: Engine,
        epochs: Arc<Epochs>,
        pre: InstancePre<CallState>,
        services: Services,
        limits: ExecutionLimits,
    ) -> Self {
        Self::start_with_prelude(handle, engine, epochs, pre, services, limits, None)
    }

    /// [`Executor::start`], with `prelude` called on each fresh instance before the
    /// requested export.
    pub(crate) fn start_with_prelude(
        handle: &tokio::runtime::Handle,
        engine: Engine,
        epochs: Arc<Epochs>,
        pre: InstancePre<CallState>,
        services: Services,
        limits: ExecutionLimits,
        prelude: Option<Prelude>,
    ) -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        let setup = Setup {
            engine,
            epochs,
            pre,
            services,
            limits,
            prelude,
        };
        handle.spawn(run(receiver, setup));
        Self { requests }
    }

    /// Calls `export` with `params` on a fresh instance. The returned future is `Send` and
    /// holds no Store; dropping it abandons the call.
    pub(crate) async fn call(
        &self,
        export: &'static str,
        params: Vec<Val>,
        cancel: CancellationToken,
    ) -> Result<Vec<Val>, ModuleFailure> {
        let (reply, answer) = oneshot::channel();
        let request = Request {
            export,
            params,
            cancel,
            reply,
        };
        if self.requests.send(request).is_err() {
            return Err(stopped());
        }
        answer.await.unwrap_or_else(|_| Err(stopped()))
    }
}

fn stopped() -> ModuleFailure {
    ModuleFailure::Trap("the module executor stopped before the call ended".to_owned())
}

async fn run(mut receiver: mpsc::UnboundedReceiver<Request>, setup: Setup) {
    let setup = Arc::new(setup);
    let mut calls = JoinSet::new();
    loop {
        tokio::select! {
            request = receiver.recv() => match request {
                Some(request) => {
                    let setup = setup.clone();
                    calls.spawn(serve(setup, request));
                }
                None => break,
            },
            Some(_) = calls.join_next(), if !calls.is_empty() => {}
        }
    }
}

async fn serve(setup: Arc<Setup>, request: Request) {
    let Request {
        export,
        params,
        cancel,
        mut reply,
    } = request;
    let mut clock = setup.epochs.subscribe();
    let deadline = clock.borrow().saturating_add(setup.limits.deadline_ticks());
    // Boxed so it can be dropped before the reply is sent: the call's Store, and every
    // process it held, is gone by the time the caller sees the outcome.
    let mut call: Pin<Box<_>> =
        Box::pin(one_call(&setup, export, &params, cancel.clone(), deadline));
    let mut interrupted = false;
    let result = loop {
        tokio::select! {
            result = &mut call => break result,
            // The caller dropped its future: the call is abandoned, and returning here
            // drops its Store and everything the call held.
            () = reply.closed() => return,
            // Past the deadline while the guest waits in a host import (a running guest
            // meets it in its epoch callback first).
            Ok(_) = clock.wait_for(|now| *now >= deadline) => break Err(ModuleFailure::DeadlineExceeded),
            () = cancel.cancelled(), if !interrupted => {
                interrupted = true;
                setup.epochs.interrupt();
            }
        }
    };
    drop(call);
    let _ = reply.send(result);
}

async fn one_call(
    setup: &Setup,
    export: &str,
    params: &[Val],
    cancel: CancellationToken,
    deadline: u64,
) -> Result<Vec<Val>, ModuleFailure> {
    if cancel.is_cancelled() {
        return Err(ModuleFailure::Cancelled);
    }
    let mut store = module_store(&setup.engine, CallState::new(cancel, &setup.services));
    store
        .set_fuel(setup.limits.fuel)
        .map_err(|error| failure(&error))?;
    store
        .fuel_async_yield_interval(Some(FUEL_YIELD_INTERVAL))
        .map_err(|error| failure(&error))?;
    // Called at every epoch tick while guest code runs, and at once after a cancellation
    // interrupts the engine's epoch.
    let clock = setup.epochs.subscribe();
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |mut context| {
        if *clock.borrow() >= deadline {
            return Err(wasmtime::Error::new(Stopped::Deadline));
        }
        if context.data().cancel.is_cancelled() && !context.data().cancel_grace {
            context.data_mut().cancel_grace = true;
            let left = context.get_fuel()?;
            context.set_fuel(left.min(CANCEL_GRACE_FUEL))?;
        }
        Ok(UpdateDeadline::Continue(1))
    });

    let instance = setup
        .pre
        .instantiate_async(&mut store)
        .await
        .map_err(|error| call_failure(&store, &error))?;
    if let Some(prelude) = &setup.prelude {
        let func = instance
            .get_func(&mut store, prelude.export)
            .ok_or_else(|| {
                ModuleFailure::Trap(format!("the module exports no {}", prelude.export))
            })?;
        let mut results = vec![Val::Bool(false); func.ty(&store).results().len()];
        if let Err(error) = func
            .call_async(&mut store, &prelude.params, &mut results)
            .await
        {
            return Err(call_failure(&store, &error));
        }
        prelude_refusal(prelude.export, &results)?;
    }
    let func = instance
        .get_func(&mut store, export)
        .ok_or_else(|| ModuleFailure::Trap(format!("the module exports no {export}")))?;
    let mut results = vec![Val::Bool(false); func.ty(&store).results().len()];
    if let Err(error) = func.call_async(&mut store, params, &mut results).await {
        return Err(call_failure(&store, &error));
    }
    Ok(results)
}

/// A prelude that answered `err` refused the call: the export's own trap shape, naming the
/// prelude and, when it gave one, its reason.
fn prelude_refusal(export: &str, results: &[Val]) -> Result<(), ModuleFailure> {
    match results.first() {
        Some(Val::Result(Err(reason))) => Err(ModuleFailure::Trap(match reason.as_deref() {
            Some(Val::String(reason)) => format!("{export} refused: {reason}"),
            _ => format!("{export} refused"),
        })),
        _ => Ok(()),
    }
}

/// A cancelled call that used up its grace fuel was stopped for the cancellation, not for
/// its budget.
fn call_failure(store: &Store<CallState>, error: &wasmtime::Error) -> ModuleFailure {
    match failure(error) {
        ModuleFailure::FuelExhausted if store.data().cancel_grace => ModuleFailure::Cancelled,
        other => other,
    }
}

/// Maps a wasmtime error into the closed failure shapes. The text is wasmtime's or this
/// runtime's own, never guest memory.
fn failure(error: &wasmtime::Error) -> ModuleFailure {
    if let Some(Stopped::Deadline) = error.downcast_ref::<Stopped>() {
        return ModuleFailure::DeadlineExceeded;
    }
    match error.downcast_ref::<Trap>() {
        Some(Trap::OutOfFuel) => ModuleFailure::FuelExhausted,
        Some(Trap::Interrupt) => ModuleFailure::DeadlineExceeded,
        Some(trap) => ModuleFailure::Trap(trap.to_string()),
        None => ModuleFailure::Trap(format!("{error:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_store_constructor_enforces_linear_memory_limit() {
        let engine = Engine::default();
        let data = BareStore {
            limits: MemoryLimiter::new(65536),
        };
        let mut store = module_store(&engine, data);
        let memory = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None))
            .expect("one page fits");
        assert!(memory.grow(&mut store, 1).is_err());
    }

    #[test]
    fn shared_store_constructor_bounds_total_memory_across_memories() {
        let engine = Engine::default();
        let data = BareStore {
            limits: MemoryLimiter::new(100_000),
        };
        let mut store = module_store(&engine, data);
        // One 64 KiB memory is inside the ceiling; a second is not, though each memory on
        // its own would pass the per-memory bound (the pre-fix behaviour). The first handle
        // stays alive so the refusal is the total, not a freed slot.
        let _first = wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None))
            .expect("the first memory fits");
        assert!(
            wasmtime::Memory::new(&mut store, wasmtime::MemoryType::new(1, None)).is_err(),
            "a second memory passed the Store's total limit"
        );
    }

    #[test]
    fn shared_store_constructor_bounds_initial_tables_and_growth() {
        let engine = Engine::default();
        let mut store = module_store(
            &engine,
            BareStore {
                limits: MemoryLimiter::new(4 * size_of::<usize>()),
            },
        );
        let table_type =
            |elements| wasmtime::TableType::new(wasmtime::RefType::FUNCREF, elements, None);
        assert!(
            wasmtime::Table::new(&mut store, table_type(5), wasmtime::Ref::Func(None)).is_err()
        );
        let table = wasmtime::Table::new(&mut store, table_type(3), wasmtime::Ref::Func(None))
            .expect("three elements fit");
        assert!(
            table
                .grow(&mut store, 2, wasmtime::Ref::Func(None))
                .is_err()
        );
        assert_eq!(
            table
                .grow(&mut store, 1, wasmtime::Ref::Func(None))
                .unwrap(),
            3
        );
    }

    #[test]
    fn shared_store_constructor_bounds_total_table_elements() {
        let engine = Engine::default();
        let mut store = module_store(
            &engine,
            BareStore {
                limits: MemoryLimiter::new(4 * size_of::<usize>()),
            },
        );
        let table_type = || wasmtime::TableType::new(wasmtime::RefType::FUNCREF, 2, None);
        let first =
            wasmtime::Table::new(&mut store, table_type(), wasmtime::Ref::Func(None)).unwrap();
        let _second =
            wasmtime::Table::new(&mut store, table_type(), wasmtime::Ref::Func(None)).unwrap();
        assert!(
            first
                .grow(&mut store, 1, wasmtime::Ref::Func(None))
                .is_err()
        );
        assert!(wasmtime::Table::new(&mut store, table_type(), wasmtime::Ref::Func(None)).is_err());
    }

    #[test]
    fn a_failed_table_allocation_stays_charged_conservatively() {
        use wasmtime::ResourceLimiter;

        let mut limits = MemoryLimiter::new(4 * size_of::<usize>());
        assert!(limits.table_growing(0, 3, None).unwrap());
        limits
            .table_grow_failed(wasmtime::format_err!("allocation failed"))
            .unwrap();
        assert!(!limits.table_growing(0, 2, None).unwrap());
        assert!(limits.table_growing(0, 1, None).unwrap());
        assert!(!limits.table_growing(0, 1, None).unwrap());
    }

    #[test]
    fn stops_and_traps_map_to_the_closed_failures() {
        assert_eq!(
            failure(&wasmtime::Error::new(Stopped::Deadline)),
            ModuleFailure::DeadlineExceeded
        );
        assert_eq!(
            failure(&wasmtime::Error::new(Trap::OutOfFuel)),
            ModuleFailure::FuelExhausted
        );
        assert_eq!(
            failure(&wasmtime::Error::new(Trap::Interrupt)),
            ModuleFailure::DeadlineExceeded
        );
        match failure(&wasmtime::Error::new(Trap::UnreachableCodeReached)) {
            ModuleFailure::Trap(message) => assert!(message.contains("unreachable"), "{message}"),
            other => panic!("expected a trap, got {other:?}"),
        }
        assert!(matches!(
            failure(&wasmtime::format_err!("a host import failed")),
            ModuleFailure::Trap(message) if message == "a host import failed"
        ));
    }

    #[test]
    fn a_deadline_is_counted_in_whole_ticks_and_never_zero() {
        let limits = |deadline| ExecutionLimits { fuel: 1, deadline };
        assert_eq!(limits(EPOCH_TICK * 30).deadline_ticks(), 30);
        assert_eq!(limits(Duration::ZERO).deadline_ticks(), 1);
    }

    #[test]
    fn only_an_err_prelude_refuses_the_call() {
        assert_eq!(prelude_refusal("configure", &[]), Ok(()));
        assert_eq!(
            prelude_refusal("configure", &[Val::Result(Ok(None))]),
            Ok(())
        );
        assert_eq!(
            prelude_refusal(
                "configure",
                &[Val::Result(Err(Some(Box::new(Val::String(
                    "bad key".to_owned()
                )))))]
            ),
            Err(ModuleFailure::Trap("configure refused: bad key".to_owned()))
        );
        assert_eq!(
            prelude_refusal("configure", &[Val::Result(Err(None))]),
            Err(ModuleFailure::Trap("configure refused".to_owned()))
        );
    }
}
