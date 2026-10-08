//! Optional delegation: the typed worker service and its in-process implementation.
//!
//! `p1-tool-delegate` depends on this crate's [`WorkerService`] trait only. Nothing
//! here implements a model-facing tool, a prompt template, a provider or a UI;
//! completion notifications name the delegate tool's `worker_result` face.
//!
//! Lifecycle: every child is one tokio task that OWNS its `Agent` for the child's
//! whole life, so turns can never overlap and there is no `Mutex<Agent>` anywhere.
//! Completion is retained state plus ONE parent notification: the status is stored
//! first, then the parent is told. A missed or ignored notification loses nothing —
//! the result stays retrievable by id for the service's lifetime.
//!
//! [`scope`] adds the scoped surface a component links (the WIT worker interfaces) over
//! any [`WorkerService`], without changing the service. [`journal`] reads back which
//! workers a journalled session started.

pub mod journal;
pub mod scope;
pub mod subagents;

pub use scope::{
    ScopeKey, WorkerScope, WorkerScopes, WorkersControl, WorkersObserve, WorkersStart,
};

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use p1_contracts::{BoxFuture, CancellationToken, InboxKind, Item, TurnEnd, Usage};
use p1_core::{Agent, Inbox, Reconfiguration};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

/// Identifier of one child within one service: `w1`, `w2`, … never reused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChildId(pub String);

/// The state a child is in. `Finished` carries the retained result; `Cancelled`
/// and `Failed` are terminal until `continue_child` starts another turn.
///
/// `Finished` is much larger than the other variants because a finished turn carries
/// its text, its end AND its report (ADR-0050 item 6). The enum is moved a handful of
/// times per turn, so an indirection would buy nothing and cost every reader a deref.
#[allow(clippy::large_enum_variant)]
#[derive(Debug, Clone, PartialEq)]
pub enum ChildStatus {
    Running,
    Finished(ChildResult),
    Cancelled,
    Failed(String),
}

/// A retained completed turn of a child.
#[derive(Debug, Clone, PartialEq)]
pub struct ChildResult {
    /// The text of the child's LAST assistant item, empty if it produced none.
    pub final_text: String,
    pub turn_end: TurnEnd,
    /// Always `None` in this slice: summing usage needs an event tap the factory
    /// contract does not carry yet. Unknown, never zero.
    pub usage_total: Option<Usage>,
    /// What the child's own tap recorded for this turn (ADR-0050 item 6), so a
    /// short-handed worker is visible without the parent's cooperation.
    pub report: WorkerReport,
}

/// What one worker says about itself (ADR-0050 item 6): the tools it was assembled
/// with, the `finish` it reported, and every call it made to a tool it did NOT have.
///
/// The host builds it from the child's event stream (its tap shares the cell with
/// [`ChildAgent::report`]); a factory that has no tap returns
/// [`WorkerReport::default`]. `tools` describes the worker for its whole life;
/// `finish` and `missing_tool_calls` describe ONE turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerReport {
    /// The model-facing names of the tools the worker was assembled with, in
    /// assembly order. Never changes between turns.
    pub tools: Vec<String>,
    /// The worker's LAST successful `finish` call this turn; `None` if it made none.
    pub finish: Option<FinishReport>,
    /// Every tool name the worker called that it did not have, with how many times,
    /// in first-seen order.
    pub missing_tool_calls: Vec<(String, u32)>,
}

/// One accepted `finish` call, as its input described it and as the host's own
/// evidence labelled it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct FinishReport {
    pub status: String,
    /// `needs` as the worker wrote it: a string, or an array joined with `", "`.
    pub needs: Option<String>,
    pub summary: Option<String>,
    /// What the ACCEPTED outcome established (ADR-0051 item 3), never parsed out of
    /// the model's input: `commands passed: …` or
    /// `not verified; parent verification required`. `None` when the outcome carried
    /// none (a report the host built without an outcome, or a `blocked` finish).
    pub evidence: Option<String>,
}

impl WorkerReport {
    /// The report of a worker assembled with these model-facing tool names. The rest
    /// starts empty; the tap fills it as the turn runs.
    pub fn new(tools: Vec<String>) -> Self {
        Self {
            tools,
            finish: None,
            missing_tool_calls: Vec::new(),
        }
    }

    /// Start a fresh turn: a continue is a new turn, so `finish` and the
    /// missing-tool calls go; the granted tools stay.
    pub fn reset_turn(&mut self) {
        self.finish = None;
        self.missing_tool_calls.clear();
    }

    /// One call to a tool this worker was not given, counted under its name.
    pub fn note_missing_tool(&mut self, name: &str) {
        match self
            .missing_tool_calls
            .iter_mut()
            .find(|(seen, _)| seen == name)
        {
            Some((_, count)) => *count += 1,
            None => self.missing_tool_calls.push((name.to_string(), 1)),
        }
    }
}

/// What the parent asks a worker service to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Environment name understood by the host's factory.
    pub environment: String,
    /// The task text. It is the ONLY thing the child receives; the parent's
    /// transcript is never forwarded.
    pub task: String,
    /// The tool MODULE names the parent granted the worker, in the parent's order
    /// and without duplicates. Never `finish` (the factory adds it to every worker,
    /// last) and never a `worker_*` module: the worker tools are not grantable, so a
    /// child can never start workers of its own.
    pub tools: Vec<String>,
    /// Workspace override, if the host supports one.
    pub workspace: Option<PathBuf>,
    /// Resolved subagent defaults and per-call overrides. Legacy starts use defaults.
    pub options: subagents::ChildOptions,
}

/// What the service needs to know about a child BEFORE its agent exists (ADR-0053 item
/// 2): the closure that builds the agent runs only once a running slot is reserved and the
/// id is allocated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PreparedStart {
    /// The first turn's message: the task text, the only thing the child receives.
    pub task: String,
    /// The tool MODULE names granted, exactly as `ChildSpec::tools` (never `finish`,
    /// never a `worker_*` module); stored as the child's grant for a later re-grant.
    pub tools: Vec<String>,
    /// Whether each ended turn sends the parent its completion notification. A
    /// workflow step's turns end inside a run whose OWN end is the one notification
    /// the parent gets (ADR-0053 item 7), so the host starts steps with `false`.
    pub notify_parent: bool,
}

impl Default for PreparedStart {
    fn default() -> Self {
        Self {
            task: String::new(),
            tools: Vec::new(),
            notify_parent: true,
        }
    }
}

/// Every way a worker service call can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkerError {
    #[error("no such worker")]
    UnknownChild,
    #[error("the worker is still running a turn")]
    Busy,
    #[error("at most {max} workers may run at once")]
    LimitReached { max: usize },
    #[error("invalid child environment: {0}")]
    InvalidEnvironment(String),
    /// The service's id namespace holds no unallocated id: the last one handed out was
    /// `w<usize::MAX>`. Stable, so a caller can report it without panicking or wrapping.
    #[error("the worker id namespace is exhausted: no id can be allocated")]
    IdsExhausted,
    #[error("the worker's tools were not changed: {0}")]
    Regrant(String),
    #[error("the worker service has shut down")]
    ShutDown,
}

/// A built child: the agent, the route/model description shown to the parent
/// (e.g. `openai-codex-responses/gpt-5.6-sol`), how to read its report, and how to
/// re-assemble it with a larger tool grant.
pub struct ChildAgent {
    pub agent: Agent,
    pub description: String,
    /// The child's report AS OF NOW. The host's tap and this closure share one cell,
    /// and the service snapshots it when a turn ends; a factory with no tap (a test)
    /// returns [`WorkerReport::default`].
    pub report: Arc<dyn ChildReport>,
    /// Given the child's FULL new tool grant (the current modules plus the added
    /// ones), the [`Reconfiguration`] that assembles it with exactly those — or the
    /// reason it cannot (ADR-0050 item 6). `None` when the factory cannot re-assemble
    /// a child it already built (a test factory, `p1 env show`): the service then
    /// refuses an `add_tools` continue with [`WorkerError::Regrant`] and keeps the
    /// child's tools as they are.
    pub regrant: Option<Regrant>,
    /// Host-selected next model after a provider failure. None at chain exhaustion.
    pub fallback: Option<ModelFallback>,
}

/// Host-owned work settles before a turn's result becomes visible. Ordinary report
/// closures have nothing asynchronous to settle and remain valid factories.
pub trait ChildReport: Send + Sync {
    fn snapshot(&self) -> WorkerReport;
    fn turn_ended(&self) -> BoxFuture<'_, Vec<String>> {
        Box::pin(async { Vec::new() })
    }
}
impl<F: Fn() -> WorkerReport + Send + Sync> ChildReport for F {
    fn snapshot(&self) -> WorkerReport {
        self()
    }
}

/// How a factory re-assembles one of its children under a larger grant: it is given
/// the granted tool MODULE names in order and returns everything a running agent can
/// be switched to between turns (ADR-0049), plus what the factory commits once the
/// switch is in force.
pub type Regrant = Arc<dyn Fn(&[String]) -> Result<Regranted, String> + Send + Sync>;

pub type ModelFallback = Arc<dyn Fn(&[String]) -> Option<Result<Regranted, String>> + Send + Sync>;

/// One re-assembly a [`Regrant`] built. Building it must change nothing the running
/// child depends on: the agent may still refuse the reconfiguration (its history, its
/// journal), and a refused re-grant leaves the worker exactly as it was.
pub struct Regranted {
    pub reconfiguration: Reconfiguration,
    /// Run by the child task once `Agent::reconfigure` has installed
    /// `reconfiguration`, before the new turn starts, and never when it was refused:
    /// what the factory may only commit once the new tools are really the agent's (the
    /// host activates the staged `finish` grant and re-points its report taps).
    pub installed: Box<dyn FnOnce() + Send>,
}

impl Regranted {
    /// A re-assembly with nothing to commit after it is installed.
    pub fn new(reconfiguration: Reconfiguration) -> Self {
        Self {
            reconfiguration,
            installed: Box::new(|| {}),
        }
    }
}

/// Builds a child `Agent` from its spec. Injected by the host and the SAME
/// assembly path a top-level agent uses, so a child on another route gets that
/// route's prompt and tools and nothing of the parent's.
///
/// The factory assembles a child with exactly the modules in
/// [`ChildSpec::tools`] plus `finish`. The worker tools are not grantable, so a
/// child can never start workers (no recursion) however the parent asks.
pub type AgentFactory = Arc<dyn Fn(&ChildSpec) -> Result<ChildAgent, String> + Send + Sync>;

/// The typed worker API a delegation tool depends on.
pub trait WorkerService: Send + Sync {
    /// Starts NOW (not when someone polls). Fails if the environment is unknown
    /// or invalid, or the concurrency bound is reached (never queues).
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>>;

