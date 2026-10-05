//! The capabilities this runtime links into a module's per-call instance, and only those the
//! manifest grants (freeze item 3): `control`, `clock` and `random` are the runtime's own,
//! `process`, `summary`, `completion`, `workspace`, `snapshot`, `workspace-mutation` and
//! `tool-outputs` are services the caller passes in explicitly — there is no registry.
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
use crate::outputs::{ToolOutputsService, link_tool_outputs};

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
/// is [`process::ProcessCapability`](crate::process::ProcessCapability) over this crate's
/// native process service.
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

/// A content search as a module asks for it (`workspace.search-query`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchQuery {
    /// A regular expression in the host's (ripgrep) syntax.
    pub pattern: String,
    /// The file or directory to search; the root when absent.
    pub path: Option<String>,
    /// Keeps only matching files.
    pub glob: Option<String>,
    /// Match regardless of case.
    pub case_insensitive: bool,
    /// Lines of context before and after each match.
    pub context: u32,
    /// The most lines (matches and context) the result carries.
    pub max_lines: u32,
}

/// One match or context line (`workspace.search-line`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchLine {
    /// One-based.
    pub line_number: u64,
    /// The line without its terminator.
    pub text: String,
    /// False for a context line.
    pub is_match: bool,
}

/// The lines one file contributed (`workspace.file-matches`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileMatches {
    /// Relative to the workspace root, `/`-separated.
    pub path: String,
    /// In file order.
    pub lines: Vec<SearchLine>,
}

/// What a search found (`workspace.search-result`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SearchResult {
    /// In bytewise displayed-path order (the listing's order).
    pub files: Vec<FileMatches>,
    /// The search stopped at `max_lines` before the walk ended.
    pub truncated: bool,
    /// Files with matches after the one where it stopped.
    pub omitted_files: u64,
}

/// The read side of the confined workspace a module's `workspace` capability is linked to
/// (`p1-workspace`). Confinement is the service's: every path it is given is the module's,
/// unchecked. `stat` and `read` are S1's; `list-files` and `search` were added beside them by
/// S2 for the search tool (U-search.3), with default bodies that refuse, so a service for a
/// tool that is not granted the walk (S1's read capability) needs no code for them.
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

    /// The files under the directory `path`, sorted, relative to the root; `glob` keeps only
    /// matching files. The default refuses: only a search service walks the workspace.
    fn list_files(
        &self,
        path: String,
        glob: Option<String>,
    ) -> BoxFuture<'_, Result<Vec<String>, FsError>> {
        let _ = (path, glob);
        Box::pin(async { Err(FsError::Io(LIST_FILES_NOT_GRANTED.to_owned())) })
    }

    /// Searches file contents with the walk of `list_files`. The default refuses, as
    /// `list_files` does.
    fn search(&self, query: SearchQuery) -> BoxFuture<'_, Result<SearchResult, FsError>> {
        let _ = query;
        Box::pin(async { Err(FsError::Io(SEARCH_NOT_GRANTED.to_owned())) })
    }
}

/// The refusal of a workspace service that does not walk the workspace.
pub const LIST_FILES_NOT_GRANTED: &str = "list-files is not granted to this tool";
/// The refusal of a workspace service that does not search the workspace.
pub const SEARCH_NOT_GRANTED: &str = "search is not granted to this tool";

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

/// The write side of the confined workspace a module's `workspace-mutation` capability is
/// linked to (`p1-workspace`'s owned mutation, S2). The host builds one per agent, over the
/// agent's observations and with the mutation policy of the assembly (observed for edit and
/// write, patch-authorized for patch): the module never chooses it.
pub trait MutationService: Send + Sync {
    /// Waits for the workspace's write gate without blocking a thread and returns it held.
    /// The runtime drops this future when the call is cancelled while it waits, so waiting
    /// must hold nothing.
    fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>>;
}

