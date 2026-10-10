//! The front-end port: how a front end that is not part of the host attaches to a
//! session (D7, ADR-0152).
//!
//! The host composes against its own `FrontEnd` trait, which names host types; a
//! front end in its own crate implements [`FrontEndPort`] instead and the host plugs
//! it in through a bridge. Traffic goes both ways: the port receives the session's
//! events, authorization questions and background-work signals, and drives the
//! session through the [`SessionHandle`] the host hands to [`FrontEndPort::run`].
//! Nothing here names a wire protocol: a protocol is an adapter behind the port.

use std::sync::Arc;

use crate::policy::{AuthorizationPolicy, EventSink, TurnEnd};
use crate::{BoxFuture, CancellationToken};

/// What kind of background work a [`BackgroundSignal`] is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundKind {
    /// A delegated worker, started directly or as a workflow step.
    Worker,
    /// A workflow run.
    Workflow,
}

/// Whether the work started or ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum BackgroundPhase {
    Started,
    Ended,
}

/// Background work that runs beside the session's turns started or ended. A
/// background shell job is part of the turn's tool calls and is never signalled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackgroundSignal {
    pub phase: BackgroundPhase,
    pub kind: BackgroundKind,
    /// The host's id for the work (`w1`, `wf1`).
    pub id: String,
    /// The ordinal (from 1) of the session turn, prompt or inbox, that was running
    /// when the work started; `None` when it started between turns. An end carries
    /// its start's turn.
    pub turn: Option<u64>,
}

/// An observed workflow execution step, not a predeclared todo. A replayed or
/// refused step can end without starting, so its task text may be unavailable.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    pub run: String,
    /// From 1 in the run's `agent()` call order; the identity within the run.
    pub ordinal: u32,
    pub call: String,
    pub label: Option<String>,
    /// The script's task text, not the worker's assembled prompt.
    pub task: Option<String>,
    /// `running`, `done`, `failed`, `blocked` or `cancelled`.
    pub status: String,
}

/// A setting of the session a front end can change between turns. More kinds come
/// with later settings (the permission mode, #696).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ConfigKind {
    /// The model: `E/P`, as the host's `/model` accepts it.
    Model,
    /// The reasoning effort of the model, as the host's `/effort` accepts it.
    Effort,
}

/// One value a [`ConfigChoice`] can take.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigValue {
    /// What a front end sends back to choose it.
    pub value: String,
    pub name: String,
    pub description: Option<String>,
}

/// One setting with its current value and every value the session can serve now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigChoice {
    pub kind: ConfigKind,
    /// One of `values`.
    pub current: String,
    pub values: Vec<ConfigValue>,
}

/// The session as a front end drives it. The host implements it; every method takes
/// `&self`, so a front end may cancel from one task while another awaits a turn.
pub trait SessionHandle: Send + Sync {
    /// Run one turn on `text` and return how it ended; `cancel` ends the turn. A
    /// second call waits until the running turn or inbox drain is over. User: the
    /// front end's prompt request (#673 `session/prompt`).
    fn prompt<'a>(&'a self, text: String, cancel: CancellationToken) -> BoxFuture<'a, TurnEnd>;

    /// Cancel every running workflow run. User: the front end's cancel request
    /// (#673 `session/cancel`), next to cancelling the turn's token.
    fn cancel_runs<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// Cancel the running turn of every worker. User: the same cancel request as
    /// [`SessionHandle::cancel_runs`].
    fn stop_workers<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// Run inbox turns (worker and workflow notices) until the inbox is empty,
    /// without waiting for running work; the last turn's end, `None` when the inbox
    /// was empty. User: the front end after a prompt turn and when a
    /// [`BackgroundPhase::Ended`] signal arrived (#673).
    fn drain_inbox<'a>(&'a self, cancel: CancellationToken) -> BoxFuture<'a, Option<TurnEnd>>;

    /// Resolves once an inbox message is pending (at once when one is). It holds the
    /// session while it waits, so a front end drops it before it prompts. User: the
    /// front end's idle loop, which races it with the next request as the host's line
    /// loop does, then calls [`SessionHandle::drain_inbox`] (#673).
    fn inbox_ready<'a>(&'a self) -> BoxFuture<'a, ()>;

    /// The settings a front end may change, in the order to show them; empty when
    /// the session offers none. It never waits for a running turn. User: the
    /// front end's session setup and its answer to a change (#675).
    fn config<'a>(&'a self) -> BoxFuture<'a, Vec<ConfigChoice>> {
        Box::pin(async { Vec::new() })
    }

    /// Change one setting to `value`, one of the values [`SessionHandle::config`]
    /// lists. Like [`SessionHandle::prompt`] it waits for a running turn; the change
    /// applies from the next turn. On an error nothing changed. User: the front end's
    /// change request (#675).
    fn set_config<'a>(
        &'a self,
        kind: ConfigKind,
        value: &'a str,
    ) -> BoxFuture<'a, Result<(), String>> {
        let _ = (kind, value);
        Box::pin(async { Err("this session has no settings to change".to_string()) })
    }
}

/// A front end attached through the port. One value per session.
pub trait FrontEndPort: Send + Sync {
    /// The sink the session's own agent emits to.
    fn event_sink(&self) -> Arc<dyn EventSink>;

    /// The sink for one worker's agent, `worker_id` as in [`BackgroundSignal::id`].
    fn child_event_sink(&self, worker_id: &str) -> Arc<dyn EventSink>;

    /// The authorization policy for the session's agent and every worker.
    fn authorization(&self) -> Arc<dyn AuthorizationPolicy>;

    /// Background work started or ended. Must not block, like [`EventSink::emit`].
    fn background(&self, signal: BackgroundSignal);

    /// The parent's effective context capacity and summarization threshold. An
    /// absent context configuration stays unknown, never a guessed capacity.
    fn context_configured(&self, _window_tokens: Option<u64>, _summarize_at_tokens: Option<u64>) {}

    /// A workflow step started or ended. Must not block, like [`EventSink::emit`].
    fn workflow_step(&self, _step: &WorkflowStep) {}

    /// Drive the session until the front end is done; the process exit code.
    fn run<'a>(&'a self, session: &'a dyn SessionHandle) -> BoxFuture<'a, i32>;
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_send_sync<T: ?Sized + Send + Sync>() {}

    /// Both ports are object-safe and Send-capable, so a front end crate can hold
    /// them behind `Arc<dyn …>` and move them across tasks.
    #[test]
    fn ports_are_object_safe_and_send() {
        assert_send_sync::<dyn FrontEndPort>();
        assert_send_sync::<dyn SessionHandle>();
        assert_send_sync::<BackgroundSignal>();
    }
}
