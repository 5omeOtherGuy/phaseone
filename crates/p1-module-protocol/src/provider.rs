//! Wire form of the provider-side values (`schema/stream-event.json`,
//! `provider-error.json`, `model-options.json`, `route-description.json`, `usage.json`).

use std::collections::BTreeMap;

use p1_contracts::{
    CacheKeySupport, CompletedResponse, Effort, ModelOptions, Outcome, ProviderError,
    ProviderErrorKind, RouteDescription, StopReason, StreamEvent, Usage,
};
use serde::{Deserialize, Serialize};

use crate::history::{WireAssistantItem, WireOrigin};
use crate::{ConversionError, refuse_null, to_u64, to_usize};

/// Requested reasoning effort.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireEffort {
    /// Low.
    Low,
    /// Medium.
    Medium,
    /// High.
    High,
    /// Extra high.
    ExtraHigh,
    /// The route's maximum.
    Max,
}

impl From<Effort> for WireEffort {
    fn from(effort: Effort) -> Self {
        match effort {
            Effort::Low => Self::Low,
            Effort::Medium => Self::Medium,
            Effort::High => Self::High,
            Effort::ExtraHigh => Self::ExtraHigh,
            Effort::Max => Self::Max,
        }
    }
}

impl From<WireEffort> for Effort {
    fn from(effort: WireEffort) -> Self {
        match effort {
            WireEffort::Low => Self::Low,
            WireEffort::Medium => Self::Medium,
            WireEffort::High => Self::High,
            WireEffort::ExtraHigh => Self::ExtraHigh,
            WireEffort::Max => Self::Max,
        }
    }
}

/// Explicit model options. An absent field means "the route's default", which must stay
/// distinguishable from an explicit value because only explicit values can be rejected.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireModelOptions {
    /// Absent: the route's default.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_effort: Option<WireEffort>,
    /// Absent: the route's default.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub max_output_tokens: Option<u32>,
    /// Stable key for provider-side prompt caching, where the route has one.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_key: Option<String>,
    /// Route-native options namespaced by route id; values are the adapter's to interpret.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub native: BTreeMap<String, serde_json::Value>,
}

impl From<ModelOptions> for WireModelOptions {
    fn from(options: ModelOptions) -> Self {
        Self {
            reasoning_effort: options.reasoning_effort.map(Into::into),
            max_output_tokens: options.max_output_tokens,
            cache_key: options.cache_key,
            native: options.native,
        }
    }
}

impl From<WireModelOptions> for ModelOptions {
    fn from(options: WireModelOptions) -> Self {
        Self {
            reasoning_effort: options.reasoning_effort.map(Into::into),
            max_output_tokens: options.max_output_tokens,
            cache_key: options.cache_key,
            native: options.native,
        }
    }
}

/// Whether a provider instance consumes a cache key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireCacheKeySupport {
    /// An explicit key is rejected.
    Unsupported,
    /// A key is used when supplied.
    Optional,
}

impl From<CacheKeySupport> for WireCacheKeySupport {
    fn from(support: CacheKeySupport) -> Self {
        match support {
            CacheKeySupport::Unsupported => Self::Unsupported,
            CacheKeySupport::Optional => Self::Optional,
        }
    }
}

impl From<WireCacheKeySupport> for CacheKeySupport {
    fn from(support: WireCacheKeySupport) -> Self {
        match support {
            WireCacheKeySupport::Unsupported => Self::Unsupported,
            WireCacheKeySupport::Optional => Self::Optional,
        }
    }
}

/// What a provider instance actually is; never carries credentials.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireRouteDescription {
    /// The route and model.
    pub origin: WireOrigin,
    /// Whether freeform tool declarations can be carried.
    pub supports_freeform_tools: bool,
    /// Text the route forces in front of the prompt, if any.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub mandatory_prompt_prefix: Option<String>,
    /// Whether the route bills per request.
    pub reports_cost: bool,
    /// Whether this instance consumes a cache key.
    pub cache_key: WireCacheKeySupport,
}

impl From<RouteDescription> for WireRouteDescription {
    fn from(route: RouteDescription) -> Self {
        Self {
            origin: route.origin.into(),
            supports_freeform_tools: route.supports_freeform_tools,
            mandatory_prompt_prefix: route.mandatory_prompt_prefix,
            reports_cost: route.reports_cost,
            cache_key: route.cache_key.into(),
        }
    }
}

impl From<WireRouteDescription> for RouteDescription {
    fn from(route: WireRouteDescription) -> Self {
        Self {
            origin: route.origin.into(),
            supports_freeform_tools: route.supports_freeform_tools,
            mandatory_prompt_prefix: route.mandatory_prompt_prefix,
            reports_cost: route.reports_cost,
            cache_key: route.cache_key.into(),
        }
    }
}

