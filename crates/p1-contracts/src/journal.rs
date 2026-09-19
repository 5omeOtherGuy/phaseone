//! Session journal: the append-only record of what the model was actually sent and
//! returned. In-memory state is its projection, not a second truth.
//!
//! The core owns ordering and asks a `CommitSink` to make each record durable at a
//! meaningful boundary. A failed commit stops forward progress. Stream deltas, UI
//! state and metrics are NOT journalled.
//!
//! Commit boundaries (the core guarantees this order):
//!   UserInput / Inbox      — before the request that contains it
//!   AssistantCompleted     — before any of its tool calls is authorized or executed
//!   ToolStarted            — before that call's side effects
//!   ToolFinished           — before the next request
//! A `ToolStarted` without a matching `ToolFinished` means the outcome is unknown.

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::history::{AssistantItem, InboxKind, Item, ToolResultItem};
use crate::provider::{ModelOptions, ProviderError, RouteDescription, StopReason, Usage};
use crate::tool::{ToolDeclaration, ToolIdentity};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptionReason {
    Cancelled,
    ProviderFailed,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum RecordBody {
    /// The effective environment of this agent: what was actually assembled.
    /// Written first, and again whenever the environment is explicitly changed.
    Environment {
        route: RouteDescription,
        system_prompt: String,
        tools: Vec<(ToolDeclaration, ToolIdentity)>,
        options: ModelOptions,
    },
    UserInput {
        text: String,
    },
    Inbox {
        kind: InboxKind,
        text: String,
    },
    AssistantCompleted {
        item: AssistantItem,
        stop: StopReason,
        usage: Option<Usage>,
    },
    /// A response that did not complete. `partial_text` is what was streamed and
    /// shown; it is NOT part of the model-visible history.
    AssistantInterrupted {
        reason: InterruptionReason,
        partial_text: String,
        error: Option<ProviderError>,
    },
    ToolStarted {
        call_id: String,
        identity: ToolIdentity,
    },
    ToolFinished {
        result: ToolResultItem,
    },
    /// The context policy replaced the model-visible history from here on.
    ContextReplaced {
        items: Vec<Item>,
    },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct JournalRecord {
    /// Dense, starting at 0, assigned by the core. Stores reject gaps and repeats.
    pub seq: u64,
    #[serde(flatten)]
    pub body: RecordBody,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("journal commit failed: {0}")]
pub struct CommitError(pub String);

/// Narrow asynchronous sink the core commits through. The host supplies the store.
pub trait CommitSink: Send + Sync {
    /// Returns once the record is as durable as this store promises.
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>>;
}
