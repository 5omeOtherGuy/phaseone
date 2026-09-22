//! Optional delegation: the typed worker service and its in-process implementation.
//!
//! `p1-tool-delegate` depends on this crate's [`WorkerService`] trait only. Nothing
//! here names a model-facing tool, a prompt template, a provider or a UI.
//!
//! Lifecycle: every child is one tokio task that OWNS its `Agent` for the child's
//! whole life, so turns can never overlap and there is no `Mutex<Agent>` anywhere.
//! Completion is retained state plus ONE parent notification: the status is stored
//! first, then the parent is told. A missed or ignored notification loses nothing —
//! the result stays retrievable by id for the service's lifetime.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use p1_contracts::{BoxFuture, CancellationToken, InboxKind, Item, TurnEnd, Usage};
use p1_core::{Agent, Inbox, Reconfiguration};
use tokio::sync::{mpsc, oneshot, watch};
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
    pub report: Arc<dyn Fn() -> WorkerReport + Send + Sync>,
    /// Given the child's FULL new tool grant (the current modules plus the added
    /// ones), the [`Reconfiguration`] that assembles it with exactly those — or the
    /// reason it cannot (ADR-0050 item 6). `None` when the factory cannot re-assemble
    /// a child it already built (a test factory, `p1 env show`): the service then
    /// refuses an `add_tools` continue with [`WorkerError::Regrant`] and keeps the
    /// child's tools as they are.
    pub regrant: Option<Regrant>,
}

/// How a factory re-assembles one of its children under a larger grant: it is given
/// the granted tool MODULE names in order and returns everything a running agent can
/// be switched to between turns (ADR-0049).
pub type Regrant = Arc<dyn Fn(&[String]) -> Result<Reconfiguration, String> + Send + Sync>;

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
    /// tools.
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
    state: Mutex<State>,
    /// Cancelled by `shutdown` (and best-effort `Drop`); every child task races
    /// this against its next command.
    shutdown: CancellationToken,
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
    /// continue that adds tools carries the [`Reconfiguration`] the CHILD TASK must
    /// apply (it owns the `Agent`) and the channel its outcome is reported on, so the
    /// turn runs only after a successful reconfiguration (ADR-0050 item 6).
    Continue {
        message: String,
        token: CancellationToken,
        reconfig: Option<Reconfiguration>,
        reply: Option<oneshot::Sender<Result<(), String>>>,
    },
    Shutdown,
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
                state: Mutex::new(State {
                    children: BTreeMap::new(),
                    next_id: 0,
                    shut_down: false,
                }),
                shutdown: CancellationToken::new(),
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
        // Take the handles out first: awaiting must not hold the tasks lock.
        let tasks: Vec<JoinHandle<()>> = std::mem::take(&mut *self.tasks.lock().unwrap());
        for handle in tasks {
            let _ = handle.await;
        }
    }
}