/// Token usage with the provider's fields kept apart. Every field is optional because
/// unknown usage is absent, never zero: a zero would be billed and summed as a fact.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireUsage {
    /// Input tokens not served from cache.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub input_uncached: Option<u64>,
    /// Input tokens read from cache.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_read: Option<u64>,
    /// Input tokens written to cache.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub cache_write: Option<u64>,
    /// Output tokens.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub output: Option<u64>,
    /// Part of `output` spent on reasoning.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub reasoning_output: Option<u64>,
    /// Cost in micro-US-dollars, only where the route reports or prices it.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub cost_micro_usd: Option<u64>,
}

impl From<Usage> for WireUsage {
    fn from(usage: Usage) -> Self {
        Self {
            input_uncached: usage.input_uncached,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            output: usage.output,
            reasoning_output: usage.reasoning_output,
            cost_micro_usd: usage.cost_micro_usd,
        }
    }
}

impl From<WireUsage> for Usage {
    fn from(usage: WireUsage) -> Self {
        Self {
            input_uncached: usage.input_uncached,
            cache_read: usage.cache_read,
            cache_write: usage.cache_write,
            output: usage.output,
            reasoning_output: usage.reasoning_output,
            cost_micro_usd: usage.cost_micro_usd,
        }
    }
}

/// Why a response stopped.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireStopReason {
    /// The model ended its turn.
    EndTurn,
    /// The model called tools.
    ToolUse,
    /// The output limit was reached.
    MaxOutputTokens,
    /// The context window was exceeded.
    ContextWindowExceeded,
    /// The model refused.
    Refusal,
    /// The provider paused a long turn and expects the same history again.
    Paused,
    /// Anything else the route reports.
    Other,
}

impl From<StopReason> for WireStopReason {
    fn from(stop: StopReason) -> Self {
        match stop {
            StopReason::EndTurn => Self::EndTurn,
            StopReason::ToolUse => Self::ToolUse,
            StopReason::MaxOutputTokens => Self::MaxOutputTokens,
            StopReason::ContextWindowExceeded => Self::ContextWindowExceeded,
            StopReason::Refusal => Self::Refusal,
            StopReason::Paused => Self::Paused,
            StopReason::Other => Self::Other,
        }
    }
}

impl From<WireStopReason> for StopReason {
    fn from(stop: WireStopReason) -> Self {
        match stop {
            WireStopReason::EndTurn => Self::EndTurn,
            WireStopReason::ToolUse => Self::ToolUse,
            WireStopReason::MaxOutputTokens => Self::MaxOutputTokens,
            WireStopReason::ContextWindowExceeded => Self::ContextWindowExceeded,
            WireStopReason::Refusal => Self::Refusal,
            WireStopReason::Paused => Self::Paused,
            WireStopReason::Other => Self::Other,
        }
    }
}

/// The closed set of provider error kinds. It is closed because the native retry and
/// fallback logic branches on it: a kind it has never seen would have no retry rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireProviderErrorKind {
    /// Retrying the same request cannot help.
    InvalidRequest,
    /// The credential was refused.
    Authentication,
    /// The account has no balance.
    InsufficientBalance,
    /// The plan does not allow this model on this route.
    NotEntitled,
    /// The usage allowance is used up until the provider's reset.
    UsageLimitExhausted,
    /// Rate limited.
    RateLimited,
    /// The request exceeds the context window.
    ContextWindowExceeded,
    /// Network, server or broken stream; may be retried.
    Transport,
    /// The stream violated the wire protocol; not retried.
    Protocol,
}

impl WireProviderErrorKind {
    /// The wire name, for messages that must name a kind without a serializer.
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::Authentication => "authentication",
            Self::InsufficientBalance => "insufficient_balance",
            Self::NotEntitled => "not_entitled",
            Self::UsageLimitExhausted => "usage_limit_exhausted",
            Self::RateLimited => "rate_limited",
            Self::ContextWindowExceeded => "context_window_exceeded",
            Self::Transport => "transport",
            Self::Protocol => "protocol",
        }
    }
}

impl From<ProviderErrorKind> for WireProviderErrorKind {
    fn from(kind: ProviderErrorKind) -> Self {
        match kind {
            ProviderErrorKind::InvalidRequest => Self::InvalidRequest,
            ProviderErrorKind::Authentication => Self::Authentication,
            ProviderErrorKind::InsufficientBalance => Self::InsufficientBalance,
            ProviderErrorKind::NotEntitled => Self::NotEntitled,
            ProviderErrorKind::UsageLimitExhausted => Self::UsageLimitExhausted,
            ProviderErrorKind::RateLimited => Self::RateLimited,
            ProviderErrorKind::ContextWindowExceeded => Self::ContextWindowExceeded,
            ProviderErrorKind::Transport => Self::Transport,
            ProviderErrorKind::Protocol => Self::Protocol,
        }
    }
}

impl From<WireProviderErrorKind> for ProviderErrorKind {
    fn from(kind: WireProviderErrorKind) -> Self {
        match kind {
            WireProviderErrorKind::InvalidRequest => Self::InvalidRequest,
            WireProviderErrorKind::Authentication => Self::Authentication,
            WireProviderErrorKind::InsufficientBalance => Self::InsufficientBalance,
            WireProviderErrorKind::NotEntitled => Self::NotEntitled,
            WireProviderErrorKind::UsageLimitExhausted => Self::UsageLimitExhausted,
            WireProviderErrorKind::RateLimited => Self::RateLimited,
            WireProviderErrorKind::ContextWindowExceeded => Self::ContextWindowExceeded,
            WireProviderErrorKind::Transport => Self::Transport,
            WireProviderErrorKind::Protocol => Self::Protocol,
        }
    }
}

