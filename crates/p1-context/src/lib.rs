//! Context control — a summarizing [`ContextPolicy`]. Specification:
//! `docs/design/context.md` §2. The module keeps no state across calls and
//! summarizes through the ordinary provider interface.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, ContextError, ContextInput, ContextPolicy, Item, ModelOptions, Prepared, Provider,
};

/// First line of every summary item (context.md "Replacement").
pub const SUMMARY_MARKER: &str = "[p1 context summary v1 — written by the harness from the earlier part of this session. The user's own messages follow verbatim.]";

/// The compiled-in summarizer prompt; an environment may override it with `summarize.md`.
pub const DEFAULT_SUMMARIZER_PROMPT: &str = "";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextConfig {
    /// Capacity of this model on this route.
    pub window_tokens: u64,
    /// Reserved for the next response.
    pub output_headroom_tokens: u64,
    /// The useful point: where summarizing starts. Below `window - headroom`.
    pub summarize_at_tokens: u64,
    /// Newest part of the history kept verbatim.
    pub keep_recent_tokens: u64,
    /// Budget for user messages kept verbatim.
    pub user_verbatim_tokens: u64,
    /// Per tool result, when rendered for the summarizer.
    pub tool_result_excerpt_chars: usize,
}

impl ContextConfig {
    pub fn validate(&self) -> Result<(), String> {
        unimplemented!("p1-context: ContextConfig::validate")
    }
}

/// `ceil(chars / 3.5)` over all model-visible text, tool inputs and replay payloads.
pub fn estimate_tokens(_items: &[Item]) -> u64 {
    unimplemented!("p1-context: estimate_tokens")
}

pub struct SummarizingContext {
    _provider: Arc<dyn Provider>,
    _options: ModelOptions,
    _config: ContextConfig,
    _prompt: String,
}

impl SummarizingContext {
    pub fn new(
        _provider: Arc<dyn Provider>,
        _options: ModelOptions,
        _config: ContextConfig,
        _prompt: String,
    ) -> Result<Self, String> {
        unimplemented!("p1-context: SummarizingContext::new")
    }
}

impl ContextPolicy for SummarizingContext {
    fn prepare<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        unimplemented!("p1-context: SummarizingContext::prepare")
    }
}
