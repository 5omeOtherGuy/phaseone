//! The native driver of the policy engine: [`SummarizingContext`] summarizes through the
//! agent's own provider, awaited on the host.

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Compaction, CompletedResponse, ContextError, ContextInput,
    ContextPolicy, Effort, Item, ModelOptions, Outcome, Prepared, Provider, ProviderRequest,
    StreamEvent,
};

use crate::engine::{self, Caps, Goal, Step, SummaryRequest};
use crate::{ContextConfig, DEFAULT_SUMMARY_OUTPUT_TOKENS};

pub struct SummarizingContext {
    provider: Arc<dyn Provider>,
    options: ModelOptions,
    config: ContextConfig,
    prompt: String,
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
            provider,
            options,
            config,
            prompt,
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
        self.options.reasoning_effort = effort;
        self
    }

    fn caps(&self) -> Caps {
        Caps {
            summary_output_tokens: self.summary_output_tokens,
            agent_max_output_tokens: self.options.max_output_tokens,
        }
    }

    /// Runs the engine to its answer, sending each summary request it asks for.
    async fn drive<G: Goal>(&self, mut step: Step<'_, G>, cancel: &CancellationToken) -> G::Output {
        loop {
            let summarization = match step {
                Step::Done(output) => return output,
                Step::Summarize(summarization) => *summarization,
            };
            let request = self.provider_request(summarization.request());
            if let Err(error) = self.provider.validate(&request) {
                step = summarization.refused(error.to_string());
                continue;
            }
            step = match self.ask(&request, cancel).await {
                Ok(response) => {
                    // Issue #142: the summarizer is a second model whose output becomes a
                    // history item, so it is masked with the same matcher the host wraps
                    // every tool in. A summary can never carry a credential shape into the
                    // history and every later request.
                    let text = p1_redact::redact(&response.item.text()).text;
                    summarization.completed(text, response.stop, response.usage)
                }
                Err(Ask::Cancelled) => return summarization.cancelled(),
                Err(Ask::Failed(reason)) => summarization.failed(reason),
            };
        }
    }

    fn provider_request(&self, request: &SummaryRequest) -> ProviderRequest {
        let mut options = self.options.clone();
        options.max_output_tokens = request.max_output_tokens;
        ProviderRequest {
            system_prompt: self.prompt.clone(),
            history: vec![Item::User {
                text: request.transcript.clone(),
            }],
            tools: Vec::new(),
            options,
        }
    }

    /// One summarization request, streamed to its terminal event. The setup future
    /// itself races `cancel`, exactly like the core's provider call: a cancel before
    /// the stream exists is `Cancelled`.
    async fn ask(
        &self,
        request: &ProviderRequest,
        cancel: &CancellationToken,
    ) -> Result<CompletedResponse, Ask> {
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(Ask::Cancelled),
            result = self.provider.stream(request.clone(), cancel.clone()) => result,
        };
        let mut stream = match stream {
            Ok(stream) => stream,
            Err(error) => return Err(Ask::Failed(error.to_string())),
        };
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(Ask::Cancelled),
                event = stream.next() => event,
            };
            let Some(event) = event else {
                return Err(Ask::Failed(
                    "the summarization stream ended without a terminal event".to_string(),
                ));
            };
            match event {
                StreamEvent::Finished(Outcome::Completed(response)) => return Ok(response),
                StreamEvent::Finished(Outcome::Failed(error)) => {
                    return Err(Ask::Failed(error.to_string()));
                }
                StreamEvent::Finished(Outcome::Cancelled) => return Err(Ask::Cancelled),
                _ => {}
            }
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

/// How one summarization request ended without a completed response.
enum Ask {
    /// The stream failed to start, or reported a failure.
    Failed(String),
    /// The caller cancelled.
    Cancelled,
}