/// A classified provider failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireProviderError {
    /// The closed kind.
    pub kind: WireProviderErrorKind,
    /// Diagnostic safe to show and log: never a credential or a response body.
    pub message: String,
}

impl From<ProviderError> for WireProviderError {
    fn from(error: ProviderError) -> Self {
        Self {
            kind: error.kind.into(),
            message: error.message,
        }
    }
}

impl From<WireProviderError> for ProviderError {
    fn from(error: WireProviderError) -> Self {
        Self {
            kind: error.kind.into(),
            message: error.message,
        }
    }
}

/// The single terminal outcome of a stream.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireOutcome {
    /// The response completed.
    Completed {
        /// The complete assistant item; the only place tool calls appear.
        item: WireAssistantItem,
        /// Why it stopped.
        stop: WireStopReason,
        /// Absent when the route reported none.
        #[serde(
            default,
            deserialize_with = "refuse_null",
            skip_serializing_if = "Option::is_none"
        )]
        usage: Option<WireUsage>,
    },
    /// The response failed.
    Failed {
        /// The classified error.
        error: WireProviderError,
    },
    /// The response was cancelled. A struct variant so unknown fields are refused here too.
    Cancelled {},
}

impl From<Outcome> for WireOutcome {
    fn from(outcome: Outcome) -> Self {
        match outcome {
            Outcome::Completed(CompletedResponse { item, stop, usage }) => Self::Completed {
                item: item.into(),
                stop: stop.into(),
                usage: usage.map(Into::into),
            },
            Outcome::Failed(error) => Self::Failed {
                error: error.into(),
            },
            Outcome::Cancelled => Self::Cancelled {},
        }
    }
}

impl From<WireOutcome> for Outcome {
    fn from(outcome: WireOutcome) -> Self {
        match outcome {
            WireOutcome::Completed { item, stop, usage } => Self::Completed(CompletedResponse {
                item: item.into(),
                stop: stop.into(),
                usage: usage.map(Into::into),
            }),
            WireOutcome::Failed { error } => Self::Failed(error.into()),
            WireOutcome::Cancelled {} => Self::Cancelled,
        }
    }
}

/// One event of a provider stream; `finished` is the only terminal one.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireStreamEvent {
    /// Incremental assistant text.
    TextDelta {
        /// Index of its block in the final item.
        block: u64,
        /// The increment.
        text: String,
    },
    /// Incremental reasoning text, display only.
    ReasoningDelta {
        /// Index of its block in the final item.
        block: u64,
        /// The increment.
        text: String,
    },
    /// Incremental tool input, display only: never executed.
    ToolInputDelta {
        /// The call being produced.
        call_id: String,
        /// Model-facing tool name.
        name: String,
        /// The increment.
        text: String,
    },
    /// Something the adapter wants the operator to know (ADR-0048); never history.
    Notice {
        /// The adapter's own text.
        text: String,
    },
    /// Progress without content, so a live stream is not taken for a stalled one.
    Activity {},
    /// The terminal event.
    Finished {
        /// How the stream ended.
        outcome: WireOutcome,
    },
}

impl From<StreamEvent> for WireStreamEvent {
    fn from(event: StreamEvent) -> Self {
        match event {
            StreamEvent::TextDelta { block, text } => Self::TextDelta {
                block: to_u64(block),
                text,
            },
            StreamEvent::ReasoningDelta { block, text } => Self::ReasoningDelta {
                block: to_u64(block),
                text,
            },
            StreamEvent::ToolInputDelta {
                call_id,
                name,
                text,
            } => Self::ToolInputDelta {
                call_id,
                name,
                text,
            },
            StreamEvent::Notice { text } => Self::Notice { text },
            StreamEvent::Activity => Self::Activity {},
            StreamEvent::Finished(outcome) => Self::Finished {
                outcome: outcome.into(),
            },
        }
    }
}

impl TryFrom<WireStreamEvent> for StreamEvent {
    type Error = ConversionError;

    fn try_from(event: WireStreamEvent) -> Result<Self, Self::Error> {
        Ok(match event {
            WireStreamEvent::TextDelta { block, text } => Self::TextDelta {
                block: to_usize("text_delta.block", block)?,
                text,
            },
            WireStreamEvent::ReasoningDelta { block, text } => Self::ReasoningDelta {
                block: to_usize("reasoning_delta.block", block)?,
                text,
            },
            WireStreamEvent::ToolInputDelta {
                call_id,
                name,
                text,
            } => Self::ToolInputDelta {
                call_id,
                name,
                text,
            },
            WireStreamEvent::Notice { text } => Self::Notice { text },
            WireStreamEvent::Activity {} => Self::Activity,
            WireStreamEvent::Finished { outcome } => Self::Finished(outcome.into()),
        })
    }
}
