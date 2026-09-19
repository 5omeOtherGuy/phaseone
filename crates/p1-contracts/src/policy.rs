//! The two named decision points, and observation.
//!
//! Policies decide; events only observe. There is no general hook bus: a new kind
//! of interception is a deliberate contract change.

use serde::{Deserialize, Serialize};

use crate::BoxFuture;
use crate::history::{Item, ToolCall, ToolResultItem};
use crate::provider::{ProviderError, StopReason, Usage};
use crate::tool::{Effect, ToolIdentity};

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("context preparation failed: {0}")]
pub struct ContextError(pub String);

/// Decision point 1: build the model-visible history for the next request.
pub trait ContextPolicy: Send + Sync {
    /// `history` is the current projection. Return `None` to send it unchanged, or
    /// `Some(items)` to REPLACE it from now on (journalled as `ContextReplaced`).
    fn prepare<'a>(
        &'a self,
        history: &'a [Item],
    ) -> BoxFuture<'a, Result<Option<Vec<Item>>, ContextError>>;
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
