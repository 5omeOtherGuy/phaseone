//! The capabilities this runtime links into a module's per-call instance, and only those the
//! manifest grants (freeze item 3): `control`, `clock` and `random` are the runtime's own,
//! `process`, `summary`, `completion`, `workspace` and `snapshot` are services the caller
//! passes in explicitly — there is no registry.
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

use crate::completion::{CompletionService, link_completion};
use crate::context_policy::{SummaryService, link_summary};
use crate::delegation::{
    WorkerServices, WorkflowServices, link_workers_control, link_workers_observe,
    link_workers_start, link_workflows,
};
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

/// Why a workspace operation failed (`workspace.fs-error`). Every message is the host's and
/// safe to show the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    /// The path resolves outside the workspace root.
    OutsideWorkspace,
    /// Nothing is at the path.
    NotFound,
    /// The path is not the kind the operation needs.
    WrongKind,
    /// Something is already at the path.
    AlreadyExists,
    /// A search pattern or glob that does not parse.
    InvalidPattern(String),
    /// The call was cancelled while the host worked on the request.
    Cancelled,
    /// Any other failure, as the host words it for the model.
    Io(String),
}

/// What a path names (`workspace.entry-kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    /// A regular file.
    File,
    /// A directory.
    Directory,
    /// Anything else.
    Other,
}

/// One path as `workspace.stat` describes it (`workspace.entry`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceEntry {
    /// Relative to the workspace root, `/`-separated: the form the model is shown.
    pub path: String,
    /// What is there.
    pub kind: EntryKind,
    /// Size in bytes; zero for anything but a file.
    pub size: u64,
}

/// The read side of the confined workspace a module's `workspace` capability is linked to
/// (`p1-workspace`, S1). Confinement is the service's: every path it is given is the
/// module's, unchecked. Only `stat` and `read` are linked; `list-files` and `search` belong
/// to the slice that moves a tool needing them, and a module importing them fails to
/// instantiate until then.
pub trait WorkspaceService: Send + Sync {
    /// Resolves `path` and describes what is there.
    fn stat(&self, path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>>;

    /// Up to `length` bytes of the file at `path` from byte `offset`; fewer, or none, at the
    /// end of the file.
    fn read(
        &self,
        path: String,
        offset: u64,
        length: u64,
    ) -> BoxFuture<'_, Result<Vec<u8>, FsError>>;
}

/// What an agent's last observation of a path says about contents it read again
/// (`snapshot.observation`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotObservation {
    /// This agent never observed the path.
    NeverObserved,
    /// The contents match the last observation.
    Unchanged,
    /// The contents changed since the last observation.
    ChangedSinceObserved,
}

