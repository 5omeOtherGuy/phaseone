//! Shared contracts between the p1 agent core and its modules.
//!
//! Everything a provider, tool, policy, journal store or host needs in order to
//! talk to the core — and nothing that names a concrete one. The shapes are driven
//! by the two real routes (`docs/design/routes.md` §C), not by a generic ideal.
//!
//! All public async interfaces are `Send`-capable boxed futures (no `async-trait`
//! dependency, `dyn`-compatible).

pub mod history;
pub mod journal;
pub mod policy;
pub mod provider;
pub mod tool;

pub use history::{
    AssistantBlock, AssistantItem, InboxKind, Item, Origin, ReplayData, ToolCall, ToolInput,
    ToolResultItem, ToolStatus,
};
pub use journal::{CommitError, CommitSink, InterruptionReason, JournalRecord, RecordBody};
pub use policy::{
    AgentEvent, AuthorizationPolicy, AuthorizationRequest, Compaction, ContextError, ContextInput,
    ContextPolicy, Decision, EventSink, Prepared, TurnEnd,
};
pub use provider::{
    CacheKeySupport, CompletedResponse, Effort, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription, StopReason, StreamEvent,
    Usage,
};
pub use tool::{
    CallDescription, DeclarationKind, EditPreview, Effect, Grammar, Tool, ToolContext,
    ToolDeclaration, ToolIdentity, ToolOutcome,
};

/// Re-exported so modules and tests agree on the JSON value type.
pub use serde_json;

/// Cooperative cancellation shared by the core, providers and tools.
pub use tokio_util::sync::CancellationToken;

/// A `Send` boxed future: the one async shape used by every contract trait.
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
