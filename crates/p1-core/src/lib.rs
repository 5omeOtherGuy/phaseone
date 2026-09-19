//! The p1 agent core: one agent's request → stream → tool calls → repeat loop.
//!
//! The core knows only `p1-contracts`. It never names a provider, a tool, a file
//! format, a prompt template or a UI. Behaviour is specified in
//! `docs/design/core.md`; that note is authoritative.

use std::sync::Arc;

use p1_contracts::{
    AuthorizationPolicy, CancellationToken, CommitSink, ContextPolicy, EventSink, InboxKind, Item,
    ModelOptions, Provider, ProviderError, Tool, TurnEnd,
};

/// Everything one agent is assembled from. The agent owns exactly these tools:
/// a tool that is not in `tools` does not exist for it.
pub struct AgentParts {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub options: ModelOptions,
    pub context: Arc<dyn ContextPolicy>,
    pub authorization: Arc<dyn AuthorizationPolicy>,
    pub journal: Arc<dyn CommitSink>,
    pub events: Arc<dyn EventSink>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("two assembled tools share the call name `{0}`")]
    DuplicateToolName(String),
    #[error("the provider rejected this environment: {0}")]
    ProviderRejected(ProviderError),
}

/// Clonable, `Send` handle for delivering messages to a (possibly running) agent.
/// Messages are handed to the model at the next safe boundary.
#[derive(Clone)]
pub struct Inbox {
    _private: (),
}

impl Inbox {
    /// Queue a message. Never blocks. Returns `false` if the agent no longer exists.
    pub fn send(&self, _kind: InboxKind, _text: impl Into<String>) -> bool {
        unimplemented!("p1-core: Inbox::send")
    }
}

/// One agent. Single owner of its state: `run_turn` takes `&mut self`.
pub struct Agent {
    _parts: AgentParts,
}

impl Agent {
    /// Fails before anything runs if the environment is incoherent.
    pub fn new(_parts: AgentParts) -> Result<Self, BuildError> {
        unimplemented!("p1-core: Agent::new")
    }

    pub fn inbox(&self) -> Inbox {
        unimplemented!("p1-core: Agent::inbox")
    }

    /// True if inbox messages are waiting to be delivered.
    pub fn has_pending_inbox(&self) -> bool {
        unimplemented!("p1-core: Agent::has_pending_inbox")
    }

    /// Resolves once at least one inbox message is pending (immediately if one is).
    /// Lets a host sleep until a notification arrives instead of polling.
    pub async fn inbox_ready(&self) {
        unimplemented!("p1-core: Agent::inbox_ready")
    }

    /// Run one turn started by user input.
    pub async fn run_turn(&mut self, _input: String, _cancel: CancellationToken) -> TurnEnd {
        unimplemented!("p1-core: Agent::run_turn")
    }

    /// Run one turn started by pending inbox messages only (no user input).
    /// Returns `None` without doing anything if the inbox is empty.
    pub async fn run_inbox_turn(&mut self, _cancel: CancellationToken) -> Option<TurnEnd> {
        unimplemented!("p1-core: Agent::run_inbox_turn")
    }

    /// The current model-visible history (the journal's projection).
    pub fn history(&self) -> &[Item] {
        unimplemented!("p1-core: Agent::history")
    }
}
