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
        /// The exit status the host observed for this call, when the tool records
        /// command evidence. Only the host writes it, so a component's output text
        /// can never set it on replay.
        ///
        /// The OUTER `Option` keeps an absent field (a journal written before this
        /// field existed, where the footer was the host's evidence format) apart
        /// from an explicit `null` (this host observed no exit, so the footer has no
        /// authority). `Some(Some(code))` is an observed exit.
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            deserialize_with = "deserialize_observed_exit"
        )]
        exit_code: Option<Option<i32>>,
    },
    /// The context policy replaced the model-visible history from here on.
    ContextReplaced {
        items: Vec<Item>,
        /// What preparing cost, where it was reported. Defaulted so journals
        /// written before this field existed still load.
        #[serde(default)]
        usage: Option<Usage>,
    },
}

/// Deserialize `exit_code`, keeping an ABSENT field apart from an explicit `null`.
/// Serde calls this only when the key is present, so a `null` becomes `Some(None)`
/// (the host observed no exit) while an absent key keeps `#[serde(default)]`'s
/// `None` (a journal written before the field existed).
fn deserialize_observed_exit<'de, D>(deserializer: D) -> Result<Option<Option<i32>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    <Option<i32> as Deserialize>::deserialize(deserializer).map(Some)
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
