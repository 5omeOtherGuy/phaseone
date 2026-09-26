//! The capabilities this runtime links into a module's per-call instance, and only those the
//! manifest grants (freeze item 3): `control`, `clock` and `random` are the runtime's own,
//! `process` and `summary` are services the caller passes in explicitly — there is no
//! registry.
//!
//! Every import is an asynchronous host function (`func_wrap_async` / `func_new_async`):
//! the guest sees a plain call, the host awaits without blocking a thread. The dynamic
//! `Val` forms are used where a WIT record or variant crosses, because wasmtime's derived
//! typed forms expand to `unsafe impl`s, which this crate forbids.

use std::collections::hash_map::RandomState;
use std::hash::{BuildHasher, Hasher};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use p1_contracts::{BoxFuture, CancellationToken};
use wasmtime::component::{Linker, Resource, ResourceAny, ResourceTable, ResourceType, Val};
use wasmtime::{Engine, bail};

use crate::context_policy::{SummaryService, link_summary};
use crate::loader::interface_import;

/// A command a module asks to run (`process.command`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessCommand {
    /// Run as `bash -lc <script>`.
    pub script: String,
    /// Wall-clock limit in milliseconds.
    pub timeout_ms: u64,
}

/// How a command ended (`process.exit-status`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExitStatus {
    /// The shell exited with this code.
    Code(i32),
    /// The shell was ended by this signal.
    Signal(i32),
    /// Ended by a signal the service could not name.
    UnknownSignal,
    /// Killed at its time limit.
    TimedOut,
    /// Killed because the call was cancelled.
    Cancelled,
}

/// One event of a running command (`process.process-event`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessEvent {
    /// Output bytes, stdout and stderr merged in arrival order.
    Output(Vec<u8>),
    /// The terminal event.
    Exited(ExitStatus),
}

/// The native process service a module's `process` capability is linked to. The real one
/// is the service extracted from `p1-tool-shell`; the runtime only adapts it.
pub trait ProcessService: Send + Sync {
    /// Starts `command` for a call whose cancellation is `cancel`. The future settles the
    /// start even when `cancel` fires while it runs: a service that started the command
    /// returns its handle (the runtime reports the cancellation to the guest through it), and
    /// one that started nothing returns `Err`. `Err` names why nothing started, never the
    /// cancellation, which `process.wit` has no `spawn` error for.
    fn spawn(
        &self,
        command: ProcessCommand,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn RunningProcess>, String>>;
}

/// A started command. Dropping it must end the process group if it still runs: the runtime
/// drops it when the module drops the resource, when the call ends (however it ends) or when
/// it is abandoned.
pub trait RunningProcess: Send {
    /// The next event: output, then one `Exited`, then `None`. The runtime drops this future
    /// when the call is cancelled or abandoned while it waits, so it must lose no event then.
    fn next(&mut self) -> BoxFuture<'_, Option<ProcessEvent>>;

    /// Kills the process group because the call was cancelled. `next` then returns the
    /// output that remains and the exit; the runtime reports that exit as `cancelled`.
    fn kill(&mut self) -> BoxFuture<'_, ()>;
}

/// The services a caller grants a module; each is linked only when the manifest grants the
/// capability too.
#[derive(Clone, Default)]
pub struct Services {
    /// The `process` capability.
    pub process: Option<Arc<dyn ProcessService>>,
    /// The `summary` capability of a context policy
    /// ([`crate::context_policy::link_summary`]).
    pub summary: Option<Arc<dyn SummaryService>>,
}

/// A `process.running` as the host holds it, and where it is in the stream `process.wit`
/// defines: output, one `exited`, then `none`, then a trap.
pub(crate) struct HostRunning {
    process: Box<dyn RunningProcess>,
    /// The host killed the process group for the cancellation.
    killed: bool,
    /// The `exited` event was returned.
    exited: bool,
    /// `next` returned `none`; one more call traps.
    finished: bool,
}

impl HostRunning {
    fn new(process: Box<dyn RunningProcess>) -> Self {
        Self {
            process,
            killed: false,
            exited: false,
            finished: false,
        }
    }

