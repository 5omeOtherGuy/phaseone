//! Context control — a summarizing [`ContextPolicy`]. Specification:
//! `docs/design/context.md` §2. The module keeps no state across calls and
//! summarizes through the ordinary provider interface.

mod estimate;
mod plan;
mod render;

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, ContextError, ContextInput, ContextPolicy, Item, ModelOptions, Outcome, Prepared,
    Provider, ProviderRequest, StreamEvent, Usage,
};

pub use estimate::estimate_tokens;

/// First line of every summary item (context.md "Replacement").
pub const SUMMARY_MARKER: &str = "[p1 context summary v1 — written by the harness from the earlier part of this session. The user's own messages follow verbatim.]";

/// The compiled-in summarizer prompt; an environment may override it with `summarize.md`.
pub const DEFAULT_SUMMARIZER_PROMPT: &str = "\
Write a durable summary of the earlier part of this coding session. Reply with exactly the sections below, in this order, and keep each one short.

## Task
One or two sentences on what the user is ultimately trying to achieve.

## Constraints and instructions
Every rule the user or the repository imposed that still applies, copied forward from a previous summary and never dropped unless the user explicitly revoked it. Invent nothing: report only limits, time estimates and instructions that are in the transcript.

## Decisions
What was decided and why, including decisions carried forward from a previous summary; never drop one unless it was reversed.

## State of the work
What is done, what is in progress and what has not started, with the file paths involved.

## Verified facts
Commands that were run and their results, and other facts checked against a source, that still matter.

## Open problems
What is unresolved, broken or uncertain, and what was already tried.

## Next step
The single most useful next action.

Do not invent limits, time estimates or instructions that are not in the transcript. If something is not there, leave it out.";

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
        if self.window_tokens == 0 {
            return Err("window_tokens must be greater than zero".to_string());
        }
        let wall = self
            .window_tokens
            .saturating_sub(self.output_headroom_tokens);
        if self.summarize_at_tokens >= wall {
            return Err(format!(
                "summarize_at_tokens ({}) must be below window_tokens - output_headroom_tokens ({wall})",
                self.summarize_at_tokens
            ));
        }
        Ok(())
    }
}

pub struct SummarizingContext {
    provider: Arc<dyn Provider>,
    options: ModelOptions,
    config: ContextConfig,
    prompt: String,
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
        })
    }
}

impl ContextPolicy for SummarizingContext {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            let history = input.history;
            if history.is_empty() {
                return Ok(None);
            }
            let wall = self
                .config
                .window_tokens
                .saturating_sub(self.config.output_headroom_tokens);
            let next_input = next_input(history, input.last_usage);
            if next_input < self.config.summarize_at_tokens {
                return Ok(None);
            }

            // What is summarized is everything outside the verbatim tail.
            let segments = plan::segments(history);
            let tail_start = plan::tail_start(history, &segments, self.config.keep_recent_tokens);
            let render_budget = wall.saturating_sub(4_000);
            let rendered = render::transcript(
                &history[..tail_start],
                self.config.tool_result_excerpt_chars,
                render_budget,
            );

            let mut options = self.options.clone();
            options.max_output_tokens = Some(options.max_output_tokens.unwrap_or(4_000).min(4_000));
            let mut request = ProviderRequest {
                system_prompt: self.prompt.clone(),
                history: vec![Item::User { text: rendered }],
                tools: Vec::new(),
                options,
            };
            if let Err(first) = self.provider.validate(&request) {
                if request.options.max_output_tokens.is_none() {
                    return failure(next_input, wall, first.to_string());
                }
                // The Codex route refuses `max_output_tokens`: retry once without it.
                request.options.max_output_tokens = None;
                if let Err(second) = self.provider.validate(&request) {
                    return failure(next_input, wall, second.to_string());
                }
            }

