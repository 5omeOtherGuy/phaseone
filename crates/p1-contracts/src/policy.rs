//! The two named decision points, and observation.
//!
//! Policies decide; events only observe. There is no general hook bus: a new kind
//! of interception is a deliberate contract change.

use serde::{Deserialize, Serialize};

use crate::history::{Item, ToolCall, ToolResultItem};
use crate::provider::{ProviderError, StopReason, Usage};
use crate::tool::{Effect, ToolIdentity};
use crate::{BoxFuture, CancellationToken};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ContextError {
    #[error("context preparation was cancelled")]
    Cancelled,
    #[error("context preparation failed: {0}")]
    Failed(String),
}

/// Everything a context policy sees for one preparation.
pub struct ContextInput<'a> {
    /// The current model-visible history (the journal's projection).
    pub history: &'a [Item],
    /// Usage of this agent's most recent COMPLETED response, if it reported any.
    /// After a resume: the last journalled `AssistantCompleted.usage`.
    pub last_usage: Option<&'a Usage>,
    /// The turn's cancellation token. `prepare` must stop waiting when it fires.
    pub cancel: &'a CancellationToken,
}

/// A replacement history plus what preparing it cost.
pub struct Prepared {
    pub items: Vec<Item>,
    /// What preparing cost (e.g. a summarization request). `None` = unknown, never zero.
    pub usage: Option<Usage>,
}

/// Decision point 1: build the model-visible history for the next request.
pub trait ContextPolicy: Send + Sync {
    /// Return `None` to send the history unchanged, or `Some(prepared)` to REPLACE
    /// it from now on (journalled as `ContextReplaced`).
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>>;

    /// Manual compaction (ADR-0076): summarize the history NOW, whatever the
    /// threshold, through the same summary `prepare` makes at the threshold. A
    /// replacement is installed and journalled exactly like one from `prepare`
    /// (`ContextReplaced`). `Unchanged` when there is nothing to summarize. The
    /// default is a policy that never summarizes: it refuses.
    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        let _ = input;
        Box::pin(async {
            Err(ContextError::Failed(
                "this context policy has no summarizer (no [context] section)".to_string(),
            ))
        })
    }
}

/// What one manual compaction did (ADR-0076). Token counts are the policy's
/// estimates of the history before and after.
pub enum Compaction {
    /// One summary replaces the history from now on.
    Replaced {
        prepared: Prepared,
        tokens_before: u64,
        tokens_after: u64,
    },
    /// Nothing older than the kept tail to summarize: the history stays as it is.
    Unchanged { tokens: u64 },
}

#[derive(Debug, Clone, PartialEq)]
pub struct AuthorizationRequest<'a> {
    pub call: &'a ToolCall,
    pub identity: &'a ToolIdentity,
    pub effect: Effect,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Permit,
    /// Nothing is executed; `reason` is shown to the model.
    Deny {
        reason: String,
    },
}

/// Decision point 2: may this call run? Supplied by the host. An interactive host
/// asks the user INSIDE its implementation; a headless host must answer without
/// waiting for input that cannot come.
pub trait AuthorizationPolicy: Send + Sync {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision>;
}

/// How one turn ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "end", rename_all = "snake_case")]
pub enum TurnEnd {
    /// The model finished without requesting further tools.
    Completed {
        stop: StopReason,
    },
    Cancelled,
    ProviderFailed {
        error: ProviderError,
    },
    /// The journal refused a record; nothing after it happened.
    CommitFailed {
        message: String,
    },
    ContextFailed {
        message: String,
    },
}

/// Everything a frontend can observe. Deliberately small.
#[derive(Debug, Clone, PartialEq)]
pub enum AgentEvent {
    TurnStarted,
    RequestStarted {
        request_index: u32,
    },
    TextDelta {
        text: String,
    },
    ReasoningDelta {
        text: String,
    },
    ToolInputDelta {
        call_id: String,
        name: String,
        text: String,
    },
    /// A display-only notice from the provider's adapter (ADR-0048): the operator
    /// is told what the adapter did, nothing else. Never history, never journaled,
    /// never sent to the model; the text is the adapter's own constant.
    ProviderNotice {
        text: String,
    },
    /// A response completed. `usage` is `None` when unknown — never zero.
    ResponseCompleted {
        model: String,
        stop: StopReason,
        usage: Option<Usage>,
    },
    /// Inbox messages were delivered to the model at this boundary.
    InboxDelivered {
        count: usize,
    },
    /// The context policy replaced the model-visible history. Emitted only AFTER
    /// the `ContextReplaced` record was committed (core ruling R6).
    ContextReplaced {
        items_before: usize,
        items_after: usize,
        usage: Option<Usage>,
    },
    ToolStarted {
        call: ToolCall,
    },
    ToolFinished {
        result: ToolResultItem,
    },
    TurnFinished {
        end: TurnEnd,
    },
}

/// Observation only. Must not block: a slow frontend buffers on its own side.
pub trait EventSink: Send + Sync {
    fn emit(&self, event: AgentEvent);
}