/// The write gate held for one export call (`workspace-mutation.mutation`). Dropping it
/// releases the gate: the runtime drops it when the module drops the resource and, at the
/// latest, when the call ends (however it ends).
pub trait HeldMutation: Send {
    /// Replaces the file at `path` with `contents`, creating missing parent directories.
    fn write(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>>;

    /// As `write`, but `AlreadyExists` when anything is at `path`.
    fn create(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>>;

    /// Removes the file at `path`.
    fn remove(&self, path: String) -> BoxFuture<'_, Result<(), FsError>>;

    /// Moves the file at `old_path` to `new_path`; `AlreadyExists` when anything is at
    /// `new_path`.
    fn rename(&self, old_path: String, new_path: String) -> BoxFuture<'_, Result<(), FsError>>;
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
    /// The `workspace-mutation` capability (S2).
    pub workspace_mutation: Option<Arc<dyn MutationService>>,
    /// The `workers-start`, `workers-observe` and `workers-control` capabilities, one
    /// optional service each ([`crate::delegation`], S6).
    pub workers: Option<WorkerServices>,
    /// The `workflows` capability ([`crate::delegation`], S6).
    pub workflows: Option<WorkflowServices>,
    /// The `tool-outputs` capability: the host's store of what a tool's commands printed
    /// ([`crate::outputs`], ADR-0109).
    pub tool_outputs: Option<Arc<dyn ToolOutputsService>>,
    /// The bounded directory listing, linked only to p1/ls (ADR-0115).
    pub directory_listing: Option<Arc<dyn crate::directory_listing::DirectoryListingService>>,
    /// The services whose state belongs to ONE export call (the read record a mutation
    /// rechecks against, ADR-0092): called once at the start of every call, before its
    /// Store, and each service it returns serves that call in place of the field above.
    /// The fields above are what the linker checks against the manifest, so a scope
    /// returns a service only where the field above holds one.
    pub call_scope: Option<CallScope>,
}

/// Builds the call-scoped services of one export call ([`Services::call_scope`]).
pub type CallScope = Arc<dyn Fn() -> Services + Send + Sync>;

impl Services {
    /// Services whose state is fresh for every call: `build` is called once here, for the
    /// capabilities an assembly links, and again at the start of every call, whose services
    /// they then are. What one call records is never seen by another, and two concurrent
    /// calls never share it.
    pub fn call_scoped(build: impl Fn() -> Services + Send + Sync + 'static) -> Self {
        let build: CallScope = Arc::new(build);
        Self {
            call_scope: Some(build.clone()),
            ..build()
        }
    }

    /// The services one call is served with: the scope's, where it returns one, else these.
    fn for_call(&self) -> Services {
        let Some(scope) = &self.call_scope else {
            return self.clone();
        };
        let call = scope();
        Services {
            process: call.process.or_else(|| self.process.clone()),
            summary: call.summary.or_else(|| self.summary.clone()),
            completion: call.completion.or_else(|| self.completion.clone()),
            workspace: call.workspace.or_else(|| self.workspace.clone()),
            snapshot: call.snapshot.or_else(|| self.snapshot.clone()),
            workspace_mutation: call
                .workspace_mutation
                .or_else(|| self.workspace_mutation.clone()),
            workers: call.workers.or_else(|| self.workers.clone()),
            workflows: call.workflows.or_else(|| self.workflows.clone()),
            tool_outputs: call.tool_outputs.or_else(|| self.tool_outputs.clone()),
            directory_listing: call
                .directory_listing
                .or_else(|| self.directory_listing.clone()),
            call_scope: None,
        }
    }
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
    pub(crate) limits: crate::executor::MemoryLimiter,
    pub(crate) cancel: CancellationToken,
    pub(crate) table: ResourceTable,
    process: Option<Arc<dyn ProcessService>>,
    pub(crate) summary: Option<Arc<dyn SummaryService>>,
    pub(crate) completion: Option<Arc<dyn CompletionService>>,
    workspace: Option<Arc<dyn WorkspaceService>>,
    snapshot: Option<Arc<dyn SnapshotService>>,
    workspace_mutation: Option<Arc<dyn MutationService>>,
    pub(crate) tool_outputs: Option<Arc<dyn ToolOutputsService>>,
    pub(crate) directory_listing:
        Option<Arc<dyn crate::directory_listing::DirectoryListingService>>,
    /// How many `workspace-mutation.mutation` resources this call holds in its table: the
    /// gate is not re-entrant, so a `begin` while one is held would wait on itself.
    mutations_held: usize,
    /// The origin of `clock.monotonic-now`, fixed per instance.
    origin: Instant,
    /// The call was cancelled and its fuel cut to the grace it gets to return.
    pub(crate) cancel_grace: bool,
}

impl CallState {
    /// The state of one call over `services`: a call-scoped part is built here, once per
    /// call, so it never outlives the call or reaches another.
    pub(crate) fn new(cancel: CancellationToken, services: &Services) -> Self {
        let services = &services.for_call();
        Self {
            limits: crate::executor::store_limits(),
            cancel,
            table: ResourceTable::new(),
            process: services.process.clone(),
            summary: services.summary.clone(),
            completion: services.completion.clone(),
            workspace: services.workspace.clone(),
            snapshot: services.snapshot.clone(),
            workspace_mutation: services.workspace_mutation.clone(),
            tool_outputs: services.tool_outputs.clone(),
            directory_listing: services.directory_listing.clone(),
            mutations_held: 0,
            origin: Instant::now(),
            cancel_grace: false,
        }
    }
}

impl crate::executor::LimitedStore for CallState {
    fn limits(&mut self) -> &mut crate::executor::MemoryLimiter {
        &mut self.limits
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
            "workspace-mutation" => {
                if services.workspace_mutation.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_workspace_mutation(&mut linker)
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
            "tool-outputs" => {
                if services.tool_outputs.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                link_tool_outputs(&mut linker)
            }
            "directory-listing" => {
                if services.directory_listing.is_none() {
                    return Err(LinkError::MissingService(capability.clone()));
                }
                crate::directory_listing::link(&mut linker)
            }
            // Some capabilities are valid for other classes but have no linker here.
            other => Err(wasmtime::format_err!(
                "{other} has no linker in this runtime"
            )),
        };
        result.map_err(wasmtime_error(capability))?;
    }
    Ok(linker)
}

/// Reject a malformed component signature before indexing dynamic parameter or result slices.
pub(crate) fn check_arity(
    name: &str,
    params: &[Val],
    results: &[Val],
    expected_params: usize,
    expected_results: usize,
) -> wasmtime::Result<()> {
    if params.len() != expected_params || results.len() != expected_results {
        bail!("{name}: invalid host function signature");
    }
    Ok(())
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
    clock.func_new_async("now", |_store, _ty, params, results| {
        Box::new(async move {
            check_arity("clock.now", params, results, 0, 1)?;
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
pub(crate) fn random_bytes(len: u32) -> wasmtime::Result<Vec<u8>> {
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
            check_arity("process.spawn", params, results, 1, 1)?;
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
            check_arity("process.running.next", params, results, 1, 1)?;
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
pub(crate) async fn unless_cancelled<T>(
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
            check_arity("workspace.stat", params, results, 1, 1)?;
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
            check_arity("workspace.read", params, results, 3, 1)?;
            let path = string_param(params, 0, "workspace.read")?;
            let (Some(Val::U64(offset)), Some(Val::U64(length))) = (params.get(1), params.get(2))
            else {
                bail!("workspace.read: offset and length are not u64");
            };
            if *length > crate::executor::MAX_TRANSFER_BYTES as u64 {
                results[0] = fs_result(Err(FsError::Io(
                    "workspace.read window exceeds the file transfer limit".to_owned(),
                )));
                return Ok(());
            }
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
    })?;
    workspace.func_new_async("list-files", |store, _ty, params, results| {
        Box::new(async move {
            check_arity("workspace.list-files", params, results, 2, 1)?;
            let path = string_param(params, 0, "workspace.list-files")?;
            let glob = option_string(params.get(1), "workspace.list-files", "glob")?;
            let Some(service) = store.data().workspace.clone() else {
                bail!("workspace.list-files called without a workspace service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = fs_result(
                unless_cancelled(&cancel, service.list_files(path, glob))
                    .await
                    .map(|paths| Some(Val::List(paths.into_iter().map(Val::String).collect()))),
            );
            Ok(())
        })
    })?;
    workspace.func_new_async("search", |store, _ty, params, results| {
        Box::new(async move {
            check_arity("workspace.search", params, results, 1, 1)?;
            let Some(query) = params.first() else {
                bail!("workspace.search called without its query");
            };
            let query = search_query(query)?;
            let Some(service) = store.data().workspace.clone() else {
                bail!("workspace.search called without a workspace service");
            };
            let cancel = store.data().cancel.clone();
            results[0] = fs_result(
                unless_cancelled(&cancel, service.search(query))
                    .await
                    .map(search_result_val),
            );
            Ok(())
        })
    })
}

/// An `option<string>` field or parameter.
fn option_string(
    value: Option<&Val>,
    function: &str,
    name: &str,
) -> wasmtime::Result<Option<String>> {
    match value {
        Some(Val::Option(None)) => Ok(None),
        Some(Val::Option(Some(inner))) => match inner.as_ref() {
            Val::String(text) => Ok(Some(text.clone())),
            _ => bail!("{function}: {name} is not an optional string"),
        },
        _ => bail!("{function}: {name} is not an optional string"),
    }
}

fn search_query(value: &Val) -> wasmtime::Result<SearchQuery> {
    const FUNCTION: &str = "workspace.search";
    let Val::Record(fields) = value else {
        bail!("{FUNCTION}: query is not a record");
    };
    let mut pattern = None;
    let mut path = None;
    let mut glob = None;
    let mut case_insensitive = None;
    let mut context = None;
    let mut max_lines = None;
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("pattern", Val::String(text)) => pattern = Some(text.clone()),
            ("path", value) => path = Some(option_string(Some(value), FUNCTION, name)?),
            ("glob", value) => glob = Some(option_string(Some(value), FUNCTION, name)?),
            ("case-insensitive", Val::Bool(flag)) => case_insensitive = Some(*flag),
            ("context", Val::U32(lines)) => context = Some(*lines),
            ("max-lines", Val::U32(lines)) => max_lines = Some(*lines),
            _ => bail!("{FUNCTION}: unexpected query field {name}"),
        }
    }
    match (pattern, path, glob, case_insensitive, context, max_lines) {
        (
            Some(pattern),
            Some(path),
            Some(glob),
            Some(case_insensitive),
            Some(context),
            Some(max_lines),
        ) => Ok(SearchQuery {
            pattern,
            path,
            glob,
            case_insensitive,
            context,
            max_lines,
        }),
        _ => bail!("{FUNCTION}: query is missing a field"),
    }
}

fn search_result_val(result: SearchResult) -> Option<Val> {
    let files = result
        .files
        .into_iter()
        .map(|file| {
            let lines = file
                .lines
                .into_iter()
                .map(|line| {
                    Val::Record(vec![
                        ("line-number".to_owned(), Val::U64(line.line_number)),
                        ("text".to_owned(), Val::String(line.text)),
                        ("is-match".to_owned(), Val::Bool(line.is_match)),
                    ])
                })
                .collect();
            Val::Record(vec![
                ("path".to_owned(), Val::String(file.path)),
                ("lines".to_owned(), Val::List(lines)),
            ])
        })
        .collect();
    Some(Val::Record(vec![
        ("files".to_owned(), Val::List(files)),
        ("truncated".to_owned(), Val::Bool(result.truncated)),
        ("omitted-files".to_owned(), Val::U64(result.omitted_files)),
    ]))
}

fn link_snapshot(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut snapshot = linker.instance(&interface_import("snapshot"))?;
    snapshot.func_new_async("observe", |store, _ty, params, results| {
        Box::new(async move {
            check_arity("snapshot.observe", params, results, 2, 1)?;
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
            check_arity("snapshot.check", params, results, 2, 1)?;
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

/// A `workspace-mutation.mutation` as the host holds it: the service's held gate, in the
/// call's resource table.
pub(crate) struct HostMutation(Box<dyn HeldMutation>);

/// The `mutation` a call cancelled while `begin` waited gets: `begin` has no error result, so
/// the cancellation reaches the guest where `workspace.wit` puts it for every file operation —
/// each use answers `cancelled`. It holds no gate, so nothing else waits on it.
struct CancelledBegin;

fn cancelled_begin() -> Box<dyn HeldMutation> {
    Box::new(CancelledBegin)
}

impl HeldMutation for CancelledBegin {
    fn write(&self, _path: String, _contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async { Err(FsError::Cancelled) })
    }

    fn create(&self, _path: String, _contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async { Err(FsError::Cancelled) })
    }

    fn remove(&self, _path: String) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async { Err(FsError::Cancelled) })
    }

    fn rename(&self, _old_path: String, _new_path: String) -> BoxFuture<'_, Result<(), FsError>> {
        Box::pin(async { Err(FsError::Cancelled) })
    }
}

fn link_workspace_mutation(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut mutation = linker.instance(&interface_import("workspace-mutation"))?;
    mutation.resource(
        "mutation",
        ResourceType::host::<HostMutation>(),
        |mut store, rep| {
            // Dropping the entry drops the service's held gate, which releases it. What the
            // call still holds when it returns goes with its Store the same way.
            store
                .data_mut()
                .table
                .delete(Resource::<HostMutation>::new_own(rep))?;
            let state = store.data_mut();
            state.mutations_held = state.mutations_held.saturating_sub(1);
            Ok(())
        },
    )?;
    mutation.func_new_async("begin", |mut store, _ty, params, results| {
        Box::new(async move {
            check_arity("workspace-mutation.begin", params, results, 0, 1)?;
            let Some(service) = store.data().workspace_mutation.clone() else {
                bail!("workspace-mutation.begin called without a workspace-mutation service");
            };
            // The gate is not re-entrant and `begin` has no error to answer with: waiting
            // would park this call on the gate it holds, keeping it from every other agent
            // until the deadline, so the call traps instead and its Store releases the gate.
            if store.data().mutations_held > 0 {
                bail!("workspace-mutation.begin called while this call already holds a mutation");
            }
            let cancel = store.data().cancel.clone();
            // Raced against the cancellation so a cancelled call does not wait out another
            // writer: it gets a mutation whose every use answers `cancelled`.
            let held = tokio::select! {
                biased;
                () = cancel.cancelled() => cancelled_begin(),
                held = service.begin() => held,
            };
            let entry = store.data_mut().table.push(HostMutation(held))?;
            store.data_mut().mutations_held += 1;
            let handle = ResourceAny::try_from_resource(entry, &mut store)?;
            results[0] = Val::Resource(handle);
            Ok(())
        })
    })?;
    mutation.func_new_async(
        "[method]mutation.write",
        |mut store, _ty, params, results| {
            Box::new(async move {
                check_arity("workspace-mutation.write", params, results, 3, 1)?;
                let path = string_param(params, 1, "workspace-mutation.mutation.write")?;
                let contents = bytes_param(params, 2, "workspace-mutation.mutation.write")?;
                let cancel = store.data().cancel.clone();
                let entry = mutation_entry(&mut store, params, "write")?;
                results[0] = fs_result(
                    unless_cancelled(&cancel, entry.0.write(path, contents))
                        .await
                        .map(|()| None),
                );
                Ok(())
            })
        },
    )?;
    mutation.func_new_async(
        "[method]mutation.create",
        |mut store, _ty, params, results| {
            Box::new(async move {
                check_arity("workspace-mutation.create", params, results, 3, 1)?;
                let path = string_param(params, 1, "workspace-mutation.mutation.create")?;
                let contents = bytes_param(params, 2, "workspace-mutation.mutation.create")?;
                let cancel = store.data().cancel.clone();
                let entry = mutation_entry(&mut store, params, "create")?;
                results[0] = fs_result(
                    unless_cancelled(&cancel, entry.0.create(path, contents))
                        .await
                        .map(|()| None),
                );
                Ok(())
            })
        },
    )?;
    mutation.func_new_async(
        "[method]mutation.remove",
        |mut store, _ty, params, results| {
            Box::new(async move {
                check_arity("workspace-mutation.remove", params, results, 2, 1)?;
                let path = string_param(params, 1, "workspace-mutation.mutation.remove")?;
                let cancel = store.data().cancel.clone();
                let entry = mutation_entry(&mut store, params, "remove")?;
                results[0] = fs_result(
                    unless_cancelled(&cancel, entry.0.remove(path))
                        .await
                        .map(|()| None),
                );
                Ok(())
            })
        },
    )?;
    mutation.func_new_async(
        "[method]mutation.rename",
        |mut store, _ty, params, results| {
            Box::new(async move {
                check_arity("workspace-mutation.rename", params, results, 3, 1)?;
                let old_path = string_param(params, 1, "workspace-mutation.mutation.rename")?;
                let new_path = string_param(params, 2, "workspace-mutation.mutation.rename")?;
                let cancel = store.data().cancel.clone();
                let entry = mutation_entry(&mut store, params, "rename")?;
                results[0] = fs_result(
                    unless_cancelled(&cancel, entry.0.rename(old_path, new_path))
                        .await
                        .map(|()| None),
                );
                Ok(())
            })
        },
    )
}

/// The held mutation a method call names. A handle the module dropped, or one the call no
/// longer holds, fails here: a later use traps.
fn mutation_entry<'s>(
    store: &'s mut wasmtime::StoreContextMut<'_, CallState>,
    params: &[Val],
    method: &str,
) -> wasmtime::Result<&'s HostMutation> {
    let Some(Val::Resource(handle)) = params.first() else {
        bail!("workspace-mutation.mutation.{method} called without its resource");
    };
    let mutation: Resource<HostMutation> = handle.try_into_resource(&mut *store)?;
    Ok(store.data().table.get(&mutation)?)
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
pub(crate) fn fs_result(result: Result<Option<Val>, FsError>) -> Val {
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
    fn workspace_mutation_links_only_with_its_service() {
        struct NoGate;

        impl MutationService for NoGate {
            fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
                Box::pin(async { cancelled_begin() })
            }
        }

        let engine = crate::engine().expect("engine");
        match capability_linker(
            &engine,
            &granted(&["workspace-mutation"]),
            &Services::default(),
        ) {
            Err(LinkError::MissingService(missing)) => assert_eq!(missing, "workspace-mutation"),
            Err(other) => panic!("wrong link error: {other}"),
            Ok(_) => panic!("workspace-mutation must not link without its service"),
        }
        let services = Services {
            workspace_mutation: Some(Arc::new(NoGate)),
            ..Services::default()
        };
        assert!(
            capability_linker(
                &engine,
                &granted(&["control", "workspace-mutation"]),
                &services
            )
            .is_ok()
        );
    }

    struct TestMutationGate {
        gate: Arc<tokio::sync::Mutex<()>>,
        begins: std::sync::atomic::AtomicUsize,
    }

    struct TestHeldMutation {
        _gate: tokio::sync::OwnedMutexGuard<()>,
    }

    impl MutationService for TestMutationGate {
        fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
            self.begins
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async {
                Box::new(TestHeldMutation {
                    _gate: self.gate.clone().lock_owned().await,
                }) as Box<dyn HeldMutation>
            })
        }
    }

    impl HeldMutation for TestHeldMutation {
        fn write(&self, _path: String, _contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
            Box::pin(async { Ok(()) })
        }

        fn create(&self, path: String, contents: Vec<u8>) -> BoxFuture<'_, Result<(), FsError>> {
            self.write(path, contents)
        }

        fn remove(&self, path: String) -> BoxFuture<'_, Result<(), FsError>> {
            self.write(path, Vec::new())
        }