    /// Starts for a caller that must record the id before any await after the child
    /// exists. The caller records it, then gives the child its first turn. Services
    /// whose `start` suspends after creating a child must override this method with
    /// a path that returns the id without suspending after creation.
    fn start_recorded_first<'a>(
        &'a self,
        spec: ChildSpec,
    ) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        self.start(spec)
    }

    /// The current status. Never blocks on a running turn.
    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;

    /// Resolves with the status as soon as the child is not `Running` (immediately
    /// if so already). If `cancel` fires first, resolves `Ok(Running)`.
    /// Cancel-safe and repeatable; a wake-up is never lost.
    fn wait<'a>(
        &'a self,
        id: &'a ChildId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;

    /// Cancels the running turn. Idempotent; a finished child is a no-op `Ok`.
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>>;

    /// Another turn in the SAME child session (repair), with the tool modules in
    /// `add_tools` granted for it and every later turn (ADR-0050 item 6). `Busy`
    /// while a turn runs. An empty `add_tools` is today's behaviour exactly: no
    /// re-assembly, the current tools kept. With added modules the child's grant
    /// becomes grant ∪ added (existing order, then the new ones, no duplicates), its
    /// factory is asked to re-assemble it with that grant, and the agent is
    /// reconfigured BEFORE the new turn — so the repair keeps the worker's context.
    /// Any failure (no re-grant path, the factory's error, the agent's refusal) is
    /// [`WorkerError::Regrant`]: the turn does NOT run and the worker keeps its old
    /// tools. The outcome is settled by the child, not by this call: a caller that goes
    /// away while it waits only misses hearing it.
    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>>;

    /// Every child this service has ever started, with its retained status.
    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>>;

    /// The route/model description of a child, as the factory built it. This is an
    /// extension of the spec trait: `ChildAgent::description` exists to be shown to
    /// the parent, and `start` returns only the id.
    fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>>;

    /// The model-facing name of the tool that reads a result, so the completion
    /// notification can point the parent at it (ADR-0057). The delegation tool
    /// passes its own face's name in; a service with no delegate tool keeps the
    /// default and names no tool. Not a worker operation: it is the ONE string this
    /// crate would otherwise have to hard-code.
    fn set_result_tool_name(&self, _name: &str) {}
}

/// The in-process [`WorkerService`]. One tokio task per child owns that child's
/// `Agent`; the service only holds handles and retained status.
pub struct InProcessWorkers {
    shared: Arc<Shared>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

struct Shared {
    factory: AgentFactory,
    max_concurrent: usize,
    /// Set after construction: the delegate tools must exist before the parent
    /// `Agent` is built, and the parent's `Inbox` only exists after `Agent::new`.
    parent_inbox: Mutex<Option<Inbox>>,
    /// The model-facing name of the result tool, set by the delegation tool that
    /// owns it (ADR-0057). `None` until then: this crate names no tool.
    result_tool_name: Mutex<Option<String>>,
    state: Mutex<State>,
    /// Cancelled by `shutdown` (and best-effort `Drop`); every child task races
    /// this against its next command.
    shutdown: CancellationToken,
    /// Wakes `wait_for_capacity`. A slot is *observed* free, never reserved, so the
    /// only alternative to this notification is polling.
    capacity: Notify,
}

struct State {
    children: BTreeMap<String, ChildEntry>,
    next_id: usize,
    shut_down: bool,
}

struct ChildEntry {
    /// Retained status; `send_replace` also wakes every `wait`er.
    status: watch::Sender<ChildStatus>,
    commands: mpsc::UnboundedSender<ChildCommand>,
    /// The token of the child's CURRENT turn. Whoever moves the child to
    /// `Running` (`start`, `continue_child`) installs the new turn's token in the
    /// same critical section, so a cancel can never land on a finished turn's
    /// token while the next turn is accepted but not yet polled.
    turn_cancel: Arc<Mutex<CancellationToken>>,
    /// Set by [`InProcessWorkers::stall_child`] while a turn runs: the reason that
    /// turn was stopped by the host's own guard, so the child ends `Failed` with
    /// that sentence instead of `Cancelled` (completion.md §3c).
    stall: Arc<Mutex<Option<String>>>,
    /// The worker's CURRENT tool grant: the module names it was started with, plus
    /// every module a later `continue_child` added. Never `finish` (the factory adds
    /// it to every assembly) and never a `worker_*` module.
    grant: Vec<String>,
    /// How to re-assemble this child under a larger grant; `None` when its factory
    /// cannot ([`ChildAgent::regrant`]).
    regrant: Option<Regrant>,
    description: String,
}

enum ChildCommand {
    /// Another turn, with the cancellation token installed when it was accepted. A
    /// continue that adds tools carries the re-grant the CHILD TASK must apply (it owns
    /// the `Agent`) and the channel its outcome is reported on, so the turn runs only
    /// after a successful reconfiguration (ADR-0050 item 6).
    Continue {
        message: String,
        token: CancellationToken,
        // Boxed: the command queue should not size every `Shutdown` after it.
        regrant: Option<Box<PendingRegrant>>,
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
    Shutdown,
}

/// A re-grant accepted by `continue_child` and finished by the child task: the task, not
/// the awaiting caller, stores the grant once the reconfiguration is in force or puts the
/// status back when it is refused, so a caller that goes away changes neither outcome.
struct PendingRegrant {
    regranted: Regranted,
    /// The grant to store once the reconfiguration is installed.
    grant: Vec<String>,
    /// The status the child had before the continue made it `Running`, restored when the
    /// reconfiguration is refused.
    previous: ChildStatus,
}

impl InProcessWorkers {
    /// Build the service. `max_concurrent` bounds RUNNING children; `start`
    /// beyond it is an error the model can read, not a queue.
    pub fn new(factory: AgentFactory, max_concurrent: usize) -> Arc<Self> {
        Arc::new(Self {
            shared: Arc::new(Shared {
                factory,
                max_concurrent,
                parent_inbox: Mutex::new(None),
                result_tool_name: Mutex::new(None),
                state: Mutex::new(State {
                    children: BTreeMap::new(),
                    next_id: 0,
                    shut_down: false,
                }),
                shutdown: CancellationToken::new(),
                capacity: Notify::new(),
            }),
            tasks: Mutex::new(Vec::new()),
        })
    }

    /// Never hand out the first `used` ids (`w1`…`w<used>`). A resumed session's
    /// history already talks about the workers of the process that ended; a new
    /// worker must not answer to one of their names.
    pub fn reserve_ids(&self, used: usize) {
        let mut state = self.shared.state.lock().unwrap();
        state.next_id = state.next_id.max(used);
    }

    /// Where completion notifications go. Called once the parent agent exists.
    /// A completion with no parent inbox set is still retained.
    pub fn set_parent_inbox(&self, inbox: Inbox) {
        *self.shared.parent_inbox.lock().unwrap() = Some(inbox);
    }

    /// The model-facing name of the result tool, as the delegation tool set it
    /// (ADR-0057). `None` when no delegate tool is assembled.
    pub fn result_tool_name(&self) -> Option<String> {
        self.shared.result_tool_name.lock().unwrap().clone()
    }

    /// Stop one child's RUNNING turn with a terminal `Failed(message)`, instead of
    /// the `Cancelled` any other cancel produces. The child's turn token lives here,
    /// in the service — the child factory that builds the agent cannot reach it —
    /// so a host guard that stops a child of its own ([completion.md §3c]) calls in
    /// here. Idempotent, and harmless for a turn that already ended.
    pub fn stall_child(&self, id: &ChildId, message: String) -> Result<(), WorkerError> {
        if self.shared.is_shut_down() {
            return Err(WorkerError::ShutDown);
        }
        let (turn_cancel, stall) = {
            let state = self.shared.state.lock().unwrap();
            let Some(entry) = state.children.get(&id.0) else {
                return Err(WorkerError::UnknownChild);
            };
            (Arc::clone(&entry.turn_cancel), Arc::clone(&entry.stall))
        };
        // The reason is stored BEFORE the cancel, so the task waking on the
        // cancelled token always reads it.
        *stall.lock().unwrap() = Some(message);
        turn_cancel.lock().unwrap().cancel();
        Ok(())
    }

    /// Prepared start: the child is built only once it can actually run (ADR-0053 item
    /// 2). A workflow step is a worker whose environment the SERVICE has already
    /// allocated, so the caller must not have to guess the next `w<N>` — and a step that
    /// cannot run must never be built.
    ///
    /// Order, under the state lock: refuse when shut down, reserve a running slot
    /// (`LimitReached` BEFORE `build`), allocate the next id, then call `build` with that
    /// id. `build` runs synchronously under the lock — as `start`'s factory always did —
    /// which is what makes the RUNNING count and the id assignment atomic under
    /// concurrent starts. A failed `build` becomes [`WorkerError::InvalidEnvironment`]
    /// and consumes NOTHING: not the id, not the slot.
    pub async fn start_prepared(
        &self,
        prepared: PreparedStart,
        build: impl FnOnce(&ChildId) -> Result<ChildAgent, String> + Send,
    ) -> Result<ChildId, WorkerError> {
        let PreparedStart {
            task,
            tools,
            notify_parent,
        } = prepared;
        // Release the state lock before spawning the child; no lock reaches the caller
        // that records the id or yields to give the child its first turn.
        let (id, status, command_rx, token, stall, agent, report, fallback) = {
            let mut state = self.shared.state.lock().unwrap();
            if state.shut_down || self.shared.shutdown.is_cancelled() {
                return Err(WorkerError::ShutDown);
            }
            // Count RUNNING children before building: a rejected start must not build
            // (and then discard) a child it cannot run.
            self.shared.reserve_running_slot(&state)?;
            // The id the build is handed is the id this call returns, and it is claimed
            // only once the child exists: the counter moves after a successful build, so
            // the next start is offered this same id. `next_id` is the last id handed
            // out, so the next is ONE more — and past `usize::MAX` there is none: a
            // checked addition refuses the start (a stable error), never wraps.
            let next = state
                .next_id
                .checked_add(1)
                .ok_or(WorkerError::IdsExhausted)?;
            let id = format!("w{next}");
            // Build failure is an invalid environment, not a service fault.
            let ChildAgent {
                agent,
                description,
                report,
                regrant,
                fallback,
            } = build(&ChildId(id.clone())).map_err(WorkerError::InvalidEnvironment)?;
            state.next_id = next;

            let (status, _) = watch::channel(ChildStatus::Running);
            let (commands, command_rx) = mpsc::unbounded_channel();
            let token = CancellationToken::new();
            let stall = Arc::new(Mutex::new(None));
            state.children.insert(
                id.clone(),
                ChildEntry {
                    status: status.clone(),
                    commands,
                    turn_cancel: Arc::new(Mutex::new(token.clone())),
                    stall: Arc::clone(&stall),
                    // The grant the child was built with, kept as `continue_child`
                    // grows it.
                    grant: tools,
                    regrant,
                    description,
                },
            );
            (
                id, status, command_rx, token, stall, agent, report, fallback,
            )
        };

        let task_id = id.clone();
        let shared = Arc::clone(&self.shared);
        let handle = tokio::spawn(run_child(
            shared,
            ChildTask {
                id: task_id,
                agent,
                task,
                token,
                commands: command_rx,
                stall,
                status,
                report,
                fallback,
                notify_parent,
            },
        ));
        self.tasks.lock().unwrap().push(handle);
        // No await between the child existing and the id reaching the caller: a caller
        // that records the id (a `WorkerScope`) cannot be dropped in between, so no
        // child ever runs outside the scope that started it. The child is spawned, so
        // it starts as soon as the runtime gets to it; a caller that wants it to have
        // had a turn yields once it has recorded the id.
        Ok(ChildId(id))
    }