            // The setup future itself races `cancel`, exactly like the core's
            // provider call: a cancel before the stream exists is `Cancelled`.
            let stream = tokio::select! {
                biased;
                _ = input.cancel.cancelled() => return Err(ContextError::Cancelled),
                result = self.provider.stream(request, input.cancel.clone()) => result,
            };
            let mut stream = match stream {
                Ok(stream) => stream,
                Err(error) => return failure(next_input, wall, error.to_string()),
            };
            let (answer, usage) = loop {
                let event = tokio::select! {
                    biased;
                    _ = input.cancel.cancelled() => return Err(ContextError::Cancelled),
                    event = stream.next() => event,
                };
                let Some(event) = event else {
                    return failure(
                        next_input,
                        wall,
                        "the summarization stream ended without a terminal event".to_string(),
                    );
                };
                match event {
                    StreamEvent::Finished(Outcome::Completed(response)) => {
                        break (response.item.text(), response.usage);
                    }
                    StreamEvent::Finished(Outcome::Failed(error)) => {
                        return failure(next_input, wall, error.to_string());
                    }
                    StreamEvent::Finished(Outcome::Cancelled) => {
                        return Err(ContextError::Cancelled);
                    }
                    _ => {}
                }
            };
            if answer.is_empty() {
                return failure(
                    next_input,
                    wall,
                    "the summarization produced an empty answer".to_string(),
                );
            }

            let summary = Item::User {
                text: format!("{SUMMARY_MARKER}\n{answer}"),
            };
            // The tail is the new history's floor. Shrink it while the estimate is
            // still at or above the useful point; a one-unit tail cannot shrink, so
            // that is accepted as the best possible replacement.
            let mut keep = self.config.keep_recent_tokens;
            let mut tail_start = plan::tail_start(history, &segments, keep);
            let mut items = plan::build_replacement(
                history,
                tail_start,
                self.config.user_verbatim_tokens,
                summary.clone(),
            );
            let mut halvings = 0;
            while estimate_tokens(&items) >= self.config.summarize_at_tokens
                && halvings < 3
                && plan::tail_units(&segments, tail_start) > 1
            {
                keep /= 2;
                tail_start = plan::tail_start(history, &segments, keep);
                items = plan::build_replacement(
                    history,
                    tail_start,
                    self.config.user_verbatim_tokens,
                    summary.clone(),
                );
                halvings += 1;
            }
            if estimate_tokens(&items) >= self.config.summarize_at_tokens
                && plan::tail_units(&segments, tail_start) > 1
            {
                return failure(
                    next_input,
                    wall,
                    "the replacement did not fit below summarize_at_tokens".to_string(),
                );
            }
            Ok(Some(Prepared { items, usage }))
        })
    }
}

/// `known` usage plus the estimate of everything after the last assistant; the
/// whole-history estimate when last usage is missing what it takes to be known.
/// The cache parts may be absent (they count as 0); `input` and `output` may not.
fn next_input(history: &[Item], last_usage: Option<&Usage>) -> u64 {
    let Some(usage) = last_usage else {
        return estimate_tokens(history);
    };
    let (Some(uncached), Some(output)) = (usage.input_uncached, usage.output) else {
        return estimate_tokens(history);
    };
    let known = uncached + usage.cache_read.unwrap_or(0) + usage.cache_write.unwrap_or(0) + output;
    let added = history
        .iter()
        .rposition(|item| matches!(item, Item::Assistant(_)))
        .map(|index| &history[index + 1..])
        .unwrap_or(history);
    known + estimate_tokens(added)
}

/// A failed preparation below the wall is soft (`Ok(None)`): the turn continues
/// with the full history and tries again. At the wall it is fatal and names both
/// numbers and the reason.
fn failure(next_input: u64, wall: u64, reason: String) -> Result<Option<Prepared>, ContextError> {
    if next_input < wall {
        Ok(None)
    } else {
        Err(ContextError::Failed(format!(
            "context is full ({next_input} of {wall} tokens) and summarizing failed: {reason}"
        )))
    }
}