        fn rename(&self, path: String, _new_path: String) -> BoxFuture<'_, Result<(), FsError>> {
            self.write(path, Vec::new())
        }
    }

    // `wasm-tools parse` then `wasm-tools strip --all` of this WAT; inline bytes because the
    // runtime has no WAT parser and the secret scan refuses tracked binaries.
    // (component
    //   (import "p1:module/workspace-mutation@1.0.0" (instance $host
    //     (export "mutation" (type (sub resource)))
    //     (export "begin" (func (result (own 0))))))
    //   (alias export $host "mutation" (type $mutation))
    //   (alias export $host "begin" (func $begin))
    //   (core func $begin (canon lower (func $begin)))
    //   (core func $drop (canon resource.drop $mutation))
    //   (core module $probe
    //     (import "host" "begin" (func $begin (result i32)))
    //     (import "host" "drop" (func $drop (param i32)))
    //     (func (export "reenter") (result i32)
    //       (drop (call $begin))
    //       (call $begin))
    //     (func (export "release") (result i32)
    //       (call $drop (call $begin))
    //       (call $drop (call $begin))
    //       (i32.const 2)))
    //   (core instance $lowered
    //     (export "begin" (func $begin))
    //     (export "drop" (func $drop)))
    //   (core instance $probe (instantiate $probe (with "host" (instance $lowered))))
    //   (func (export "reenter") (result u32) (canon lift (core func $probe "reenter")))
    //   (func (export "release") (result u32) (canon lift (core func $probe "release"))))
    const MUTATION_PROBE: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00, 0x07, 0x22, 0x01, 0x42, 0x04, 0x04, 0x00,
        0x08, 0x6d, 0x75, 0x74, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x03, 0x01, 0x01, 0x69, 0x00, 0x01,
        0x40, 0x00, 0x00, 0x01, 0x04, 0x00, 0x05, 0x62, 0x65, 0x67, 0x69, 0x6e, 0x01, 0x02, 0x0a,
        0x27, 0x01, 0x00, 0x22, 0x70, 0x31, 0x3a, 0x6d, 0x6f, 0x64, 0x75, 0x6c, 0x65, 0x2f, 0x77,
        0x6f, 0x72, 0x6b, 0x73, 0x70, 0x61, 0x63, 0x65, 0x2d, 0x6d, 0x75, 0x74, 0x61, 0x74, 0x69,
        0x6f, 0x6e, 0x40, 0x31, 0x2e, 0x30, 0x2e, 0x30, 0x05, 0x00, 0x06, 0x16, 0x02, 0x03, 0x00,
        0x00, 0x08, 0x6d, 0x75, 0x74, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x01, 0x00, 0x00, 0x05, 0x62,
        0x65, 0x67, 0x69, 0x6e, 0x08, 0x07, 0x02, 0x01, 0x00, 0x00, 0x00, 0x03, 0x01, 0x01, 0x63,
        0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x09, 0x02, 0x60, 0x00, 0x01, 0x7f,
        0x60, 0x01, 0x7f, 0x00, 0x02, 0x1a, 0x02, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x05, 0x62, 0x65,
        0x67, 0x69, 0x6e, 0x00, 0x00, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x04, 0x64, 0x72, 0x6f, 0x70,
        0x00, 0x01, 0x03, 0x03, 0x02, 0x00, 0x00, 0x07, 0x15, 0x02, 0x07, 0x72, 0x65, 0x65, 0x6e,
        0x74, 0x65, 0x72, 0x00, 0x02, 0x07, 0x72, 0x65, 0x6c, 0x65, 0x61, 0x73, 0x65, 0x00, 0x03,
        0x0a, 0x16, 0x02, 0x07, 0x00, 0x10, 0x00, 0x1a, 0x10, 0x00, 0x0b, 0x0c, 0x00, 0x10, 0x00,
        0x10, 0x01, 0x10, 0x00, 0x10, 0x01, 0x41, 0x02, 0x0b, 0x02, 0x1c, 0x02, 0x01, 0x02, 0x05,
        0x62, 0x65, 0x67, 0x69, 0x6e, 0x00, 0x00, 0x04, 0x64, 0x72, 0x6f, 0x70, 0x00, 0x01, 0x00,
        0x00, 0x01, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x12, 0x00, 0x07, 0x05, 0x01, 0x40, 0x00, 0x00,
        0x79, 0x06, 0x0d, 0x01, 0x00, 0x00, 0x01, 0x01, 0x07, 0x72, 0x65, 0x65, 0x6e, 0x74, 0x65,
        0x72, 0x08, 0x06, 0x01, 0x00, 0x00, 0x02, 0x00, 0x02, 0x07, 0x05, 0x01, 0x40, 0x00, 0x00,
        0x79, 0x06, 0x0d, 0x01, 0x00, 0x00, 0x01, 0x01, 0x07, 0x72, 0x65, 0x6c, 0x65, 0x61, 0x73,
        0x65, 0x08, 0x06, 0x01, 0x00, 0x00, 0x03, 0x00, 0x03, 0x0b, 0x19, 0x02, 0x00, 0x07, 0x72,
        0x65, 0x65, 0x6e, 0x74, 0x65, 0x72, 0x01, 0x01, 0x00, 0x00, 0x07, 0x72, 0x65, 0x6c, 0x65,
        0x61, 0x73, 0x65, 0x01, 0x02, 0x00,
    ];

    async fn mutation_probe(export: &str, reentrant: bool) {
        let engine = crate::engine().unwrap();
        let component = wasmtime::component::Component::new(&engine, MUTATION_PROBE).unwrap();
        let gate = Arc::new(TestMutationGate {
            gate: Arc::new(tokio::sync::Mutex::new(())),
            begins: std::sync::atomic::AtomicUsize::new(0),
        });
        let services = Services {
            workspace_mutation: Some(gate.clone()),
            ..Services::default()
        };
        let linker =
            capability_linker(&engine, &granted(&["workspace-mutation"]), &services).unwrap();
        let mut store = crate::executor::module_store(
            &engine,
            CallState::new(CancellationToken::new(), &services),
        );
        store.set_fuel(crate::executor::DEFAULT_FUEL).unwrap();
        store.set_epoch_deadline(1);
        let instance = linker
            .instantiate_async(&mut store, &component)
            .await
            .unwrap();
        let func = instance.get_func(&mut store, export).unwrap();
        let mut results = [Val::U32(0)];
        // No engine epoch ticks: only the reentrancy trap can end a blocked second
        // begin. This timeout is a deadlock guard, not the module's deadline.
        let outcome = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            func.call_async(&mut store, &[], &mut results),
        )
        .await
        .expect("mutation probe deadlocked instead of returning");
        if reentrant {
            let error = outcome.expect_err("second begin while held must trap");
            assert!(
                format!("{error:#}").contains(
                    "workspace-mutation.begin called while this call already holds a mutation"
                ),
                "{error:#}"
            );
            assert_eq!(gate.begins.load(std::sync::atomic::Ordering::SeqCst), 1);
            assert_eq!(store.data().mutations_held, 1);
            assert!(gate.gate.try_lock().is_err());
        } else {
            outcome.expect("explicit drop must allow another begin");
            assert_eq!(results, [Val::U32(2)]);
            assert_eq!(gate.begins.load(std::sync::atomic::Ordering::SeqCst), 2);
            assert_eq!(store.data().mutations_held, 0);
            assert!(
                gate.gate.try_lock().is_ok(),
                "resource drop did not release gate"
            );
        }
        drop(store);
        assert!(
            gate.gate.try_lock().is_ok(),
            "call teardown did not release gate"
        );
    }

    #[tokio::test]
    async fn a_second_mutation_begin_traps_before_waiting_on_its_own_gate() {
        mutation_probe("reenter", true).await;
    }

    #[tokio::test]
    async fn an_explicit_mutation_drop_releases_the_gate_and_allows_another_begin() {
        mutation_probe("release", false).await;
    }

    /// A call-scoped part is built at the start of every call and serves that call alone
    /// (ADR-0092); what the scope does not return is the assembly's, shared as before.
    #[test]
    fn every_call_gets_its_own_call_scoped_services() {
        struct NoGate;

        impl MutationService for NoGate {
            fn begin(&self) -> BoxFuture<'_, Box<dyn HeldMutation>> {
                Box::pin(async { cancelled_begin() })
            }
        }

        let built = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = built.clone();
        let mut services = Services::call_scoped(move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Services {
                workspace_mutation: Some(Arc::new(NoGate)),
                ..Services::default()
            }
        });
        let shared: Arc<dyn WorkspaceService> = Arc::new(NoFiles);
        services.workspace = Some(shared.clone());
        assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 1);

        let one = CallState::new(CancellationToken::new(), &services);
        let two = CallState::new(CancellationToken::new(), &services);
        assert_eq!(built.load(std::sync::atomic::Ordering::SeqCst), 3);
        let (Some(one_gate), Some(two_gate), Some(assembly_gate)) = (
            &one.workspace_mutation,
            &two.workspace_mutation,
            &services.workspace_mutation,
        ) else {
            panic!("every call is served a mutation");
        };
        assert!(!Arc::ptr_eq(one_gate, two_gate), "one per call");
        assert!(
            !Arc::ptr_eq(one_gate, assembly_gate),
            "never the assembly's"
        );
        let (Some(one_files), Some(two_files)) = (&one.workspace, &two.workspace) else {
            panic!("the assembly's workspace serves every call");
        };
        assert!(Arc::ptr_eq(one_files, &shared) && Arc::ptr_eq(two_files, &shared));
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
    fn a_search_query_is_read_by_field_name_and_its_result_is_the_wit_record() {
        let some = |text: &str| Val::Option(Some(Box::new(Val::String(text.to_owned()))));
        let record = Val::Record(vec![
            ("pattern".to_owned(), Val::String("beta".to_owned())),
            ("path".to_owned(), some("src")),
            ("glob".to_owned(), Val::Option(None)),
            ("case-insensitive".to_owned(), Val::Bool(true)),
            ("context".to_owned(), Val::U32(1)),
            ("max-lines".to_owned(), Val::U32(10)),
        ]);
        assert_eq!(
            search_query(&record).unwrap(),
            SearchQuery {
                pattern: "beta".to_owned(),
                path: Some("src".to_owned()),
                glob: None,
                case_insensitive: true,
                context: 1,
                max_lines: 10,
            }
        );
        assert!(search_query(&Val::Record(vec![])).is_err());
        assert!(option_string(Some(&Val::U32(1)), "f", "glob").is_err());

        let result = SearchResult {
            files: vec![FileMatches {
                path: "a.rs".to_owned(),
                lines: vec![SearchLine {
                    line_number: 2,
                    text: "fn beta() {}".to_owned(),
                    is_match: true,
                }],
            }],
            truncated: false,
            omitted_files: 3,
        };
        assert_eq!(
            search_result_val(result),
            Some(Val::Record(vec![
                (
                    "files".to_owned(),
                    Val::List(vec![Val::Record(vec![
                        ("path".to_owned(), Val::String("a.rs".to_owned())),
                        (
                            "lines".to_owned(),
                            Val::List(vec![Val::Record(vec![
                                ("line-number".to_owned(), Val::U64(2)),
                                ("text".to_owned(), Val::String("fn beta() {}".to_owned())),
                                ("is-match".to_owned(), Val::Bool(true)),
                            ])])
                        ),
                    ])])
                ),
                ("truncated".to_owned(), Val::Bool(false)),
                ("omitted-files".to_owned(), Val::U64(3)),
            ]))
        );
    }

    #[tokio::test]
    async fn a_workspace_service_without_the_walk_refuses_list_files_and_search() {
        let files = NoFiles;
        assert_eq!(
            files.list_files(".".to_owned(), None).await,
            Err(FsError::Io(LIST_FILES_NOT_GRANTED.to_owned()))
        );
        let query = SearchQuery {
            pattern: "x".to_owned(),
            path: None,
            glob: None,
            case_insensitive: false,
            context: 0,
            max_lines: 1,
        };
        assert_eq!(
            files.search(query).await,
            Err(FsError::Io(SEARCH_NOT_GRANTED.to_owned()))
        );
    }

    #[test]
    fn unlinked_interface_is_not_described_as_loader_rejection() {
        let engine = crate::engine().unwrap();
        let error = capability_linker(&engine, &granted(&["notices"]), &Services::default())
            .err()
            .expect("notices has no linker");
        assert!(error.to_string().contains("no linker"));
    }

    #[test]
    fn malformed_dynamic_host_signature_is_a_error_not_a_panic() {
        assert!(check_arity("test", &[], &[], 0, 1).is_err());
        assert!(check_arity("test", &[], &[Val::Bool(false)], 1, 1).is_err());
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