    /// Wait until a child COULD be started (ADR-0053 item 2): `Ok(true)` as soon as
    /// fewer than `max_concurrent` children are `Running`, `Ok(false)` when `cancel`
    /// fires first, `Err(ShutDown)` once the service has shut down — also when it shuts
    /// down while this call waits.
    ///
    /// A slot observed free here is NOT reserved: a concurrent start can still take it.
    /// The caller therefore loops `wait_for_capacity` → [`InProcessWorkers::start_prepared`]
    /// and treats `LimitReached` as "wait again". Capacity is the same count direct
    /// `worker_start` calls use.
    pub async fn wait_for_capacity(&self, cancel: CancellationToken) -> Result<bool, WorkerError> {
        if self.shared.is_shut_down() {
            return Err(WorkerError::ShutDown);
        }
        loop {
            // Subscribe BEFORE reading the count: a turn that ends between the two wakes
            // every waiter registered at that moment, and this one already is — so no
            // wake-up is lost. (`notify_waiters` stores no permit: the loop registers a
            // fresh waiter after every wake for the same reason.)
            let notified = self.shared.capacity.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            {
                let state = self.shared.state.lock().unwrap();
                if state.shut_down {
                    return Err(WorkerError::ShutDown);
                }
                if self.shared.running_count(&state) < self.shared.max_concurrent {
                    return Ok(true);
                }
            }
            tokio::select! {
                biased;
                _ = cancel.cancelled() => return Ok(false),
                _ = &mut notified => {}
            }
        }
    }

    /// Children `Running` now. A report, not a lease: a caller that starts on the
    /// strength of this reading can still lose the slot to a concurrent start.
    pub fn running(&self) -> usize {
        let state = self.shared.state.lock().unwrap();
        self.shared.running_count(&state)
    }

    /// The bound the service was built with; shared by every way of starting a child.
    pub fn max_concurrent(&self) -> usize {
        self.shared.max_concurrent
    }

    /// Cancel every running child, then join every child task. Afterwards every
    /// fallible call returns [`WorkerError::ShutDown`].
    pub async fn shutdown(&self) {
        // Cancel and wake the tasks. No lock is held across the joins below.
        {
            let mut state = self.shared.state.lock().unwrap();
            state.shut_down = true;
            for entry in state.children.values() {
                entry.turn_cancel.lock().unwrap().cancel();
                let _ = entry.commands.send(ChildCommand::Shutdown);
            }
        }
        self.shared.shutdown.cancel();
        // Wake the capacity waiters with it: a service that is shutting down has no
        // slot to offer, and a waiter must not be left waiting on its own cancel token.
        self.shared.capacity.notify_waiters();
        // Take the handles out first: awaiting must not hold the tasks lock.
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock().unwrap());
        for handle in tasks {
            let _ = handle.await;
        }
    }
}

impl Shared {
    /// Children `Running` right now. Called with the state lock held: the count is only
    /// meaningful as part of the same critical section that changes a status.
    fn running_count(&self, state: &State) -> usize {
        state
            .children
            .values()
            .filter(|entry| matches!(&*entry.status.borrow(), ChildStatus::Running))
            .count()
    }

    /// `Err(LimitReached)` when `max_concurrent` children are already Running.
    /// Called with the state lock held, by everything that sets a child Running.
    /// A turn ending concurrently only lowers the count, so the check is safe.
    fn reserve_running_slot(&self, state: &State) -> Result<(), WorkerError> {
        let running = self.running_count(state);
        if running >= self.max_concurrent {
            return Err(WorkerError::LimitReached {
                max: self.max_concurrent,
            });
        }
        Ok(())
    }

    fn is_shut_down(&self) -> bool {
        self.shutdown.is_cancelled() || self.state.lock().unwrap().shut_down
    }
}

impl Drop for InProcessWorkers {
    fn drop(&mut self) {
        // Best-effort: `Drop` cannot await the joins, so cancel everything and let
        // the runtime reap the tasks. A task always stores its final status before
        // it observes shutdown, so no child is left silently `Running`.
        self.shared.shutdown.cancel();
        if let Ok(state) = self.shared.state.lock() {
            for entry in state.children.values() {
                entry.turn_cancel.lock().unwrap().cancel();
            }
        }
    }
}

impl WorkerService for InProcessWorkers {
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        Box::pin(async move {
            let id = self.start_recorded_first(spec).await?;
            // Direct callers expect the fresh child to have had its first turn.
            // Scopes use the non-yielding path instead so they can record its id first.
            tokio::task::yield_now().await;
            Ok(id)
        })
    }

    fn start_recorded_first<'a>(
        &'a self,
        spec: ChildSpec,
    ) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        Box::pin(async move {
            // The general seam specialised to the host's factory: everything
            // else — the slot check before the build, the id allocation, the spawn, the
            // error mapping, the retained grant — lives in `start_prepared`.
            let prepared = PreparedStart {
                task: spec.task.clone(),
                tools: spec.tools.clone(),
                notify_parent: spec.options.background,
            };
            let factory = Arc::clone(&self.shared.factory);
            self.start_prepared(prepared, move |_id| factory(&spec))
                .await
        })
    }

    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move {
            if self.shared.is_shut_down() {
                return Err(WorkerError::ShutDown);
            }
            let state = self.shared.state.lock().unwrap();
            let Some(entry) = state.children.get(&id.0) else {
                return Err(WorkerError::UnknownChild);
            };
            // Reading the watch never waits on the running turn.
            Ok(entry.status.borrow().clone())
        })
    }

    fn wait<'a>(
        &'a self,
        id: &'a ChildId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move {
            if self.shared.is_shut_down() {
                return Err(WorkerError::ShutDown);
            }
            // Subscribe BEFORE reading the status: a completion landing between
            // the two marks the watch already-changed, so no wake-up is lost.
            let mut rx = {
                let state = self.shared.state.lock().unwrap();
                let Some(entry) = state.children.get(&id.0) else {
                    return Err(WorkerError::UnknownChild);
                };
                entry.status.subscribe()
            };
            loop {
                let current = rx.borrow_and_update().clone();
                if !matches!(current, ChildStatus::Running) {
                    return Ok(current);
                }
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Ok(ChildStatus::Running),
                    changed = rx.changed() => {
                        if changed.is_err() {
                            // The entry keeps the sender alive for the service's
                            // lifetime, so this is unreachable while it lives.
                            return Ok(rx.borrow().clone());
                        }
                    }
                }
            }
        })
    }

    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            if self.shared.is_shut_down() {
                return Err(WorkerError::ShutDown);
            }
            let turn_cancel = {
                let state = self.shared.state.lock().unwrap();
                let Some(entry) = state.children.get(&id.0) else {
                    return Err(WorkerError::UnknownChild);
                };
                Arc::clone(&entry.turn_cancel)
            };
            // Cancelling a finished turn's token is a harmless no-op.
            turn_cancel.lock().unwrap().cancel();
            Ok(())
        })
    }

    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            if self.shared.is_shut_down() {
                return Err(WorkerError::ShutDown);
            }
            // Every transition into Running happens under the state lock: the limit
            // check, the status change and the new turn's cancellation token are one
            // step, so neither a concurrent `start`/`continue_child` nor a `cancel`
            // or `shutdown` can slip between them. No await is held here.
            //
            // A continue that adds tools re-assembles the child FIRST (ADR-0050 item
            // 6): the reconfiguration is built here — the factory closure runs under
            // the state lock, exactly as `start` runs it — and the child task applies
            // it before its new turn. Every failure leaves the turn unrun and the
            // worker's tools unchanged. The child task also finishes the bookkeeping
            // (the stored grant, or the status put back): this call only waits for the
            // outcome, so dropping it changes nothing but who hears of it.
            let reply = {
                let mut state = self.shared.state.lock().unwrap();
                if state.shut_down {
                    return Err(WorkerError::ShutDown);
                }
                let Some(entry) = state.children.get(&id.0) else {
                    return Err(WorkerError::UnknownChild);
                };
                if matches!(&*entry.status.borrow(), ChildStatus::Running) {
                    return Err(WorkerError::Busy);
                }
                self.shared.reserve_running_slot(&state)?;
                // The new grant: the child's current modules in their order, then the
                // added ones that are not already there. An empty `add_tools` keeps
                // today's continue exactly — no re-assembly at all.
                let mut grant = entry.grant.clone();
                for module in &add_tools {
                    if !grant.contains(module) {
                        grant.push(module.clone());
                    }
                }
                let previous = entry.status.borrow().clone();
                let regrant = if add_tools.is_empty() {
                    None
                } else {
                    let Some(regrant) = &entry.regrant else {
                        return Err(WorkerError::Regrant(format!(
                            "worker {} was not started by a factory that can re-assemble it",
                            id.0
                        )));
                    };
                    let regranted = regrant(&grant).map_err(WorkerError::Regrant)?;
                    Some(Box::new(PendingRegrant {
                        regranted,
                        grant,
                        previous: previous.clone(),
                    }))
                };
                let (reply, reply_tx) = if regrant.is_some() {
                    let (tx, rx) = oneshot::channel();
                    (Some(rx), Some(tx))
                } else {
                    (None, None)
                };
                let entry = state.children.get_mut(&id.0).expect("checked above");
                let token = CancellationToken::new();
                *entry.turn_cancel.lock().unwrap() = token.clone();
                // A reason left over from the previous turn must not colour this one.
                *entry.stall.lock().unwrap() = None;
                // Publish Running BEFORE handing the turn to the task. Once the
                // command is sent it may complete and publish Finished immediately;
                // publishing Running afterwards would overwrite that completion and
                // leave `wait` parked forever.
                entry.status.send_replace(ChildStatus::Running);
                if entry
                    .commands
                    .send(ChildCommand::Continue {
                        message,
                        token,
                        regrant,
                        reply: reply_tx,
                    })
                    .is_err()
                {
                    entry.status.send_replace(previous);
                    return Err(WorkerError::ShutDown);
                }
                reply
            };
            let Some(reply) = reply else {
                // No reconfiguration: the turn was accepted, as it always was.
                return Ok(());
            };
            // The child task owns the `Agent`, so IT applies the reconfiguration and
            // settles the grant and the status before it replies; this call does not
            // return until it has (the lock is released first: the task must never
            // wait on it).
            match reply.await {
                Ok(Ok(())) => Ok(()),
                Ok(Err(reason)) => Err(WorkerError::Regrant(reason)),
                // The task ended without an answer: a shutdown, or a panic its guard
                // has already turned into `Failed`.
                Err(_) if self.shared.is_shut_down() => Err(WorkerError::ShutDown),
                Err(_) => Err(WorkerError::Regrant(
                    "the worker's task ended while its tools were being changed".to_string(),
                )),
            }
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        Box::pin(async move {
            let state = self.shared.state.lock().unwrap();
            state
                .children
                .iter()
                .map(|(id, entry)| (ChildId(id.clone()), entry.status.borrow().clone()))
                .collect()
        })
    }

    fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        Box::pin(async move {
            if self.shared.is_shut_down() {
                return Err(WorkerError::ShutDown);
            }
            let state = self.shared.state.lock().unwrap();
            let Some(entry) = state.children.get(&id.0) else {
                return Err(WorkerError::UnknownChild);
            };
            Ok(entry.description.clone())
        })
    }

    fn set_result_tool_name(&self, name: &str) {
        *self.shared.result_tool_name.lock().unwrap() = Some(name.to_string());
    }
}

