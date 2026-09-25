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
//! call ends. Fuel and an epoch deadline are set per call from [`ExecutionLimits`].

use std::time::Duration;

use p1_contracts::CancellationToken;
use p1_module_protocol::ModuleFailure;
use thiserror::Error;
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinSet;
use wasmtime::component::{InstancePre, Val};
use wasmtime::{Engine, Store, Trap, UpdateDeadline};

use crate::capabilities::{CallState, Services};
use crate::loader::EPOCH_TICK;

/// The fuel of one `execute` unless the caller sets its own: a generous bound on pure
/// computation (host waits cost none), so only a runaway loop meets it.
pub const DEFAULT_FUEL: u64 = 10_000_000_000;

/// The wall-clock deadline of one `execute` unless the caller sets its own; it bounds the
/// time the guest runs (checked at every epoch tick while guest code executes).
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(300);

/// How many epoch ticks a cancelled call may keep running to return its `cancelled`
/// status cooperatively (`control.cancelled`) before the host stops it.
pub const CANCEL_GRACE_TICKS: u64 = 50;

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
    #[error("the call was cancelled and did not return")]
    Cancelled,
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
    pre: InstancePre<CallState>,
    services: Services,
    limits: ExecutionLimits,
}

impl Executor {
    /// Starts the executor task on the Tokio runtime of `handle`.
    pub(crate) fn start(
        handle: &tokio::runtime::Handle,
        engine: Engine,
        pre: InstancePre<CallState>,
        services: Services,
        limits: ExecutionLimits,
    ) -> Self {
        let (requests, receiver) = mpsc::unbounded_channel();
        let setup = Setup {
            engine,
            pre,
            services,
            limits,
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
    let setup = std::sync::Arc::new(setup);
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

async fn serve(setup: std::sync::Arc<Setup>, mut request: Request) {
    let reply = tokio::select! {
        result = one_call(&setup, request.export, &request.params, request.cancel.clone()) => result,
        // The caller dropped its future: the call is abandoned, and dropping it here drops
        // its Store and everything the call held.
        () = request.reply.closed() => return,
    };
    let _ = request.reply.send(reply);
}

async fn one_call(
    setup: &Setup,
    export: &str,
    params: &[Val],
    cancel: CancellationToken,
) -> Result<Vec<Val>, ModuleFailure> {
    if cancel.is_cancelled() {
        return Err(ModuleFailure::Cancelled);
    }
    let mut store = Store::new(&setup.engine, CallState::new(cancel, &setup.services));
    store
        .set_fuel(setup.limits.fuel)
        .map_err(|error| failure(&error))?;
    let deadline_ticks = setup.limits.deadline_ticks();
    // Called at every epoch tick while guest code runs: the deadline and the cancellation
    // grace are counted here, and between ticks the call yields to the Tokio scheduler, so
    // a busy guest never starves the caller's runtime, current-thread included.
    store.set_epoch_deadline(1);
    store.epoch_deadline_callback(move |mut context| {
        let state = context.data_mut();
        state.ticks += 1;
        if state.ticks >= deadline_ticks {
            return Err(wasmtime::Error::new(Stopped::Deadline));
        }
        if state.cancel.is_cancelled() {
            state.cancelled_ticks += 1;
            if state.cancelled_ticks > CANCEL_GRACE_TICKS {
                return Err(wasmtime::Error::new(Stopped::Cancelled));
            }
        }
        Ok(UpdateDeadline::Yield(1))
    });

    let instance = setup
        .pre
        .instantiate_async(&mut store)
        .await
        .map_err(|error| failure(&error))?;
    let func = instance
        .get_func(&mut store, export)
        .ok_or_else(|| ModuleFailure::Trap(format!("the module exports no {export}")))?;
    let mut results = vec![Val::Bool(false); func.ty(&store).results().len()];
    func.call_async(&mut store, params, &mut results)
        .await
        .map_err(|error| failure(&error))?;
    Ok(results)
}

/// Maps a wasmtime error into the closed failure shapes. The text is wasmtime's or this
/// runtime's own, never guest memory.
fn failure(error: &wasmtime::Error) -> ModuleFailure {
    if let Some(stopped) = error.downcast_ref::<Stopped>() {
        return match stopped {
            Stopped::Deadline => ModuleFailure::DeadlineExceeded,
            Stopped::Cancelled => ModuleFailure::Cancelled,
        };
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
    fn stops_and_traps_map_to_the_closed_failures() {
        let map = |error| failure(&wasmtime::Error::new(error));
        assert_eq!(map(Stopped::Deadline), ModuleFailure::DeadlineExceeded);
        assert_eq!(map(Stopped::Cancelled), ModuleFailure::Cancelled);
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
}