    /// The next event of the stream. A cancellation, before or while this waits, kills the
    /// process group; what the process still prints follows, then `exited(cancelled)`.
    async fn next(&mut self, cancel: &CancellationToken) -> Option<ProcessEvent> {
        if self.exited {
            self.finished = true;
            return None;
        }
        if !self.killed {
            let event = tokio::select! {
                biased;
                () = cancel.cancelled() => None,
                event = self.process.next() => Some(event),
            };
            match event {
                Some(event) => return self.record(event),
                None => {
                    self.process.kill().await;
                    self.killed = true;
                }
            }
        }
        match self.process.next().await {
            Some(ProcessEvent::Output(bytes)) => Some(ProcessEvent::Output(bytes)),
            Some(ProcessEvent::Exited(_)) | None => {
                self.exited = true;
                Some(ProcessEvent::Exited(ExitStatus::Cancelled))
            }
        }
    }

    fn record(&mut self, event: Option<ProcessEvent>) -> Option<ProcessEvent> {
        match &event {
            Some(ProcessEvent::Exited(_)) => self.exited = true,
            Some(ProcessEvent::Output(_)) => {}
            None => self.finished = true,
        }
        event
    }
}

/// The `process.running` a cancelled call gets when its service refused to start a command:
/// the guest reads the cancellation where `process.wit` says it is — `exited(cancelled)` on
/// the resource — instead of a `spawn` error it could only report as a plain failure. No
/// command ran, so there is nothing to kill and nothing to read.
struct CancelledStart;

fn cancelled_start() -> Box<dyn RunningProcess> {
    Box::new(CancelledStart)
}

impl RunningProcess for CancelledStart {
    fn next(&mut self) -> BoxFuture<'_, Option<ProcessEvent>> {
        Box::pin(async { Some(ProcessEvent::Exited(ExitStatus::Cancelled)) })
    }

    fn kill(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }
}

/// The state of one per-call Store: everything the linked capabilities read.
pub(crate) struct CallState {
    pub(crate) cancel: CancellationToken,
    pub(crate) table: ResourceTable,
    process: Option<Arc<dyn ProcessService>>,
    pub(crate) summary: Option<Arc<dyn SummaryService>>,
    /// The origin of `clock.monotonic-now`, fixed per instance.
    origin: Instant,
    /// The call was cancelled and its fuel cut to the grace it gets to return.
    pub(crate) cancel_grace: bool,
}

impl CallState {
    pub(crate) fn new(cancel: CancellationToken, services: &Services) -> Self {
        Self {
            cancel,
            table: ResourceTable::new(),
            process: services.process.clone(),
            summary: services.summary.clone(),
            origin: Instant::now(),
            cancel_grace: false,
        }
    }
}

/// Why capabilities could not be linked.
#[derive(Debug, thiserror::Error)]
pub enum LinkError {
    /// The manifest grants a capability whose service the caller did not pass.
    #[error("the manifest grants {0}, but no {0} service was given")]
    MissingService(String),
    /// wasmtime refused a definition.
    #[error("cannot link {capability}: {reason}")]
    Wasmtime {
        /// The capability.
        capability: String,
        /// wasmtime's message.
        reason: String,
    },
}

/// A linker holding exactly the granted capabilities, plus the type-only `types` interface.
pub(crate) fn capability_linker(
    engine: &Engine,
    granted: &[String],
    services: &Services,
) -> Result<Linker<CallState>, LinkError> {
    let mut linker = Linker::new(engine);
    let wasmtime_error = |capability: &str| {
        let capability = capability.to_owned();
        move |error: wasmtime::Error| LinkError::Wasmtime {
            capability,
            reason: format!("{error:#}"),
        }
    };
    linker
        .instance(&interface_import("types"))
        .map_err(wasmtime_error("types"))?;
    for capability in granted {
        let result = match capability.as_str() {
            "control" => link_control(&mut linker),
            "clock" => link_clock(&mut linker),
            "random" => link_random(&mut linker),
            "process" => {
                if services.process.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_process(&mut linker)
            }
            "summary" => {
                if services.summary.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_summary(&mut linker)
            }
            // The loader refuses every other capability before a linker is built.
            other => Err(wasmtime::format_err!(
                "{other} is not a capability of this runtime"
            )),
        };
        result.map_err(wasmtime_error(capability))?;
    }
    Ok(linker)
}