/// Everything one child task owns: the agent, the turn it currently runs, and the
/// state the service shares with it.
struct ChildTask {
    id: String,
    agent: Agent,
    task: String,
    token: CancellationToken,
    commands: mpsc::UnboundedReceiver<ChildCommand>,
    /// [`InProcessWorkers::stall_child`] writes the reason here; the task reads it
    /// when the turn ends.
    stall: Arc<Mutex<Option<String>>>,
    status: watch::Sender<ChildStatus>,
    /// The child's report read at a turn end, sharing one cell with the host's tap.
    report: Arc<dyn ChildReport>,
    fallback: Option<ModelFallback>,
    notify_parent: bool,
}

/// The whole life of one child, owned by one task. `task` is replaced by each
/// `continue_child`; the `Agent` is never moved and its turns never overlap.
async fn run_child(shared: Arc<Shared>, child: ChildTask) {
    let ChildTask {
        id,
        mut agent,
        mut task,
        mut token,
        mut commands,
        stall,
        status,
        report,
        fallback,
        notify_parent: notifies_parent,
    } = child;
    // Armed whenever the child is `Running` — every turn, and the reconfiguration an
    // accepted re-grant applies before one: a panic there ends this task
    // (tokio catches it and drops the future) while the service still holds a clone
    // of the status sender, so without the guard the child would stay `Running` for
    // ever and every `wait` on it would park. Seen live in a host test that hung.
    let mut guard = AbnormalEnd {
        shared: Arc::clone(&shared),
        id: id.clone(),
        status: status.clone(),
        notify_parent: notifies_parent,
        armed: true,
    };
    loop {
        // The status is already Running and `token` already installed: whoever
        // accepted this turn did both before the task could see it.
        let mut end = agent.run_turn(task, token.clone()).await;
        loop {
            // Drain messages only after completion. A failed turn retains them;
            // retrying a provider must not repeat committed tools or user input.
            if matches!(end, TurnEnd::Completed { .. })
                && agent.has_pending_inbox()
                && let Some(next) = agent.run_inbox_turn(token.clone()).await
            {
                end = next;
                continue;
            }
            if !matches!(end, TurnEnd::ProviderFailed { .. }) || token.is_cancelled() {
                break;
            }
            let grant = shared
                .state
                .lock()
                .unwrap()
                .children
                .get(&id)
                .expect("running child remains registered")
                .grant
                .clone();
            let Some(next) = fallback.as_ref().and_then(|fallback| fallback(&grant)) else {
                break;
            };
            match next {
                Ok(next) => {
                    let origin = next.reconfiguration.provider.describe().origin;
                    match agent.reconfigure(next.reconfiguration).await {
                        Ok(()) => {
                            (next.installed)();
                            if let Some(entry) = shared.state.lock().unwrap().children.get_mut(&id)
                            {
                                entry.description = format!("{}/{}", origin.route, origin.model);
                            }
                            end = agent.resume_turn(token.clone()).await;
                        }
                        Err(error) => {
                            end = TurnEnd::ContextFailed {
                                message: error.to_string(),
                            };
                            break;
                        }
                    }
                }
                Err(message) => {
                    end = TurnEnd::ContextFailed { message };
                    break;
                }
            }
        }
        // The host owns work that outlives a tool call, not this worker turn. Settle it
        // before releasing the running slot or making any terminal result observable.
        let cancelled_jobs = report.turn_ended().await;
        // One snapshot describes the turn that just ended; the report body and the
        // status below both read it.
        let report_snapshot = report.snapshot();
        let mut final_text = last_assistant_text(&agent);
        // ADR-0120 point 5: a worker's turn now ends on its accepted finish, so the
        // last assistant text IS the text of the response that carried the finish.
        // When that response carried no text, fall back to the accepted finish's
        // `summary` — the model's final report, which the description tells it to put
        // there.
        if final_text.is_empty()
            && let Some(summary) = report_snapshot
                .finish
                .as_ref()
                .and_then(|finish| finish.summary.as_deref())
        {
            final_text.push_str(summary);
        }
        if !cancelled_jobs.is_empty() {
            final_text.push_str(&format!(
                "\nCancelled background jobs: {}",
                cancelled_jobs.join(", ")
            ));
        }
        // A guard of the host's own stopped this turn ([`InProcessWorkers::stall_child`]):
        // the child ends `Failed` with the host's sentence, not `Cancelled`.
        let mut child_status = child_status(
            &end,
            stall.lock().unwrap().take(),
            final_text,
            // The tap recorded this turn as it ran; the snapshot describes the turn
            // that just ended, and the NEXT turn starts from a fresh one.
            report_snapshot.clone(),
        );
        if !cancelled_jobs.is_empty() {
            let detail = format!("Cancelled background jobs: {}", cancelled_jobs.join(", "));
            match &mut child_status {
                ChildStatus::Cancelled => {
                    // The unit cancelled variant has no result text. Retain the actual
                    // cancelled turn end in a result only when cleanup needs reporting.
                    child_status = ChildStatus::Finished(ChildResult {
                        final_text: format!("Worker cancelled.\n{detail}"),
                        turn_end: end.clone(),
                        usage_total: None,
                        report: report_snapshot.clone(),
                    });
                }
                ChildStatus::Failed(reason) => reason.push_str(&format!("\n{detail}")),
                _ => {}
            }
        }
        // The turn ended on its own: the guard must not overwrite this status if the
        // task is dropped while it waits for a command (shutdown).
        guard.armed = false;
        // (1) store the status and (2) wake every `wait`er: one `send_replace`.
        status.send_replace(child_status.clone());
        // (3) a slot is given back: wake everyone waiting for capacity, too.
        shared.capacity.notify_waiters();
        // (4) exactly ONE notification, and only after the result is retrievable.
        if notifies_parent {
            notify_parent(&shared, &id, &child_status);
        }
        // The session is retained for repair: wait for `continue_child` or
        // shutdown instead of exiting. A continue that ADDS tools carries the
        // reconfiguration for the next turn (ADR-0050 item 6); the turn's message is
        // only accepted once it has been applied, and when `reconfigure` refuses one
        // the task reports the reason and waits for another command instead of
        // running a turn with the wrong tools.
        loop {
            let command = tokio::select! {
                biased;
                _ = shared.shutdown.cancelled() => None,
                command = commands.recv() => command,
            };
            let Some(ChildCommand::Continue {
                message,
                token: next_token,
                regrant,
                reply,
            }) = command
            else {
                // Shutdown. A continue accepted but never run must not leave the child
                // `Running` behind it.
                if matches!(&*status.borrow(), ChildStatus::Running) {
                    status.send_replace(ChildStatus::Cancelled);
                    shared.capacity.notify_waiters();
                }
                return;
            };
            // The continue that sent this already made the child `Running`: from here
            // on — the reconfiguration included — a panic must not strand it there.
            guard.armed = true;
            if let Some(regrant) = regrant {
                let PendingRegrant {
                    regranted,
                    grant,
                    previous,
                } = *regrant;
                match agent.reconfigure(regranted.reconfiguration).await {
                    Ok(()) => {
                        // In force: the factory commits what it staged, and the grant is
                        // stored before the turn can end and admit the next continue.
                        (regranted.installed)();
                        if let Some(entry) = shared.state.lock().unwrap().children.get_mut(&id) {
                            entry.grant = grant;
                        }
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    Err(error) => {
                        // Cleanup must finish before publishing a status that admits
                        // another regrant with its own staged state.
                        drop(regranted.installed);
                        // No turn, and the worker keeps its old tools and grant. It
                        // was `Running` for a moment, so the status goes back and the
                        // slot with it, before the caller (if still there) is told why.
                        guard.armed = false;
                        status.send_replace(previous);
                        shared.capacity.notify_waiters();
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(error.to_string()));
                        }
                        continue;
                    }
                }
            }
            task = message;
            token = next_token;
            break;
        }
    }
}

/// Ends a child whose task dies mid-turn. Tokio catches a panic inside a task and drops
/// its future; the service's own clone of the status sender keeps the watch alive, so
/// only this drop can turn the stranded `Running` into a terminal status and wake the
/// waiters, the capacity waiters and the parent.
struct AbnormalEnd {
    shared: Arc<Shared>,
    id: String,
    status: watch::Sender<ChildStatus>,
    /// As the child was started: a workflow step's parent hears of the run, not of it.
    notify_parent: bool,
    /// True while the child is `Running` (a turn, or the reconfiguration before one);
    /// false while the task waits for its next command.
    armed: bool,
}

impl Drop for AbnormalEnd {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let failed = ChildStatus::Failed(
            "the worker's task ended abnormally during its turn (a panic inside the agent)"
                .to_string(),
        );
        self.status.send_replace(failed.clone());
        self.shared.capacity.notify_waiters();
        if self.notify_parent {
            notify_parent(&self.shared, &self.id, &failed);
        }
    }
}

