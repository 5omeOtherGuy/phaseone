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
use p1_core::{Agent, Inbox};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;

/// Identifier of one child within one service: `w1`, `w2`, … never reused.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ChildId(pub String);

/// The state a child is in. `Finished` carries the retained result; `Cancelled`
/// and `Failed` are terminal until `continue_child` starts another turn.
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
}

/// What the parent asks a worker service to run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSpec {
    /// Environment name understood by the host's factory.
    pub environment: String,
    /// The task text. It is the ONLY thing the child receives; the parent's
    /// transcript is never forwarded.
    pub task: String,
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
    #[error("the worker service has shut down")]
    ShutDown,
}

/// A built child: the agent plus the route/model description shown to the parent
/// (e.g. `openai-codex-responses/gpt-5.6-sol`).
pub struct ChildAgent {
    pub agent: Agent,
    pub description: String,
}

/// Builds a child `Agent` from its spec. Injected by the host and the SAME
/// assembly path a top-level agent uses, so a child on another route gets that
/// route's prompt and tools and nothing of the parent's.
///
/// In this slice the factory MUST build children WITHOUT the delegation tools:
/// nothing in this crate hands a child a [`WorkerService`], so a child cannot
/// start workers (no recursion).
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

    /// Another turn in the SAME child session (repair). `Busy` while a turn runs.
    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
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
    description: String,
}

enum ChildCommand {
    /// Another turn, with the cancellation token installed when it was accepted.
    Continue(String, CancellationToken),
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
            let (id, status, command_rx, token, agent) = {
                let mut state = self.shared.state.lock().unwrap();
                if state.shut_down || self.shared.shutdown.is_cancelled() {
                    return Err(WorkerError::ShutDown);
                }
                // Count RUNNING children before building: a rejected start must
                // not build (and then discard) a child it cannot run.
                self.shared.reserve_running_slot(&state)?;
                // Factory failure is an invalid environment, not a service fault.
                let child =
                    (self.shared.factory)(&spec).map_err(WorkerError::InvalidEnvironment)?;
                let id = format!("w{}", state.next_id + 1);
                state.next_id += 1;

                let (status, _) = watch::channel(ChildStatus::Running);
                let (commands, command_rx) = mpsc::unbounded_channel();
                let token = CancellationToken::new();
                state.children.insert(
                    id.clone(),
                    ChildEntry {
                        status: status.clone(),
                        commands,
                        turn_cancel: Arc::new(Mutex::new(token.clone())),
                        description: child.description.clone(),
                    },
                );
                (id, status, command_rx, token, child.agent)
            };

            let task_id = id.clone();
            let shared = Arc::clone(&self.shared);
            let handle = tokio::spawn(run_child(
                shared, task_id, agent, spec.task, token, command_rx, status,
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
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            if self.shared.is_shut_down() {
                return Err(WorkerError::ShutDown);
            }
            // Every transition into Running happens under the state lock: the limit
            // check, the status change and the new turn's cancellation token are one
            // step, so neither a concurrent `start`/`continue_child` nor a `cancel`
            // or `shutdown` can slip between them. No await is held here.
            let state = self.shared.state.lock().unwrap();
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
            let token = CancellationToken::new();
            entry
                .commands
                .send(ChildCommand::Continue(message, token.clone()))
                .map_err(|_| WorkerError::ShutDown)?;
            *entry.turn_cancel.lock().unwrap() = token;
            entry.status.send_replace(ChildStatus::Running);
            Ok(())
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

/// The whole life of one child, owned by one task. `task` is replaced by each
/// `continue_child`; the `Agent` is never moved and its turns never overlap.
async fn run_child(
    shared: Arc<Shared>,
    id: String,
    mut agent: Agent,
    mut task: String,
    mut token: CancellationToken,
    mut commands: mpsc::UnboundedReceiver<ChildCommand>,
    status: watch::Sender<ChildStatus>,
) {
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
        let child_status = status_from_end(&end, last_assistant_text(&agent));
        // (1) store the status and (2) wake every waiter: one `send_replace`.
        status.send_replace(child_status.clone());
        // (3) exactly ONE notification, and only after the result is retrievable.
        notify_parent(&shared, &id, &child_status);
        // The session is retained for repair: wait for `continue_child` or
        // shutdown instead of exiting.
        let command = tokio::select! {
            biased;
            _ = shared.shutdown.cancelled() => None,
            command = commands.recv() => command,
        };
        match command {
            Some(ChildCommand::Continue(message, next_token)) => {
                task = message;
                token = next_token;
            }
            _ => break,
        }
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
/// with the failure message. `usage_total` is always `None` (unknown).
fn status_from_end(end: &TurnEnd, final_text: String) -> ChildStatus {
    match end {
        TurnEnd::Completed { .. } => ChildStatus::Finished(ChildResult {
            final_text,
            turn_end: end.clone(),
            usage_total: None,
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
        let completed = status_from_end(
            &TurnEnd::Completed {
                stop: StopReason::EndTurn,
            },
            "answer".into(),
        );
        assert_eq!(
            completed,
            ChildStatus::Finished(ChildResult {
                final_text: "answer".into(),
                turn_end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
                usage_total: None,
            })
        );
        assert_eq!(
            status_from_end(&TurnEnd::Cancelled, "x".into()),
            ChildStatus::Cancelled
        );
        assert_eq!(
            status_from_end(
                &TurnEnd::ProviderFailed {
                    error: ProviderError::new(ProviderErrorKind::Transport, "broke"),
                },
                "x".into(),
            ),
            ChildStatus::Failed("Transport: broke".into())
        );
        assert_eq!(
            status_from_end(
                &TurnEnd::CommitFailed {
                    message: "no disk".into()
                },
                "x".into()
            ),
            ChildStatus::Failed("no disk".into())
        );
        assert_eq!(
            status_from_end(
                &TurnEnd::ContextFailed {
                    message: "too big".into()
                },
                "x".into()
            ),
            ChildStatus::Failed("too big".into())
        );
    }
}
