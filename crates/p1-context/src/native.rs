//! The native driver of the policy engine: [`SummarizingContext`] sends the engine's
//! summary requests through [`ProviderSummary`], awaited on the host.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, CancellationToken, Compaction, ContextError, ContextInput, ContextPolicy, Effort,
    ModelOptions, Prepared, Provider,
};

use crate::engine::{self, Caps, Goal, Step};
use crate::summary::{ProviderSummary, SummaryFailure};
use crate::{ContextConfig, DEFAULT_SUMMARY_OUTPUT_TOKENS};

pub struct SummarizingContext {
    summary: ProviderSummary,
    config: ContextConfig,
    summary_output_tokens: u64,
}

impl SummarizingContext {
    pub fn new(
        provider: Arc<dyn Provider>,
        options: ModelOptions,
        config: ContextConfig,
        prompt: String,
    ) -> Result<Self, String> {
        config.validate()?;
        if prompt.is_empty() {
            return Err("the summarizer prompt must not be empty".to_string());
        }
        Ok(Self {
            summary: ProviderSummary::new(provider, options, prompt),
            config,
            summary_output_tokens: DEFAULT_SUMMARY_OUTPUT_TOKENS,
        })
    }

    /// Sets the cap on one summary's output tokens (context.md "Revision
    /// 2026-09-20"), the `[context] summary_output_tokens` setting. It is a policy
    /// setting rather than a [`ContextConfig`] field because the frozen acceptance
    /// suite builds that struct with full literals. Defaults to
    /// [`DEFAULT_SUMMARY_OUTPUT_TOKENS`]; [`ContextConfig::validate_summary_output_tokens`]
    /// rejects what cannot be sent.
    pub fn with_summary_output_tokens(mut self, tokens: u64) -> Result<Self, String> {
        self.config.validate_summary_output_tokens(tokens)?;
        self.summary_output_tokens = tokens;
        Ok(self)
    }

    /// Sets the reasoning effort the summarization request carries (#125). A summary
    /// must NOT inherit the agent's effort: on a thinking model the route spends part
    /// of the summary-output cap on reasoning before a single summary token, and the
    /// transcript handed to the summarizer is already condensed. The host passes the
    /// LOWEST effort the model profile supports, and `Low` when the environment names
    /// no profile at all; the one cap-doubling retry is untouched. Passing `None` CLEARS
    /// the effort, so the summary request carries no explicit effort and the route's
    /// default applies — the host passes it for a profile that lists no effort levels.
    /// Not calling this at all keeps the effort the options carry.
    pub fn with_summary_effort(mut self, effort: Option<Effort>) -> Self {
        self.summary.options_mut().reasoning_effort = effort;
        self
    }

    fn caps(&self) -> Caps {
        Caps {
            summary_output_tokens: self.summary_output_tokens,
            agent_max_output_tokens: self.summary.options().max_output_tokens,
        }
    }

    /// Runs the engine to its answer, sending each summary request it asks for.
    async fn drive<G: Goal>(&self, mut step: Step<'_, G>, cancel: &CancellationToken) -> G::Output {
        loop {
            let summarization = match step {
                Step::Done(output) => return output,
                Step::Summarize(summarization) => *summarization,
            };
            step = match self
                .summary
                .summarize(summarization.request(), cancel)
                .await
            {
                Ok(answer) => summarization.completed(answer.text, answer.stop, answer.usage),
                Err(SummaryFailure::Refused(error)) => summarization.refused(error.to_string()),
                Err(SummaryFailure::Failed(error)) => summarization.failed(error.to_string()),
                Err(SummaryFailure::Cancelled) => return summarization.cancelled(),
            };
        }
    }
}

impl ContextPolicy for SummarizingContext {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            let step = engine::prepare(&self.config, self.caps(), input.history, input.last_usage);
            self.drive(step, input.cancel).await
        })
    }

    /// Manual compaction (ADR-0076): ONE summary of the current history, made by the
    /// summarization the threshold path runs, whatever `summarize_at_tokens` says; the
    /// rules are [`engine::compact_now`]'s.
    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(async move {
            let step = engine::compact_now(&self.config, self.caps(), input.history);
            self.drive(step, input.cancel).await
        })
    }
}