/// The agent's observed-file registry a module's `snapshot` capability is linked to
/// (`p1-workspace`'s `ObservedFiles`, S1).
pub trait SnapshotService: Send + Sync {
    /// Records `contents` as the agent's observation of `path`.
    fn observe(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>>;

    /// Compares `current` with the agent's last observation of `path`.
    fn check(
        &self,
        path: String,
        current: Vec<u8>,
    ) -> BoxFuture<'_, Result<SnapshotObservation, FsError>>;
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
    /// The `completion` capability: the host's completion hub
    /// ([`crate::completion::link_completion`]).
    pub completion: Option<Arc<dyn CompletionService>>,
    /// The `workspace` capability, read side (S1).
    pub workspace: Option<Arc<dyn WorkspaceService>>,
    /// The `snapshot` capability (S1).
    pub snapshot: Option<Arc<dyn SnapshotService>>,
    /// The `workers-start`, `workers-observe` and `workers-control` capabilities, one
    /// optional service each ([`crate::delegation`], S6).
    pub workers: Option<WorkerServices>,
    /// The `workflows` capability ([`crate::delegation`], S6).
    pub workflows: Option<WorkflowServices>,
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
    pub(crate) completion: Option<Arc<dyn CompletionService>>,
    workspace: Option<Arc<dyn WorkspaceService>>,
    snapshot: Option<Arc<dyn SnapshotService>>,
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
            completion: services.completion.clone(),
            workspace: services.workspace.clone(),
            snapshot: services.snapshot.clone(),
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
            "completion" => {
                if services.completion.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_completion(&mut linker)
            }
            "workspace" => {
                if services.workspace.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_workspace(&mut linker)
            }
            "snapshot" => {
                if services.snapshot.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_snapshot(&mut linker)
            }
            "workers-start" => match services.workers.as_ref().and_then(|w| w.start.clone()) {
                Some(start) => link_workers_start(&mut linker, start),
                None => return Err(LinkError::MissingService(capability.clone())),
            },
            "workers-observe" => match services.workers.as_ref().and_then(|w| w.observe.clone()) {
                Some(observe) => link_workers_observe(&mut linker, observe),
                None => return Err(LinkError::MissingService(capability.clone())),
            },
            "workers-control" => match services.workers.as_ref().and_then(|w| w.control.clone()) {
                Some(control) => link_workers_control(&mut linker, control),
                None => return Err(LinkError::MissingService(capability.clone())),
            },
            "workflows" => match services.workflows.clone() {
                Some(workflows) => link_workflows(&mut linker, workflows),
                None => return Err(LinkError::MissingService(capability.clone())),
            },
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

/// Runs a service request for a call, answering `cancelled` as soon as the call is cancelled
/// — also when it already was — as `workspace.wit` defines for every file operation.
async fn unless_cancelled<T>(
    cancel: &CancellationToken,
    request: BoxFuture<'_, Result<T, FsError>>,
) -> Result<T, FsError> {
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err(FsError::Cancelled),
        result = request => result,
    }
}

fn link_workspace(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut workspace = linker.instance(&interface_import("workspace"))?;
    workspace.func_new_async("stat", |store, _ty, params, results| {
        Box::new(async move {
            let path = string_param(params, 0, "workspace.stat")?;
            let Some(service) = store.data().workspace.clone() else {
                bail!("workspace.stat called without a workspace service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = fs_result(
                unless_cancelled(&cancel, service.stat(path))
                    .await
                    .map(entry_val),
            );
            Ok(())
        })
    })?;
    workspace.func_new_async("read", |store, _ty, params, results| {
        Box::new(async move {
            let path = string_param(params, 0, "workspace.read")?;
            let (Some(Val::U64(offset)), Some(Val::U64(length))) = (params.get(1), params.get(2))
            else {
                bail!("workspace.read: offset and length are not u64");
            };
            let Some(service) = store.data().workspace.clone() else {
                bail!("workspace.read called without a workspace service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = fs_result(
                unless_cancelled(&cancel, service.read(path, *offset, *length))
                    .await
                    .map(bytes_val),
            );
            Ok(())
        })
    })
}

fn link_snapshot(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut snapshot = linker.instance(&interface_import("snapshot"))?;
    snapshot.func_new_async("observe", |store, _ty, params, results| {
        Box::new(async move {
            let path = string_param(params, 0, "snapshot.observe")?;
            let contents = bytes_param(params, 1, "snapshot.observe")?;
            let Some(service) = store.data().snapshot.clone() else {
                bail!("snapshot.observe called without a snapshot service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = fs_result(
                unless_cancelled(&cancel, service.observe(path, contents))
                    .await
                    .map(|()| None),
            );
            Ok(())
        })
    })?;
    snapshot.func_new_async("check", |store, _ty, params, results| {
        Box::new(async move {
            let path = string_param(params, 0, "snapshot.check")?;
            let current = bytes_param(params, 1, "snapshot.check")?;
            let Some(service) = store.data().snapshot.clone() else {
                bail!("snapshot.check called without a snapshot service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = fs_result(
                unless_cancelled(&cancel, service.check(path, current))
                    .await
                    .map(|observation| Some(observation_val(observation))),
            );
            Ok(())
        })
    })
}

fn string_param(params: &[Val], index: usize, function: &str) -> wasmtime::Result<String> {
    match params.get(index) {
        Some(Val::String(text)) => Ok(text.clone()),
        _ => bail!("{function}: parameter {index} is not a string"),
    }
}

fn bytes_param(params: &[Val], index: usize, function: &str) -> wasmtime::Result<Vec<u8>> {
    let Some(Val::List(items)) = params.get(index) else {
        bail!("{function}: parameter {index} is not a list");
    };
    items
        .iter()
        .map(|item| match item {
            Val::U8(byte) => Ok(*byte),
            _ => bail!("{function}: parameter {index} is not a list of bytes"),
        })
        .collect()
}

/// A `result<T, fs-error>`, `ok` carrying `None` for `result<_, fs-error>`.
fn fs_result(result: Result<Option<Val>, FsError>) -> Val {
    Val::Result(match result {
        Ok(value) => Ok(value.map(Box::new)),
        Err(error) => Err(Some(Box::new(fs_error_val(error)))),
    })
}

fn fs_error_val(error: FsError) -> Val {
    let (case, payload) = match error {
        FsError::OutsideWorkspace => ("outside-workspace", None),
        FsError::NotFound => ("not-found", None),
        FsError::WrongKind => ("wrong-kind", None),
        FsError::AlreadyExists => ("already-exists", None),
        FsError::InvalidPattern(message) => ("invalid-pattern", Some(message)),
        FsError::Cancelled => ("cancelled", None),
        FsError::Io(message) => ("io", Some(message)),
    };
    Val::Variant(
        case.to_owned(),
        payload.map(|message| Box::new(Val::String(message))),
    )
}

fn entry_val(entry: WorkspaceEntry) -> Option<Val> {
    let kind = match entry.kind {
        EntryKind::File => "file",
        EntryKind::Directory => "directory",
        EntryKind::Other => "other",
    };
    Some(Val::Record(vec![
        ("path".to_owned(), Val::String(entry.path)),
        ("kind".to_owned(), Val::Enum(kind.to_owned())),
        ("size".to_owned(), Val::U64(entry.size)),
    ]))
}

fn bytes_val(bytes: Vec<u8>) -> Option<Val> {
    Some(Val::List(bytes.into_iter().map(Val::U8).collect()))
}

fn observation_val(observation: SnapshotObservation) -> Val {
    Val::Enum(
        match observation {
            SnapshotObservation::NeverObserved => "never-observed",
            SnapshotObservation::Unchanged => "unchanged",
            SnapshotObservation::ChangedSinceObserved => "changed-since-observed",
        }
        .to_owned(),
    )
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

    struct NoFiles;

    impl WorkspaceService for NoFiles {
        fn stat(&self, _path: String) -> BoxFuture<'_, Result<WorkspaceEntry, FsError>> {
            Box::pin(async { Err(FsError::NotFound) })
        }

        fn read(
            &self,
            _path: String,
            _offset: u64,
            _length: u64,
        ) -> BoxFuture<'_, Result<Vec<u8>, FsError>> {
            Box::pin(async { Err(FsError::NotFound) })
        }
    }

    impl SnapshotService for NoFiles {
        fn observe(&self, _path: String, _contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
            Box::pin(async { Ok(()) })
        }

        fn check(
            &self,
            _path: String,
            _current: Vec<u8>,
        ) -> BoxFuture<'_, Result<SnapshotObservation, FsError>> {
            Box::pin(async { Ok(SnapshotObservation::NeverObserved) })
        }
    }

    #[test]
    fn workspace_and_snapshot_link_only_with_their_services() {
        let engine = crate::engine().expect("engine");
        for capability in ["workspace", "snapshot"] {
            match capability_linker(&engine, &granted(&[capability]), &Services::default()) {
                Err(LinkError::MissingService(missing)) => assert_eq!(missing, capability),
                Err(other) => panic!("wrong link error: {other}"),
                Ok(_) => panic!("{capability} must not link without its service"),
            }
        }
        let files = Arc::new(NoFiles);
        let services = Services {
            workspace: Some(files.clone()),
            snapshot: Some(files),
            ..Services::default()
        };
        assert!(
            capability_linker(
                &engine,
                &granted(&["control", "workspace", "snapshot"]),
                &services
            )
            .is_ok()
        );
    }

    #[test]
    fn fs_errors_are_the_wit_variant_cases() {
        assert_eq!(
            fs_error_val(FsError::Io("disk".to_owned())),
            Val::Variant(
                "io".to_owned(),
                Some(Box::new(Val::String("disk".to_owned())))
            )
        );
        assert_eq!(
            fs_error_val(FsError::OutsideWorkspace),
            Val::Variant("outside-workspace".to_owned(), None)
        );
        assert_eq!(
            bytes_param(&[Val::List(vec![Val::U8(1), Val::U8(2)])], 0, "f").unwrap(),
            vec![1, 2]
        );
        assert!(bytes_param(&[Val::List(vec![Val::U32(1)])], 0, "f").is_err());
    }

    #[test]
    fn a_granted_completion_without_its_service_is_a_missing_service() {
        let engine = crate::engine().expect("engine");
        match capability_linker(&engine, &granted(&["completion"]), &Services::default()) {
            Err(LinkError::MissingService(capability)) => assert_eq!(capability, "completion"),
            Err(other) => panic!("wrong link error: {other}"),
            Ok(_) => panic!("completion must not link without a completion service"),
        }
    }
}
