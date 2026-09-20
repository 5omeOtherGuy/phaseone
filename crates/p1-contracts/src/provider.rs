//! Provider seam: request in, stream out, plus honest route description.
//!
//! A provider translates wire behaviour for ONE concrete model + route. It executes
//! no tools and chooses neither the prompt nor the tool set.
//!
//! Stream rules every adapter obeys (checked by the shared conformance suite):
//! 1. exactly one terminal `StreamEvent::Finished`, and it is the last event;
//! 2. a stream that ends without it is a failure, never an implicit completion;
//! 3. tool calls appear only inside the completed item, only when complete;
//! 4. unknown usage is `None`, never zero;
//! 5. an explicitly requested option the route cannot honour is an error from
//!    `validate`/`stream`, never silently dropped.

use std::collections::BTreeMap;
use std::pin::Pin;

use futures_core::Stream;
use serde::{Deserialize, Serialize};

use crate::history::{AssistantItem, Item, Origin};
use crate::tool::ToolDeclaration;
use crate::{BoxFuture, CancellationToken};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effort {
    Low,
    Medium,
    High,
    ExtraHigh,
    Max,
}

/// Explicit options. `None` means "the adapter's default for this route", which is
/// distinguishable from an explicit value: only explicit values can be rejected.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ModelOptions {
    pub reasoning_effort: Option<Effort>,
    pub max_output_tokens: Option<u32>,
    /// Stable key for provider-side prompt caching, where the route has one.
    pub cache_key: Option<String>,
    /// Route-native options, namespaced by route id (`"anthropic-messages.…"`).
    /// An adapter rejects keys in its own namespace it does not know, rejects
    /// keys in another adapter's namespace (a route switch must not silently
    /// drop an explicit preference), and ignores un-namespaced keys.
    pub native: BTreeMap<String, serde_json::Value>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ProviderRequest {
    /// The effective prompt text. Route-mandated wrapping (e.g. an identity block)
    /// is the adapter's wire behaviour and is reported by `RouteDescription`.
    pub system_prompt: String,
    /// History as prepared by the context policy.
    pub history: Vec<Item>,
    /// Exactly the tools assembled for this agent.
    pub tools: Vec<ToolDeclaration>,
    pub options: ModelOptions,
}

/// Whether this provider instance consumes [`ModelOptions::cache_key`]. The
/// truth about the instance itself, read from its request builder: a route that
/// has no provider-side cache key (`Unsupported`) must not be handed one, and
/// an explicit key on such a route is an error, not a silent drop.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CacheKeySupport {
    /// The route has no provider-side cache key; an explicit one is rejected.
    Unsupported,
    /// The route takes a stable cache key when one is supplied or generated.
    Optional,
}

/// What this provider instance actually is. Safe to log: never contains credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteDescription {
    pub origin: Origin,
    /// Declaration kinds this route can carry.
    pub supports_freeform_tools: bool,
    /// Text the route forces in front of the prompt, if any.
    pub mandatory_prompt_prefix: Option<String>,
    /// Whether the route bills per request (false for subscriptions: cost unknown).
    pub reports_cost: bool,
    /// Whether this instance consumes [`ModelOptions::cache_key`].
    pub cache_key: CacheKeySupport,
}

/// Token usage with provider fields kept distinct: the two routes disagree on
/// whether "input" includes cached tokens, so no pre-summed total is stored.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_uncached: Option<u64>,
    pub cache_read: Option<u64>,
    pub cache_write: Option<u64>,
    pub output: Option<u64>,
    /// Part of `output` spent on reasoning, where reported.
    pub reasoning_output: Option<u64>,
    /// Cost in micro-US-dollars, only where the route actually reports or prices it.
    pub cost_micro_usd: Option<u64>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    ToolUse,
    MaxOutputTokens,
    ContextWindowExceeded,
    Refusal,
    /// The provider paused a long turn and expects the same history to be re-sent.
    Paused,
    Other,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CompletedResponse {
    pub item: AssistantItem,
    pub stop: StopReason,
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProviderErrorKind {
    /// Bad request or unsupported option: retrying the same request cannot help.
    InvalidRequest,
    Authentication,
    RateLimited,
    ContextWindowExceeded,
    /// Network, 5xx, or a stream that broke (incl. EOF before the terminal event).
    Transport,
    /// The stream violated the wire protocol (e.g. unparsable event).
    Protocol,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error, Serialize, Deserialize)]
#[error("{kind:?}: {message}")]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    /// Diagnostic safe to show and log: never a credential or a response body.
    pub message: String,
}

impl ProviderError {
    pub fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// The single terminal outcome of a stream.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    Completed(CompletedResponse),
    Failed(ProviderError),
    Cancelled,
}

#[derive(Debug, Clone, PartialEq)]
pub enum StreamEvent {
    /// Incremental assistant text; `block` is the index of its block in the final item.
    TextDelta {
        block: usize,
        text: String,
    },
    /// Incremental reasoning text, display only.
    ReasoningDelta {
        block: usize,
        text: String,
    },
    /// Incremental input of a tool call still being produced. DISPLAY ONLY: never
    /// executed, never part of the history; the complete call arrives in `Finished`.
    ToolInputDelta {
        call_id: String,
        text: String,
    },
    /// Progress without content (ping, retry back-off) so a live stream is not
    /// mistaken for a stalled one.
    Activity,
    Finished(Outcome),
}

pub type ProviderStream = Pin<Box<dyn Stream<Item = StreamEvent> + Send>>;

pub trait Provider: Send + Sync {
    fn describe(&self) -> RouteDescription;

    /// Reject what this route cannot carry BEFORE a run starts: unsupported
    /// declaration kinds, unsupported explicit options, foreign native options
    /// in its namespace. Used by environment assembly to fail fast.
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError>;

    /// Start one model response. `Err` = setup failed and no stream exists. Once a
    /// stream exists, every failure is reported as its terminal event. Cancelling
    /// `cancel` ends the stream promptly with `Outcome::Cancelled`.
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>>;
}