fn link_control(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut control = linker.instance(&interface_import("control"))?;
    control.func_wrap_async("cancelled", |store, (): ()| {
        let cancelled = store.data().cancel.is_cancelled();
        Box::new(async move { Ok((cancelled,)) })
    })
}

fn link_clock(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut clock = linker.instance(&interface_import("clock"))?;
    clock.func_new_async("now", |_store, _ty, _params, results| {
        Box::new(async move {
            // A wall clock before 1970 is a host misconfiguration; zero says "unknown"
            // without failing the module's call.
            let since = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default();
            results[0] = Val::Record(vec![
                ("seconds".to_owned(), Val::U64(since.as_secs())),
                ("nanoseconds".to_owned(), Val::U32(since.subsec_nanos())),
            ]);
            Ok(())
        })
    })?;
    clock.func_wrap_async("monotonic-now", |store, (): ()| {
        let elapsed = store.data().origin.elapsed().as_nanos();
        let nanos = u64::try_from(elapsed).unwrap_or(u64::MAX);
        Box::new(async move { Ok((nanos,)) })
    })
}

fn link_random(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut random = linker.instance(&interface_import("random"))?;
    random.func_wrap_async("bytes", |_store, (len,): (u32,)| {
        Box::new(async move { Ok((random_bytes(len)?,)) })
    })
}

/// The largest `random.bytes` answer: nonces and ids, never bulk data.
const MAX_RANDOM_BYTES: u32 = 4096;

/// Bytes from std's per-process randomly keyed SipHash: unpredictable enough for the nonces
/// and ids `random` is for, and no key source (`runtime.wit` says so), which is why no
/// cryptographic generator is linked for it.
fn random_bytes(len: u32) -> wasmtime::Result<Vec<u8>> {
    if len > MAX_RANDOM_BYTES {
        bail!("random.bytes asked for {len} bytes, at most {MAX_RANDOM_BYTES} are given");
    }
    let state = RandomState::new();
    let mut bytes = Vec::with_capacity(len as usize);
    let mut counter: u64 = 0;
    while bytes.len() < len as usize {
        let mut hasher = state.build_hasher();
        hasher.write_u64(counter);
        counter += 1;
        bytes.extend_from_slice(&hasher.finish().to_le_bytes());
    }
    bytes.truncate(len as usize);
    Ok(bytes)
}

fn link_process(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut process = linker.instance(&interface_import("process"))?;
    process.resource(
        "running",
        ResourceType::host::<HostRunning>(),
        |mut store, rep| {
            // Dropping the entry drops the service's handle, which ends the process group.
            store
                .data_mut()
                .table
                .delete(Resource::<HostRunning>::new_own(rep))?;
            Ok(())
        },
    )?;
    process.func_new_async("spawn", |mut store, _ty, params, results| {
        Box::new(async move {
            let command = process_command(&params[0])?;
            let Some(service) = store.data().process.clone() else {
                bail!("process.spawn called without a process service");
            };
            let cancel = store.data().cancel.clone();
            // A cancellation is never answered with `err`: `process.wit` reserves that for a
            // command that could not start and gives the guest one way to read a cancellation
            // — the resource's `exited(cancelled)`. So this wait is not raced against the
            // cancellation (the service holds the token and settles the start itself), and a
            // service that refused to start for the cancellation yields a resource reporting
            // that ending at once instead of an `err` the guest would report as a failure.
            let spawned = service.spawn(command, cancel.clone()).await;
            let spawned = match spawned {
                Err(_) if cancel.is_cancelled() => Ok(cancelled_start()),
                spawned => spawned,
            };
            results[0] = match spawned {
                Ok(process) => {
                    let running = store.data_mut().table.push(HostRunning::new(process))?;
                    let handle = ResourceAny::try_from_resource(running, &mut store)?;
                    Val::Result(Ok(Some(Box::new(Val::Resource(handle)))))
                }
                Err(reason) => Val::Result(Err(Some(Box::new(Val::String(reason))))),
            };
            Ok(())
        })
    })?;
    process.func_new_async("[method]running.next", |mut store, _ty, params, results| {
        Box::new(async move {
            let Val::Resource(handle) = &params[0] else {
                bail!("process.running.next called without its resource");
            };
            // A handle the module dropped, or one the call no longer holds, fails here: a
            // later use traps.
            let running: Resource<HostRunning> = handle.try_into_resource(&mut store)?;
            let cancel = store.data().cancel.clone();
            let entry = store.data_mut().table.get_mut(&running)?;
            if entry.finished {
                bail!("process.running.next called after the stream ended");
            }
            let event = entry.next(&cancel).await;
            results[0] = Val::Option(event.map(|event| Box::new(event_val(event))));
            Ok(())
        })
    })
}

