//! The native summary operation: one summarization request through the agent's own
//! provider. It is what the host answers the context-policy component's `summary` import
//! with (the `summary` interface of `modules/wit/session.wit`), and what
//! [`SummarizingContext`](crate::SummarizingContext) sends its own requests through, so
//! both drivers of the engine summarize the same way.
//!
//! It only sends: it holds no policy state and never calls back into a policy, so a
//! component waiting on it is never re-entered and no `prepare` runs inside it.

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    CancellationToken, Item, ModelOptions, Outcome, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest, StopReason, StreamEvent, Usage,
};

use crate::engine::SummaryRequest;

/// The agent's provider, its options at the summary effort, and the summarizer prompt.
pub struct ProviderSummary {
    provider: Arc<dyn Provider>,
    options: ModelOptions,
    prompt: String,
}

/// A completed summary response.
#[derive(Debug, Clone, PartialEq)]
pub struct SummaryAnswer {
    /// The response's text, masked.
    pub text: String,
    pub stop: StopReason,
    /// Unknown usage is `None`, never zero.
    pub usage: Option<Usage>,
}

/// Why no summary response came back.
#[derive(Debug, Clone, PartialEq)]
pub enum SummaryFailure {
    /// The route refused the request before anything was sent.
    Refused(ProviderError),
    /// The request was sent and failed.
    Failed(ProviderError),
    /// The call was cancelled while the request was in flight.
    Cancelled,
}

impl ProviderSummary {
    pub fn new(provider: Arc<dyn Provider>, options: ModelOptions, prompt: String) -> Self {
        Self {
            provider,
            options,
            prompt,
        }
    }

    pub(crate) fn options_mut(&mut self) -> &mut ModelOptions {
        &mut self.options
    }

    pub(crate) fn options(&self) -> &ModelOptions {
        &self.options
    }

    /// Sends `request` as the one user item under the summarizer prompt, with the cap it
    /// names (`None` sends none), and streams the response to its terminal event.
    pub async fn summarize(
        &self,
        request: &SummaryRequest,
        cancel: &CancellationToken,
    ) -> Result<SummaryAnswer, SummaryFailure> {
        let request = self.provider_request(request);
        self.provider
            .validate(&request)
            .map_err(SummaryFailure::Refused)?;
        // The setup future itself races `cancel`, exactly like the core's provider call:
        // a cancel before the stream exists is `Cancelled`.
        let stream = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(SummaryFailure::Cancelled),
            result = self.provider.stream(request, cancel.clone()) => result,
        };
        let mut stream = stream.map_err(SummaryFailure::Failed)?;
        loop {
            let event = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(SummaryFailure::Cancelled),
                event = stream.next() => event,
            };
            let Some(event) = event else {
                return Err(SummaryFailure::Failed(ProviderError::new(
                    ProviderErrorKind::Transport,
                    "the summarization stream ended without a terminal event",
                )));
            };
            match event {
                StreamEvent::Finished(Outcome::Completed(response)) => {
                    // Issue #142: the summarizer is a second model whose output becomes a
                    // history item, so it is masked with the same matcher the host wraps
                    // every tool in. A summary can never carry a credential shape into the
                    // history and every later request.
                    return Ok(SummaryAnswer {
                        text: p1_redact::redact(&response.item.text()).text,
                        stop: response.stop,
                        usage: response.usage,
                    });
                }
                StreamEvent::Finished(Outcome::Failed(error)) => {
                    return Err(SummaryFailure::Failed(error));
                }
                StreamEvent::Finished(Outcome::Cancelled) => {
                    return Err(SummaryFailure::Cancelled);
                }
                _ => {}
            }
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
}