impl Shared {
    /// `Err(LimitReached)` when `max_concurrent` children are already Running.
    /// Called with the state lock held, by everything that sets a child Running.
    /// A turn ending concurrently only lowers the count, so the check is safe.
    fn reserve_running_slot(&self, state: &State) -> Result<(), WorkerError> {
        let running = state
            .children
            .values()
            .filter(|entry| matches!(&*entry.status.borrow(), ChildStatus::Running))
            .count();
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
            // The state lock lives only inside this block: nothing below holds a
            // std lock across the `yield_now` (invariant 7d). The factory is
            // synchronous, so holding the lock across it keeps the RUNNING count
            // and the id assignment atomic under concurrent `start` calls.
            let (id, status, command_rx, token, stall, agent, report) = {
                let mut state = self.shared.state.lock().unwrap();
                if state.shut_down || self.shared.shutdown.is_cancelled() {
                    return Err(WorkerError::ShutDown);
                }
                // Count RUNNING children before building: a rejected start must
                // not build (and then discard) a child it cannot run.
                self.shared.reserve_running_slot(&state)?;
                // Factory failure is an invalid environment, not a service fault.
                let ChildAgent {
                    agent,
                    description,
                    report,
                    regrant,
                } = (self.shared.factory)(&spec).map_err(WorkerError::InvalidEnvironment)?;
                let id = format!("w{}", state.next_id + 1);
                state.next_id += 1;

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
                        // The grant the factory assembled the child with, kept as
                        // `continue_child` grows it.
                        grant: spec.tools.clone(),
                        regrant,
                        description,
                    },
                );
                (id, status, command_rx, token, stall, agent, report)
            };

            let task_id = id.clone();
            let shared = Arc::clone(&self.shared);
            let handle = tokio::spawn(run_child(
                shared,
                ChildTask {
                    id: task_id,
                    agent,
                    task: spec.task,
                    token,
                    commands: command_rx,
                    stall,
                    status,
                    report,
                },
            ));
            self.tasks.lock().unwrap().push(handle);
            // Give the fresh task one turn before returning: `start` promises the
            // child is running now, and a child that can complete without waiting
            // has then actually started. A yield is not a clock.
            tokio::task::yield_now().await;
            Ok(ChildId(id))
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
            // worker's tools unchanged.
            let (reply, new_grant, previous) = {
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
                let reconfig = if add_tools.is_empty() {
                    None
                } else {
                    let Some(regrant) = &entry.regrant else {
                        return Err(WorkerError::Regrant(format!(
                            "worker {} was not started by a factory that can re-assemble it",
                            id.0
                        )));
                    };
                    Some(regrant(&grant).map_err(WorkerError::Regrant)?)
                };
                let (reply, reply_tx) = if reconfig.is_some() {
                    let (tx, rx) = oneshot::channel();
                    (Some(rx), Some(tx))
                } else {
                    (None, None)
                };
                let entry = state.children.get_mut(&id.0).expect("checked above");
                let previous = entry.status.borrow().clone();
                let token = CancellationToken::new();
                entry
                    .commands
                    .send(ChildCommand::Continue {
                        message,
                        token: token.clone(),
                        reconfig,
                        reply: reply_tx,
                    })
                    .map_err(|_| WorkerError::ShutDown)?;
                *entry.turn_cancel.lock().unwrap() = token;
                // A reason left over from the previous turn must not colour this one.
                *entry.stall.lock().unwrap() = None;
                entry.status.send_replace(ChildStatus::Running);
                (reply, grant, previous)
            };
            let Some(reply) = reply else {
                // No reconfiguration: the turn was accepted, as it always was.
                return Ok(());
            };
            // The child task owns the `Agent`, so IT applies the reconfiguration —
            // and this call does not return until it has (the lock is released
            // first: the task must never wait on it).
            match reply.await {
                Ok(Ok(())) => {
                    // The new grant is stored only now, once it is really in force.
                    let mut state = self.shared.state.lock().unwrap();
                    if let Some(entry) = state.children.get_mut(&id.0) {
                        entry.grant = new_grant;
                    }
                    Ok(())
                }
                Ok(Err(reason)) => {
                    // `reconfigure` refused: no turn ran, and the worker keeps the
                    // tools it had.
                    let mut state = self.shared.state.lock().unwrap();
                    if let Some(entry) = state.children.get_mut(&id.0) {
                        entry.status.send_replace(previous);
                    }
                    Err(WorkerError::Regrant(reason))
                }
                // The child task is gone (shutdown): nothing was applied.
                Err(_) => Err(WorkerError::ShutDown),
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
    report: Arc<dyn Fn() -> WorkerReport + Send + Sync>,
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
    } = child;
    loop {
        // The status is already Running and `token` already installed: whoever
        // accepted this turn did both before the task could see it.
        let mut end = agent.run_turn(task, token.clone()).await;
        // Drain any inbox messages that arrived during the turn, per the spec.
        while agent.has_pending_inbox() {
            match agent.run_inbox_turn(token.clone()).await {
                Some(next) => end = next,
                None => break,
            }
        }
        // A guard of the host's own stopped this turn ([`InProcessWorkers::stall_child`]):
        // the child ends `Failed` with the host's sentence, not `Cancelled`.
        let child_status = child_status(
            &end,
            stall.lock().unwrap().take(),
            last_assistant_text(&agent),
            // The tap recorded this turn as it ran; the snapshot describes the turn
            // that just ended, and the NEXT turn starts from a fresh one.
            (report)(),
        );
        // (1) store the status and (2) wake every waiter: one `send_replace`.
        status.send_replace(child_status.clone());
        // (3) exactly ONE notification, and only after the result is retrievable.
        notify_parent(&shared, &id, &child_status);
        // The session is retained for repair: wait for `continue_child` or
        // shutdown instead of exiting. A continue that ADDS tools carries the
        // reconfiguration for the next turn (ADR-0050 item 6); the turn's message is
        // only accepted once it has been applied, and when `reconfigure` refuses one
        // the task reports the reason and waits for another command instead of
        // running a turn with the wrong tools.
        let mut command = tokio::select! {
            biased;
            _ = shared.shutdown.cancelled() => None,
            command = commands.recv() => command,
        };
        loop {
            let Some(ChildCommand::Continue {
                message,
                token: next_token,
                reconfig,
                reply,
            }) = command
            else {
                return;
            };
            if let Some(reconfig) = reconfig {
                match agent.reconfigure(reconfig) {
                    Ok(()) => {
                        if let Some(reply) = reply {
                            let _ = reply.send(Ok(()));
                        }
                    }
                    Err(error) => {
                        // No turn: the service puts the child's status back and tells
                        // the parent why, and the worker keeps its old tools.
                        if let Some(reply) = reply {
                            let _ = reply.send(Err(error.to_string()));
                        }
                        command = tokio::select! {
                            biased;
                            _ = shared.shutdown.cancelled() => None,
                            command = commands.recv() => command,
                        };
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
        ChildStatus::Finished(_) => "completed",
        ChildStatus::Cancelled => "cancelled",
        ChildStatus::Failed(_) => "failed",
        ChildStatus::Running => return,
    };
    let inbox = shared.parent_inbox.lock().unwrap().clone();
    if let Some(inbox) = inbox {
        let text = format!("Worker {id} finished ({word}). Use worker_result to read its result.");
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
        }
    }

    fn child_agent(provider: Arc<ScriptedProvider>, grant: &[String]) -> Agent {
        Agent::new(AgentParts {
            provider,
            tools: tools_for(grant),
            system_prompt: "child".into(),
            options: ModelOptions::default(),
            context: Arc::new(PassthroughContext),
            authorization: Arc::new(ScriptedAuthorization::permit_all()),
            journal: Arc::new(RecordingJournal::new()),
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
                regrant: regrant.then(|| {
                    Arc::new(move |grant: &[String]| -> Result<Reconfiguration, String> {
                        grants.lock().unwrap().push(grant.to_vec());
                        decision(grant)?;
                        Ok(reconfiguration(Arc::clone(&provider_for_regrant), grant))
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
}