/// The child's terminal status for one turn: a `Cancelled` turn the host's own
/// guard stopped carries that guard's reason as `Failed(reason)`, so the parent
/// reads the host's sentence instead of "cancelled" (completion.md §3c). Every
/// other end maps as [`status_from_end`] always did, and a reason set for a turn
/// that ended some other way is dropped — it can never colour a later turn.
fn child_status(
    end: &TurnEnd,
    stall: Option<String>,
    final_text: String,
    report: WorkerReport,
) -> ChildStatus {
    match (stall, end) {
        (Some(message), TurnEnd::Cancelled) => ChildStatus::Failed(message),
        _ => status_from_end(end, final_text, report),
    }
}

/// The text of the child's LAST assistant item, empty if there is none.
fn last_assistant_text(agent: &Agent) -> String {
    agent
        .history()
        .iter()
        .rev()
        .find_map(|item| match item {
            Item::Assistant(item) => Some(item.text()),
            _ => None,
        })
        .unwrap_or_default()
}

/// `Completed` → `Finished`, `Cancelled` → `Cancelled`, anything else → `Failed`
/// with the failure message. `usage_total` is always `None` (unknown). `report` is
/// kept only by `Finished`: the other statuses carry no result to read it from.
fn status_from_end(end: &TurnEnd, final_text: String, report: WorkerReport) -> ChildStatus {
    match end {
        TurnEnd::Completed { .. } => ChildStatus::Finished(ChildResult {
            final_text,
            turn_end: end.clone(),
            usage_total: None,
            report,
        }),
        TurnEnd::Cancelled => ChildStatus::Cancelled,
        TurnEnd::ProviderFailed { error } => ChildStatus::Failed(error.to_string()),
        TurnEnd::CommitFailed { message } => ChildStatus::Failed(message.clone()),
        TurnEnd::ContextFailed { message } => ChildStatus::Failed(message.clone()),
    }
}