fn process_command(value: &Val) -> wasmtime::Result<ProcessCommand> {
    let Val::Record(fields) = value else {
        bail!("process.spawn: command is not a record");
    };
    let mut script = None;
    let mut timeout_ms = None;
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("script", Val::String(text)) => script = Some(text.clone()),
            ("timeout-ms", Val::U64(ms)) => timeout_ms = Some(*ms),
            _ => bail!("process.spawn: unexpected command field {name}"),
        }
    }
    match (script, timeout_ms) {
        (Some(script), Some(timeout_ms)) => Ok(ProcessCommand { script, timeout_ms }),
        _ => bail!("process.spawn: command is missing a field"),
    }
}

fn event_val(event: ProcessEvent) -> Val {
    match event {
        ProcessEvent::Output(bytes) => Val::Variant(
            "output".to_owned(),
            Some(Box::new(Val::List(
                bytes.into_iter().map(Val::U8).collect(),
            ))),
        ),
        ProcessEvent::Exited(status) => {
            let status = match status {
                ExitStatus::Code(code) => {
                    Val::Variant("code".to_owned(), Some(Box::new(Val::S32(code))))
                }
                ExitStatus::Signal(signal) => {
                    Val::Variant("signal".to_owned(), Some(Box::new(Val::S32(signal))))
                }
                ExitStatus::UnknownSignal => Val::Variant("unknown-signal".to_owned(), None),
                ExitStatus::TimedOut => Val::Variant("timed-out".to_owned(), None),
                ExitStatus::Cancelled => Val::Variant("cancelled".to_owned(), None),
            };
            Val::Variant("exited".to_owned(), Some(Box::new(status)))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_bytes_have_the_asked_length_and_a_bound() {
        assert_eq!(random_bytes(0).unwrap().len(), 0);
        assert_eq!(random_bytes(13).unwrap().len(), 13);
        assert!(random_bytes(MAX_RANDOM_BYTES + 1).is_err());
    }

    #[test]
    fn a_command_record_is_read_by_field_name() {
        let record = Val::Record(vec![
            ("script".to_owned(), Val::String("true".to_owned())),
            ("timeout-ms".to_owned(), Val::U64(5)),
        ]);
        assert_eq!(
            process_command(&record).unwrap(),
            ProcessCommand {
                script: "true".to_owned(),
                timeout_ms: 5
            }
        );
        assert!(process_command(&Val::Record(vec![])).is_err());
    }

    fn granted(capabilities: &[&str]) -> Vec<String> {
        capabilities.iter().map(|name| (*name).to_owned()).collect()
    }

    #[test]
    fn the_runtimes_own_capabilities_link_without_services_as_before() {
        let engine = crate::engine().expect("engine");
        let linked = capability_linker(
            &engine,
            &granted(&["control", "clock", "random"]),
            &Services::default(),
        );
        assert!(linked.is_ok());
    }

    #[test]
    fn a_granted_summary_without_its_service_is_a_missing_service() {
        let engine = crate::engine().expect("engine");
        match capability_linker(
            &engine,
            &granted(&["control", "summary"]),
            &Services::default(),
        ) {
            Err(LinkError::MissingService(capability)) => assert_eq!(capability, "summary"),
            Err(other) => panic!("wrong link error: {other}"),
            Ok(_) => panic!("summary must not link without a summary service"),
        }
    }
}