/// Send the ONE completion notification. A missing parent inbox is not an error:
/// the status is already retained.
fn notify_parent(shared: &Shared, id: &str, status: &ChildStatus) {
    let word = match status {
        ChildStatus::Finished(result) if result.turn_end == TurnEnd::Cancelled => "cancelled",
        ChildStatus::Finished(_) => "completed",
        ChildStatus::Cancelled => "cancelled",
        ChildStatus::Failed(_) => "failed",
        ChildStatus::Running => return,
    };
    let inbox = shared.parent_inbox.lock().unwrap().clone();
    if let Some(inbox) = inbox {
        // The result tool's model-facing name comes from the delegation tool that
        // owns it (ADR-0057); with none assembled, the notification names no tool.
        let text = match shared.result_tool_name.lock().unwrap().as_deref() {
            Some(name) => format!("Worker {id} finished ({word}). Use {name} to read its result."),
            None => format!("Worker {id} finished ({word})."),
        };
        let _ = inbox.send(InboxKind::Notification, text);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ProviderError, ProviderErrorKind, StopReason};

    #[test]
    fn status_from_end_maps_every_turn_end() {
        let report = WorkerReport::new(vec!["read".into(), "finish".into()]);
        let completed = status_from_end(
            &TurnEnd::Completed {
                stop: StopReason::EndTurn,
            },
            "answer".into(),
            report.clone(),
        );
        assert_eq!(
            completed,
            ChildStatus::Finished(ChildResult {
                final_text: "answer".into(),
                turn_end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
                usage_total: None,
                report,
            })
        );
        assert_eq!(
            status_from_end(&TurnEnd::Cancelled, "x".into(), WorkerReport::default()),
            ChildStatus::Cancelled
        );
        assert_eq!(
            status_from_end(
                &TurnEnd::ProviderFailed {
                    error: ProviderError::new(ProviderErrorKind::Transport, "broke"),
                },
                "x".into(),
                WorkerReport::default(),
            ),
            ChildStatus::Failed("Transport: broke".into())
        );
        assert_eq!(
            status_from_end(
                &TurnEnd::CommitFailed {
                    message: "no disk".into()
                },
                "x".into(),
                WorkerReport::default()
            ),
            ChildStatus::Failed("no disk".into())
        );
        assert_eq!(
            status_from_end(
                &TurnEnd::ContextFailed {
                    message: "too big".into()
                },
                "x".into(),
                WorkerReport::default()
            ),
            ChildStatus::Failed("too big".into())
        );
    }

    /// A guard-stopped turn is `Failed` with the guard's own sentence, not
    /// `Cancelled` — and a reason that arrives for a turn ending any other way is
    /// dropped instead of leaking into the next one.
    #[test]
    fn a_guard_stopped_turn_is_failed_with_its_reason() {
        let reason = "stalled: 2 context summaries without a change to the workspace";
        assert_eq!(
            child_status(
                &TurnEnd::Cancelled,
                Some(reason.into()),
                "x".into(),
                WorkerReport::default()
            ),
            ChildStatus::Failed(reason.into())
        );
        assert_eq!(
            child_status(
                &TurnEnd::Cancelled,
                None,
                "x".into(),
                WorkerReport::default()
            ),
            ChildStatus::Cancelled
        );
        assert_eq!(
            child_status(
                &TurnEnd::Completed {
                    stop: StopReason::EndTurn
                },
                Some(reason.into()),
                "answer".into(),
                WorkerReport::default()
            ),
            status_from_end(
                &TurnEnd::Completed {
                    stop: StopReason::EndTurn
                },
                "answer".into(),
                WorkerReport::default()
            )
        );
    }

    /// A missing tool is counted under its name, first-seen order kept, and a new
    /// turn starts from nothing — the granted tools stay.
    #[test]
    fn the_report_counts_missing_tools_and_resets_per_turn() {
        let mut report = WorkerReport::new(vec!["read".into(), "finish".into()]);
        report.note_missing_tool("edit");
        report.note_missing_tool("shell");
        report.note_missing_tool("edit");
        report.finish = Some(FinishReport {
            status: "blocked".into(),
            needs: Some("edit".into()),
            summary: Some("cannot write".into()),
            evidence: None,
        });
        assert_eq!(
            report.missing_tool_calls,
            vec![("edit".to_string(), 2), ("shell".to_string(), 1)]
        );

        report.reset_turn();
        assert_eq!(report.finish, None);
        assert!(report.missing_tool_calls.is_empty());
        assert_eq!(report.tools, vec!["read".to_string(), "finish".to_string()]);
    }

    // ------------------------------------------------- the grant grows (ADR-0050 item 6)

    use p1_contracts::{ModelOptions, Tool};
    use p1_core::AgentParts;
    use p1_testkit::{
        FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
        ScriptedProvider, text_response,
    };

    /// One child tool per granted module: a test factory has no catalog, so the
    /// re-assembled tool set is exactly the grant.
    fn tools_for(grant: &[String]) -> Vec<Arc<dyn Tool>> {
        grant
            .iter()
            .map(|module| Arc::new(FakeTool::new(module)) as Arc<dyn Tool>)
            .collect()
    }

    /// The child spec every re-grant test starts from: one granted module, `read`.
    fn spec() -> ChildSpec {
        ChildSpec {
            environment: "child".into(),
            task: "do it".into(),
            tools: vec!["read".into()],
            workspace: None,
            options: Default::default(),
        }
    }

    /// The reconfiguration a test factory's `regrant` builds: the SAME provider (so
    /// the child's scripted session continues) over the new tool set.
    fn reconfiguration(provider: Arc<ScriptedProvider>, grant: &[String]) -> Reconfiguration {
        Reconfiguration {
            provider,
            tools: tools_for(grant),
            system_prompt: "child".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: None,
        }
    }

    #[tokio::test]
    async fn model_fallback_preserves_user_and_completed_tool_history() {
        use p1_testkit::{Step, json_call, tool_call_response};
        let primary = Arc::new(ScriptedProvider::new(vec![
            tool_call_response(vec![json_call("read-1", "read", "{}")]),
            Step::SetupError(ProviderError::new(
                ProviderErrorKind::UsageLimitExhausted,
                "quota",
            )),
        ]));
        let backup = Arc::new(ScriptedProvider::new(vec![text_response("Recovered")]));
        let installed = Arc::new(AtomicUsize::new(0));
        let factory: AgentFactory = {
            let backup = backup.clone();
            let installed = installed.clone();
            Arc::new(move |spec| {
                let backup = backup.clone();
                let installed = installed.clone();
                Ok(ChildAgent {
                    agent: child_agent(primary.clone(), &spec.tools),
                    description: "primary".into(),
                    report: Arc::new(WorkerReport::default),
                    regrant: None,
                    fallback: Some(Arc::new(move |grant| {
                        if installed.load(Ordering::SeqCst) != 0 {
                            return None;
                        }
                        let installed = installed.clone();
                        Some(Ok(Regranted {
                            reconfiguration: reconfiguration(backup.clone(), grant),
                            installed: Box::new(move || {
                                installed.fetch_add(1, Ordering::SeqCst);
                            }),
                        }))
                    })),
                })
            })
        };
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        let ChildStatus::Finished(result) =
            workers.wait(&id, CancellationToken::new()).await.unwrap()
        else {
            panic!("fallback did not finish");
        };
        assert_eq!(result.final_text, "Recovered");
        assert_eq!(installed.load(Ordering::SeqCst), 1);
        assert_eq!(
            workers.describe(&id).await.unwrap(),
            "fake-route/fake-model"
        );
        let requests = backup.requests();
        assert_eq!(requests.len(), 1);
        assert_eq!(
            requests[0]
                .history
                .iter()
                .filter(|item| matches!(item, p1_contracts::Item::User { .. }))
                .count(),
            1
        );
        assert_eq!(
            requests[0]
                .history
                .iter()
                .filter(|item| matches!(item, p1_contracts::Item::ToolResult(_)))
                .count(),
            1
        );
        assert_eq!(tool_names(&requests[0]), ["read"]);
        workers.shutdown().await;
    }

    #[tokio::test]
    async fn exhausted_model_chain_keeps_provider_failure() {
        use p1_testkit::Step;
        let calls = Arc::new(AtomicUsize::new(0));
        let factory: AgentFactory = {
            let calls = calls.clone();
            Arc::new(move |spec| {
                let calls = calls.clone();
                Ok(ChildAgent {
                    agent: child_agent(
                        Arc::new(ScriptedProvider::new(vec![Step::SetupError(
                            ProviderError::new(
                                ProviderErrorKind::RateLimited,
                                "last model unavailable",
                            ),
                        )])),
                        &spec.tools,
                    ),
                    description: "last".into(),
                    report: Arc::new(WorkerReport::default),
                    regrant: None,
                    fallback: Some(Arc::new(move |_| {
                        calls.fetch_add(1, Ordering::SeqCst);
                        None
                    })),
                })
            })
        };
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        assert!(
            matches!(workers.wait(&id, CancellationToken::new()).await.unwrap(),
            ChildStatus::Failed(reason) if reason.contains("last model unavailable"))
        );
        assert_eq!(calls.load(Ordering::SeqCst), 1);
        workers.shutdown().await;
    }

    fn child_agent(provider: Arc<ScriptedProvider>, grant: &[String]) -> Agent {
        child_agent_with(provider, grant, Arc::new(RecordingJournal::new()))
    }

    fn child_agent_with(
        provider: Arc<ScriptedProvider>,
        grant: &[String],
        journal: Arc<dyn p1_contracts::CommitSink>,
    ) -> Agent {
        Agent::new(AgentParts {
            provider,
            tools: tools_for(grant),
            system_prompt: "child".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: Arc::new(ScriptedAuthorization::permit_all()),
            journal,
            events: Arc::new(RecordingEvents::new()),
        })
        .expect("the test child agent builds")
    }

    fn tool_names(request: &p1_contracts::ProviderRequest) -> Vec<String> {
        request.tools.iter().map(|tool| tool.name.clone()).collect()
    }

    /// A factory whose children each answer with `steps` text responses and record
    /// the provider that served them, so a test sees each turn's request. `decision`
    /// decides whether a re-grant succeeds; the grants it was asked for are recorded
    /// too, so a test sees the union. With `regrant` false the child has no re-grant
    /// path at all (a factory that cannot re-assemble what it built).
    type Providers = Arc<Mutex<Vec<Arc<ScriptedProvider>>>>;
    type Recorder = Arc<Mutex<Vec<Vec<String>>>>;
    type Decision = Arc<dyn Fn(&[String]) -> Result<(), String> + Send + Sync>;

    fn factory(
        steps: usize,
        regrant: bool,
        decision: impl Fn(&[String]) -> Result<(), String> + Send + Sync + 'static,
    ) -> (AgentFactory, Providers, Recorder) {
        let providers: Providers = Arc::new(Mutex::new(Vec::new()));
        let grants: Recorder = Arc::new(Mutex::new(Vec::new()));
        let providers_for_factory = Arc::clone(&providers);
        let grants_for_factory = Arc::clone(&grants);
        let decision: Decision = Arc::new(decision);
        let factory: AgentFactory = Arc::new(move |spec: &ChildSpec| {
            let provider = Arc::new(ScriptedProvider::new(
                (0..steps).map(|_| text_response("answered")).collect(),
            ));
            providers_for_factory
                .lock()
                .unwrap()
                .push(Arc::clone(&provider));
            let grants = Arc::clone(&grants_for_factory);
            let decision = Arc::clone(&decision);
            let provider_for_regrant = Arc::clone(&provider);
            Ok(ChildAgent {
                agent: child_agent(Arc::clone(&provider), &spec.tools),
                description: "route/model".into(),
                report: Arc::new(WorkerReport::default),
                fallback: None,
                regrant: regrant.then(|| {
                    Arc::new(move |grant: &[String]| -> Result<Regranted, String> {
                        grants.lock().unwrap().push(grant.to_vec());
                        decision(grant)?;
                        Ok(Regranted::new(reconfiguration(
                            Arc::clone(&provider_for_regrant),
                            grant,
                        )))
                    }) as Regrant
                }),
            })
        });
        (factory, providers, grants)
    }

    /// A continue with added modules: the union (existing order, then the new ones,
    /// no duplicates) is re-assembled and applied BEFORE the turn, the turn runs on
    /// the new tool set, and the grant is stored — the NEXT continue starts from it.
    #[tokio::test(start_paused = true)]
    async fn a_continue_with_added_tools_regrants_the_child_and_stores_the_grant() {
        let (factory, providers, grants) = factory(3, true, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 2);

        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        // `spec()` grants `read`; `edit` is new and the second `read` is a duplicate.
        workers
            .continue_child(&id, "again".into(), vec!["edit".into(), "read".into()])
            .await
            .unwrap();
        let status = workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert!(matches!(status, ChildStatus::Finished(_)), "{status:?}");
        // The stored grant grew, so the next continue unions on top of it.
        workers
            .continue_child(&id, "and again".into(), vec!["shell".into()])
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        assert_eq!(
            grants.lock().unwrap().as_slice(),
            [
                vec!["read".to_string(), "edit".to_string()],
                vec!["read".to_string(), "edit".to_string(), "shell".to_string()],
            ],
            "the union keeps the existing order and adds each new module once"
        );
        let providers = providers.lock().unwrap();
        let requests = providers[0].requests();
        assert_eq!(requests.len(), 3, "the child ran one turn per continue");
        assert_eq!(tool_names(&requests[0]), ["read"], "the start grant");
        // The re-granted tool set is in force when the next turn's request is built.
        assert_eq!(tool_names(&requests[1]), ["read", "edit"]);
        assert_eq!(tool_names(&requests[2]), ["read", "edit", "shell"]);
    }

    /// A `regrant` that fails: no turn runs, the child's status is the one it had, and
    /// its grant is NOT grown.
    #[tokio::test(start_paused = true)]
    async fn a_refused_regrant_runs_no_turn_and_keeps_the_old_grant() {
        let (factory, providers, grants) = factory(2, true, |grant: &[String]| {
            if grant.iter().any(|module| module == "edit") {
                Err("no edit here".to_string())
            } else {
                Ok(())
            }
        });
        let workers = InProcessWorkers::new(factory, 2);

        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert_eq!(
            workers
                .continue_child(&id, "again".into(), vec!["edit".into()])
                .await,
            Err(WorkerError::Regrant("no edit here".into()))
        );
        assert!(matches!(
            workers.status(&id).await.unwrap(),
            ChildStatus::Finished(_)
        ));
        assert_eq!(
            providers.lock().unwrap()[0].requests().len(),
            1,
            "a refused re-grant runs no turn"
        );

        // The failed grant was never stored: the next union starts from `read`.
        workers
            .continue_child(&id, "and again".into(), vec!["shell".into()])
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert_eq!(
            grants.lock().unwrap().as_slice(),
            [
                vec!["read".to_string(), "edit".to_string()],
                vec!["read".to_string(), "shell".to_string()],
            ]
        );
        assert_eq!(providers.lock().unwrap()[0].requests().len(), 2);
    }

    /// A factory that cannot re-assemble its children (no `regrant`): an `add_tools`
    /// continue is refused with `Regrant` and the worker keeps what it has, while an
    /// empty `add_tools` continue still just runs the turn.
    #[tokio::test(start_paused = true)]
    async fn a_factory_without_a_regrant_refuses_added_tools() {
        let (factory, providers, _) = factory(2, false, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 2);
        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        let error = workers
            .continue_child(&id, "again".into(), vec!["edit".into()])
            .await
            .expect_err("a factory without a re-grant path cannot add tools");
        assert!(
            matches!(&error, WorkerError::Regrant(reason) if reason.contains("re-assemble")),
            "{error:?}"
        );
        assert!(matches!(
            workers.status(&id).await.unwrap(),
            ChildStatus::Finished(_)
        ));
        assert_eq!(providers.lock().unwrap()[0].requests().len(), 1);

        // Today's continue is unchanged: an empty `add_tools` runs the turn.
        workers
            .continue_child(&id, "again".into(), Vec::new())
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert_eq!(providers.lock().unwrap()[0].requests().len(), 2);
    }

    /// An empty `add_tools` never reaches the re-grant path: the closure is not
    /// called and the child's tool set is untouched.
    #[tokio::test(start_paused = true)]
    async fn an_empty_add_tools_keeps_todays_continue() {
        let (factory, providers, grants) = factory(2, true, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 2);

        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        workers
            .continue_child(&id, "again".into(), Vec::new())
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        assert!(grants.lock().unwrap().is_empty(), "no re-assembly ran");
        let providers = providers.lock().unwrap();
        let requests = providers[0].requests();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            tool_names(&requests[1]),
            ["read"],
            "the tool set is unchanged"
        );
    }

    /// A caller on a script thread can hand off a fast turn while the runtime
    /// completes it; the completion must never be overwritten by Running.
    #[tokio::test]
    async fn a_fast_continue_retains_its_terminal_status() {
        let (factory, providers, _) = factory(129, false, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        for _ in 0..128 {
            let workers_for_thread = workers.clone();
            let id_for_thread = id.clone();
            let handle = tokio::runtime::Handle::current();
            tokio::task::spawn_blocking(move || {
                handle.block_on(workers_for_thread.continue_child(
                    &id_for_thread,
                    "again".into(),
                    Vec::new(),
                ))
            })
            .await
            .unwrap()
            .unwrap();
            let status = tokio::time::timeout(
                std::time::Duration::from_secs(2),
                workers.wait(&id, CancellationToken::new()),
            )
            .await
            .expect("a fast turn must wake its waiter")
            .unwrap();
            assert!(matches!(status, ChildStatus::Finished(_)), "{status:?}");
        }
        assert_eq!(providers.lock().unwrap()[0].requests().len(), 129);
    }

    // ------------------------------- prepared start and capacity (ADR-0053 item 2)

    use std::time::Duration;

    use p1_testkit::Step;

    /// What `start` builds from its spec for the prepared seam: the task text and the
    /// granted modules, the only two things the service needs before an agent exists.
    fn prepared() -> PreparedStart {
        PreparedStart {
            task: "do it".into(),
            tools: vec!["read".into()],
            ..PreparedStart::default()
        }
    }

    /// One child whose FIRST turn waits for cancellation: the running slot stays held
    /// until the test cancels it, which is the only way to observe the bound.
    fn hanging_child(spec: &ChildSpec) -> Result<ChildAgent, String> {
        let provider = Arc::new(ScriptedProvider::new(vec![Step::EventsThenAwaitCancel(
            vec![],
        )]));
        Ok(ChildAgent {
            agent: child_agent(provider, &spec.tools),
            description: "route/model".into(),
            report: Arc::new(WorkerReport::default),
            regrant: None,
            fallback: None,
        })
    }

    /// The service still wants a factory; these tests build every child through the
    /// prepared seam's `build` closure instead, so calling this one is a bug.
    fn unused_factory() -> AgentFactory {
        Arc::new(|_spec: &ChildSpec| unreachable!("the prepared seam supplies the build closure"))
    }

    /// The future `start_prepared` returns is `Send`, like every public async interface
    /// (ADR-0015) — the compile-time assertion the rhai spike used, since a future that
    /// only becomes `!Send` later is caught by no ordinary test.
    fn require_send<F: Send>(future: F) -> F {
        future
    }

    #[test]
    fn the_start_prepared_future_is_send() {
        let (factory, _, _) = factory(1, false, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 1);
        let future = require_send(workers.start_prepared(prepared(), |_| Err("no".to_string())));
        drop(future);
    }

    /// `build` is handed the id `start_prepared` then returns, and the children it builds
    /// run and finish exactly as `start`'s do.
    #[tokio::test(start_paused = true)]
    async fn start_prepared_builds_with_the_id_it_returns() {
        let (factory, _, _) = factory(1, false, |_| Ok(()));
        let workers = InProcessWorkers::new(Arc::clone(&factory), 2);
        let built: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let mut ids = Vec::new();
        for _ in 0..2 {
            let built = Arc::clone(&built);
            let factory = Arc::clone(&factory);
            let id = workers
                .start_prepared(prepared(), move |child_id| {
                    built.lock().unwrap().push(child_id.0.clone());
                    factory(&spec())
                })
                .await
                .unwrap();
            ids.push(id);
        }

        assert_eq!(ids, [ChildId("w1".into()), ChildId("w2".into())]);
        assert_eq!(
            *built.lock().unwrap(),
            ["w1", "w2"],
            "the build sees the id the call then returns"
        );
        for id in &ids {
            match workers.wait(id, CancellationToken::new()).await.unwrap() {
                ChildStatus::Finished(result) => assert_eq!(result.final_text, "answered"),
                other => panic!("{other:?}"),
            }
        }
    }

    /// A `build` that refuses is an invalid environment, and it consumes NOTHING: no
    /// child, no slot, and the id it was offered is still the next one.
    #[tokio::test(start_paused = true)]
    async fn a_failed_build_consumes_nothing() {
        let (factory, _, _) = factory(1, false, |_| Ok(()));
        let workers = InProcessWorkers::new(Arc::clone(&factory), 2);

        let error = workers
            .start_prepared(prepared(), |_| Err("no such environment".to_string()))
            .await
            .expect_err("an unknown environment is an invalid environment");
        assert_eq!(
            error,
            WorkerError::InvalidEnvironment("no such environment".into())
        );
        assert!(workers.list().await.is_empty(), "no child was inserted");
        assert_eq!(workers.running(), 0, "the slot was given back");

        let id = workers
            .start_prepared(prepared(), |_| factory(&spec()))
            .await
            .unwrap();
        assert_eq!(id, ChildId("w1".into()), "the id was not consumed");
    }

    /// The exact boundary of the id namespace (issue #98): `w<usize::MAX>` is still
    /// allocated and the counter reaches `usize::MAX`, and the NEXT start — direct or
    /// prepared — is a stable `IdsExhausted` error, never a wrapped id or a panic. A
    /// build that fails at the boundary consumes nothing, so the boundary id is still
    /// on offer to the next successful build.
    #[tokio::test(start_paused = true)]
    async fn the_last_worker_id_is_allocated_and_exhaustion_is_an_error() {
        let (factory, _, _) = factory(1, false, |_| Ok(()));
        let workers = InProcessWorkers::new(Arc::clone(&factory), 2);
        workers.reserve_ids(usize::MAX - 1);

        // A failed build at the boundary consumes no id: the boundary one is offered again.
        assert_eq!(
            workers
                .start_prepared(prepared(), |_| Err("no such environment".to_string()))
                .await,
            Err(WorkerError::InvalidEnvironment(
                "no such environment".into()
            ))
        );

        // `w<usize::MAX>` is the last allocatable id, and the build sees it.
        let id = workers
            .start_prepared(prepared(), |child_id| {
                assert_eq!(child_id.0, format!("w{}", usize::MAX));
                factory(&spec())
            })
            .await
            .expect("the last id is still allocated");
        assert_eq!(id, ChildId(format!("w{}", usize::MAX)));
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        // The namespace is exhausted: a stable error for every way of starting.
        assert_eq!(workers.start(spec()).await, Err(WorkerError::IdsExhausted));
        assert_eq!(
            workers
                .start_prepared(prepared(), |_| factory(&spec()))
                .await,
            Err(WorkerError::IdsExhausted)
        );
    }

    /// At the bound the slot check runs BEFORE the build: `LimitReached`, and the build
    /// closure is never called.
    #[tokio::test(start_paused = true)]
    async fn a_prepared_start_at_the_bound_never_builds() {
        let workers = InProcessWorkers::new(unused_factory(), 1);
        let id = workers
            .start_prepared(prepared(), |_| hanging_child(&spec()))
            .await
            .unwrap();
        assert!(matches!(
            workers.status(&id).await.unwrap(),
            ChildStatus::Running
        ));

        let built = Arc::new(Mutex::new(false));
        let built_for_build = Arc::clone(&built);
        let error = workers
            .start_prepared(prepared(), move |_| {
                *built_for_build.lock().unwrap() = true;
                Err("the bound must be refused before the build".to_string())
            })
            .await
            .expect_err("the bound is reached");
        assert_eq!(error, WorkerError::LimitReached { max: 1 });
        assert!(!*built.lock().unwrap(), "the build was not called");

        workers.cancel(&id).await.unwrap();
    }

    /// `wait_for_capacity`: at once with a slot free, pending at the bound — proved with
    /// a test-util clock, which is not a sleep — `true` when the running child gives the
    /// slot back, and `false` when the caller's cancel token fires first.
    #[tokio::test(start_paused = true)]
    async fn wait_for_capacity_resolves_on_a_free_slot_a_turn_end_or_cancel() {
        let workers = InProcessWorkers::new(unused_factory(), 1);
        assert_eq!(
            workers.wait_for_capacity(CancellationToken::new()).await,
            Ok(true),
            "nothing is running, so the slot is free at once"
        );

        let id = workers
            .start_prepared(prepared(), |_| hanging_child(&spec()))
            .await
            .unwrap();
        let waiting = workers.wait_for_capacity(CancellationToken::new());
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut waiting)
                .await
                .is_err(),
            "the bound is reached, so the wait stays pending"
        );

        // The child's turn ends: the slot is free and the waiter is woken — no polling.
        workers.cancel(&id).await.unwrap();
        assert_eq!(waiting.await, Ok(true));

        let id = workers
            .start_prepared(prepared(), |_| hanging_child(&spec()))
            .await
            .unwrap();
        let cancel = CancellationToken::new();
        let waiting = workers.wait_for_capacity(cancel.clone());
        tokio::pin!(waiting);
        cancel.cancel();
        assert_eq!(waiting.await, Ok(false), "the cancel token wins");
        workers.cancel(&id).await.unwrap();
    }

    /// After `shutdown` neither a slot nor a child is on offer.
    #[tokio::test(start_paused = true)]
    async fn a_shut_down_service_offers_no_capacity_and_builds_nothing() {
        let workers = InProcessWorkers::new(unused_factory(), 1);
        workers.shutdown().await;

        assert_eq!(
            workers.wait_for_capacity(CancellationToken::new()).await,
            Err(WorkerError::ShutDown)
        );
        let built = Arc::new(Mutex::new(false));
        let built_for_build = Arc::clone(&built);
        let started = workers
            .start_prepared(prepared(), move |_| {
                *built_for_build.lock().unwrap() = true;
                hanging_child(&spec())
            })
            .await;
        assert_eq!(started, Err(WorkerError::ShutDown));
        assert!(!*built.lock().unwrap(), "nothing was built");
    }

    /// A waiter that is already waiting is woken by the shutdown itself.
    #[tokio::test(start_paused = true)]
    async fn shutdown_wakes_a_capacity_waiter() {
        let workers = InProcessWorkers::new(unused_factory(), 1);
        let id = workers
            .start_prepared(prepared(), |_| hanging_child(&spec()))
            .await
            .unwrap();
        let waiting = workers.wait_for_capacity(CancellationToken::new());
        tokio::pin!(waiting);
        assert!(
            tokio::time::timeout(Duration::from_secs(60), &mut waiting)
                .await
                .is_err(),
            "the bound is reached, so the wait stays pending"
        );

        workers.shutdown().await;
        assert_eq!(waiting.await, Err(WorkerError::ShutDown));
        assert_eq!(workers.running(), 0, "every child was stopped");
        drop(id);
    }

    /// `running` and `max_concurrent` report the live count and the bound the service was
    /// built with.
    #[tokio::test(start_paused = true)]
    async fn running_and_max_concurrent_report_the_live_count() {
        let workers = InProcessWorkers::new(unused_factory(), 1);
        assert_eq!(workers.max_concurrent(), 1);
        assert_eq!(workers.running(), 0, "nothing has started");

        let id = workers
            .start_prepared(prepared(), |_| hanging_child(&spec()))
            .await
            .unwrap();
        assert_eq!(workers.running(), 1);

        workers.cancel(&id).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert_eq!(workers.running(), 0, "an ended turn frees the slot");
    }

    /// `notify_parent: false` keeps every turn of that child out of the parent's inbox —
    /// the first and a continued one — while a default prepared start still notifies.
    #[tokio::test(start_paused = true)]
    async fn a_prepared_start_without_notify_parent_sends_no_notification() {
        let (factory, _, _) = factory(2, false, |_| Ok(()));
        let workers = InProcessWorkers::new(Arc::clone(&factory), 2);
        let parent = child_agent(
            Arc::new(ScriptedProvider::new(Vec::new())),
            &["read".to_string()],
        );
        workers.set_parent_inbox(parent.inbox());

        let quiet = PreparedStart {
            notify_parent: false,
            ..prepared()
        };
        let id = workers
            .start_prepared(quiet, |_| factory(&spec()))
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        workers
            .continue_child(&id, "again".into(), Vec::new())
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert!(!parent.has_pending_inbox(), "a quiet child notifies nobody");

        let id = workers
            .start_prepared(prepared(), |_| factory(&spec()))
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert!(parent.has_pending_inbox(), "the default still notifies");
    }

    /// ADR-0057: the completion notification names the result tool the delegation
    /// tool set (its face), not a name hard-coded here. With no delegate tool, the
    /// notification names no tool at all.
    #[tokio::test(start_paused = true)]
    async fn the_result_tool_name_set_by_the_delegate_tool_reaches_the_notification() {
        let (factory, _, _) = factory(2, false, |_| Ok(()));
        let workers = InProcessWorkers::new(Arc::clone(&factory), 2);
        let mut parent = child_agent(
            Arc::new(ScriptedProvider::new(vec![text_response("ack")])),
            &["read".to_string()],
        );
        workers.set_parent_inbox(parent.inbox());
        // Stand in for the delegate tool's face: `worker_result` renamed.
        workers.set_result_tool_name("ResultTool");
        assert_eq!(workers.result_tool_name().as_deref(), Some("ResultTool"));

        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();
        parent
            .run_inbox_turn(CancellationToken::new())
            .await
            .expect("the notification turn runs");
        let text = parent
            .history()
            .iter()
            .find_map(|item| match item {
                Item::Inbox { text, .. } => Some(text.clone()),
                _ => None,
            })
            .expect("the completion notification was delivered");
        assert!(
            text.contains("Use ResultTool to read its result."),
            "{text:?}"
        );

        // A service with no delegate tool keeps the default and names no tool.
        let bare = InProcessWorkers::new(Arc::clone(&factory), 1);
        assert_eq!(bare.result_tool_name(), None);
    }

    /// A panic inside a turn (here: a provider asked for more than it scripted) must not
    /// strand the child in `Running`: the guard marks it `Failed`, `wait` returns, the
    /// slot is free again. Under paused time a `timeout` fires as soon as nothing is
    /// runnable, so a hang would fail this test at once instead of parking it.
    #[tokio::test(start_paused = true)]
    async fn a_panicking_turn_ends_the_child_failed_instead_of_hanging() {
        let factory: AgentFactory = Arc::new(|spec: &ChildSpec| {
            // No scripted response at all: the first request panics inside the task.
            let provider = Arc::new(ScriptedProvider::new(Vec::new()));
            Ok(ChildAgent {
                agent: child_agent(provider, &spec.tools),
                description: "route/model".into(),
                report: Arc::new(WorkerReport::default),
                regrant: None,
                fallback: None,
            })
        });
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        let status = tokio::time::timeout(
            Duration::from_secs(60),
            workers.wait(&id, CancellationToken::new()),
        )
        .await
        .expect("a dying child must end its wait")
        .unwrap();
        assert!(
            matches!(&status, ChildStatus::Failed(message) if message.contains("abnormally")),
            "{status:?}"
        );
        assert_eq!(workers.running(), 0, "the slot is free again");
        assert_eq!(
            workers.wait_for_capacity(CancellationToken::new()).await,
            Ok(true)
        );
    }

    // ------------------- the child task owns a continue's bookkeeping (issue #533)

    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Context, Poll, Waker};

    use p1_contracts::{CommitError, CommitSink, JournalRecord, RecordBody};

    /// Poll `future` exactly once, then drop it: a caller that goes away mid-call.
    fn poll_once_and_drop<F: Future>(future: F) -> Poll<F::Output> {
        let mut future = std::pin::pin!(future);
        future
            .as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
    }

    /// A scoped start whose caller is dropped after one poll: every child the service
    /// created is the scope's, so none runs where its scope cannot observe, continue or
    /// cancel it.
    #[tokio::test(start_paused = true)]
    async fn a_dropped_scoped_start_never_strands_its_child() {
        let (factory, _, _) = factory(1, false, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 2);
        let scopes = WorkerScopes::new(Arc::clone(&workers) as Arc<dyn WorkerService>);
        let scope = scopes.scope(ScopeKey {
            generation: 1,
            operation: "delegate".into(),
            parent: "main".into(),
        });

        let _ = poll_once_and_drop(scope.start(spec()));

        let started: Vec<ChildId> = workers.list().await.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            started,
            [ChildId("w1".into())],
            "the one poll started the child"
        );
        let scoped: Vec<ChildId> = scope.list().await.into_iter().map(|(id, _)| id).collect();
        assert_eq!(
            scoped, started,
            "the child is recorded in the scope that started it"
        );
        assert!(matches!(
            scope.wait(&started[0], CancellationToken::new()).await,
            Ok(ChildStatus::Finished(_))
        ));
    }

    /// A child whose re-grant the AGENT refuses (two tools under one name: the factory
    /// builds it, `Agent::reconfigure` rejects it). The caller that asked is dropped while
    /// it waits: the child still gets its status back and the slot is free.
    #[tokio::test(start_paused = true)]
    async fn a_refused_reconfiguration_restores_the_status_without_its_caller() {
        let factory: AgentFactory = Arc::new(|spec: &ChildSpec| {
            let provider = Arc::new(ScriptedProvider::new(vec![text_response("answered")]));
            let for_regrant = Arc::clone(&provider);
            Ok(ChildAgent {
                agent: child_agent(provider, &spec.tools),
                description: "route/model".into(),
                report: Arc::new(WorkerReport::default),
                fallback: None,
                regrant: Some(Arc::new(
                    move |grant: &[String]| -> Result<Regranted, String> {
                        let mut doubled = grant.to_vec();
                        doubled.push(grant[0].clone());
                        Ok(Regranted::new(reconfiguration(
                            Arc::clone(&for_regrant),
                            &doubled,
                        )))
                    },
                )),
            })
        });
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        assert!(
            poll_once_and_drop(workers.continue_child(&id, "again".into(), vec!["edit".into()]))
                .is_pending(),
            "the call waits for the child task's answer"
        );
        let status = tokio::time::timeout(
            Duration::from_secs(60),
            workers.wait(&id, CancellationToken::new()),
        )
        .await
        .expect("the refusal puts the status back without its caller")
        .unwrap();
        assert!(matches!(status, ChildStatus::Finished(_)), "{status:?}");
        assert_eq!(workers.running(), 0, "the slot is free again");

        // A caller that stays hears the refusal, and the child is unchanged after it.
        let refused = workers
            .continue_child(&id, "again".into(), vec!["edit".into()])
            .await;
        assert!(
            matches!(refused, Err(WorkerError::Regrant(_))),
            "{refused:?}"
        );
        assert!(matches!(
            workers.status(&id).await.unwrap(),
            ChildStatus::Finished(_)
        ));
    }

    /// A re-grant whose caller is dropped while it waits is still stored once it is in
    /// force: the NEXT continue unions on top of it instead of dropping the tool added.
    #[tokio::test(start_paused = true)]
    async fn an_installed_regrant_is_stored_without_its_caller() {
        let (factory, providers, grants) = factory(3, true, |_| Ok(()));
        let workers = InProcessWorkers::new(factory, 2);
        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        assert!(
            poll_once_and_drop(workers.continue_child(&id, "again".into(), vec!["edit".into()]))
                .is_pending()
        );
        let status = workers.wait(&id, CancellationToken::new()).await.unwrap();
        assert!(matches!(status, ChildStatus::Finished(_)), "{status:?}");
        workers
            .continue_child(&id, "and again".into(), vec!["shell".into()])
            .await
            .unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        assert_eq!(
            grants.lock().unwrap().as_slice(),
            [
                vec!["read".to_string(), "edit".to_string()],
                vec!["read".to_string(), "edit".to_string(), "shell".to_string()],
            ],
            "the grant the dropped call asked for was stored"
        );
        let providers = providers.lock().unwrap();
        let requests = providers[0].requests();
        assert_eq!(tool_names(&requests[2]), ["read", "edit", "shell"]);
    }

    /// Accepts every record but panics on the `Environment` record a reconfiguration
    /// commits (the first turn's is seq 0).
    struct PanicsOnReconfigure;

    impl CommitSink for PanicsOnReconfigure {
        fn commit<'a>(
            &'a self,
            record: &'a JournalRecord,
        ) -> BoxFuture<'a, Result<(), CommitError>> {
            if record.seq > 0 && matches!(record.body, RecordBody::Environment { .. }) {
                panic!("the journal broke while the worker was reconfigured");
            }
            Box::pin(async { Ok(()) })
        }
    }

    /// A panic inside `Agent::reconfigure` ends the child `Failed`: the continue made it
    /// `Running`, so the guard is armed for the reconfiguration as for a turn, and the
    /// caller hears a re-grant failure, not a shutdown.
    #[tokio::test(start_paused = true)]
    async fn a_panic_while_reconfiguring_ends_the_child_failed() {
        let factory: AgentFactory = Arc::new(|spec: &ChildSpec| {
            let provider = Arc::new(ScriptedProvider::new(vec![text_response("answered")]));
            let for_regrant = Arc::clone(&provider);
            Ok(ChildAgent {
                agent: child_agent_with(provider, &spec.tools, Arc::new(PanicsOnReconfigure)),
                description: "route/model".into(),
                report: Arc::new(WorkerReport::default),
                fallback: None,
                regrant: Some(Arc::new(
                    move |grant: &[String]| -> Result<Regranted, String> {
                        Ok(Regranted::new(reconfiguration(
                            Arc::clone(&for_regrant),
                            grant,
                        )))
                    },
                )),
            })
        });
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        workers.wait(&id, CancellationToken::new()).await.unwrap();

        let continued = workers
            .continue_child(&id, "again".into(), vec!["edit".into()])
            .await;
        assert!(
            matches!(continued, Err(WorkerError::Regrant(_))),
            "{continued:?}"
        );
        let status = tokio::time::timeout(
            Duration::from_secs(60),
            workers.wait(&id, CancellationToken::new()),
        )
        .await
        .expect("a child that died reconfiguring must end its wait")
        .unwrap();
        assert!(
            matches!(&status, ChildStatus::Failed(message) if message.contains("abnormally")),
            "{status:?}"
        );
        assert_eq!(workers.running(), 0, "the slot is free again");
    }

    /// Refuses every commit, and panics once asked too often, so a worker that retries a
    /// refused commit for ever fails the test instead of hanging it.
    #[derive(Default)]
    struct RefusingJournal {
        attempts: AtomicUsize,
    }

    impl CommitSink for RefusingJournal {
        fn commit<'a>(
            &'a self,
            _record: &'a JournalRecord,
        ) -> BoxFuture<'a, Result<(), CommitError>> {
            let attempts = self.attempts.fetch_add(1, Ordering::SeqCst) + 1;
            assert!(
                attempts <= 50,
                "the worker retried a refused commit {attempts} times"
            );
            Box::pin(async { Err(CommitError("the disk is gone".into())) })
        }
    }

    /// A turn whose commit is refused while inbox messages wait: the child stops
    /// draining and ends `Failed` with the commit's reason; the messages stay queued.
    #[tokio::test(start_paused = true)]
    async fn a_refused_commit_with_queued_inbox_messages_ends_the_child_failed() {
        let factory: AgentFactory = Arc::new(|spec: &ChildSpec| {
            let provider = Arc::new(ScriptedProvider::new(Vec::new()));
            let agent =
                child_agent_with(provider, &spec.tools, Arc::new(RefusingJournal::default()));
            let _ = agent
                .inbox()
                .send(InboxKind::Notification, "news".to_string());
            Ok(ChildAgent {
                agent,
                description: "route/model".into(),
                report: Arc::new(WorkerReport::default),
                regrant: None,
                fallback: None,
            })
        });
        let workers = InProcessWorkers::new(factory, 1);
        let id = workers.start(spec()).await.unwrap();
        let status = tokio::time::timeout(
            Duration::from_secs(60),
            workers.wait(&id, CancellationToken::new()),
        )
        .await
        .expect("the child must end")
        .unwrap();
        assert!(
            matches!(&status, ChildStatus::Failed(message) if message.contains("the disk is gone")),
            "{status:?}"
        );
        assert_eq!(workers.running(), 0);
    }
}
