//! Context control — a summarizing [`ContextPolicy`]. Specification:
//! `docs/design/context.md` §2. The module keeps no state across calls and
//! summarizes through the ordinary provider interface.

mod estimate;
mod plan;
mod render;

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Compaction, CompletedResponse, ContextError, ContextInput,
    ContextPolicy, Effort, Item, ModelOptions, Outcome, Prepared, Provider, ProviderRequest,
    StopReason, StreamEvent, Usage,
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

## Files
For every file that was read or changed and still matters: its path and, in a few words each, the symbols and line ranges that matter in it, so the work can continue with ranged reads instead of reading whole files again. Copied forward from a previous summary while the file still matters.

## Verified facts
Commands that were run and their results, and other facts checked against a source, that still matter.

## Open problems
What is unresolved, broken or uncertain, and what was already tried.

## Next step
The single most useful next action.

Do not invent limits, time estimates or instructions that are not in the transcript. If something is not there, leave it out.";

/// Cap on one summary's output tokens, sent as `max_output_tokens` (context.md
/// "Revision 2026-09-20"): the `[context] summary_output_tokens` setting's default,
/// and the reserve the rendered transcript is measured against.
pub const DEFAULT_SUMMARY_OUTPUT_TOKENS: u64 = 4_000;

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
        let wall = self.wall();
        if self.summarize_at_tokens >= wall {
            return Err(format!(
                "summarize_at_tokens ({}) must be below window_tokens - output_headroom_tokens ({wall})",
                self.summarize_at_tokens
            ));
        }
        Ok(())
    }

    /// Rejects a summary output cap that cannot be sent: zero, or so large that it
    /// leaves no room under the wall for the request that carries it.
    pub fn validate_summary_output_tokens(&self, tokens: u64) -> Result<(), String> {
        if tokens == 0 {
            return Err("summary_output_tokens must be greater than zero".to_string());
        }
        let wall = self.wall();
        if tokens >= wall {
            return Err(format!(
                "summary_output_tokens ({tokens}) must be below window_tokens - output_headroom_tokens ({wall})"
            ));
        }
        Ok(())
    }

    /// The wall: the capacity left for the request once the next response is reserved.
    fn wall(&self) -> u64 {
        self.window_tokens
            .saturating_sub(self.output_headroom_tokens)
    }
}

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
            let wall = self.config.wall();
            let next_input = next_input(history, input.last_usage);
            if next_input < self.config.summarize_at_tokens {
                return Ok(None);
            }
            match self.summarize(history, input.cancel).await? {
                Summarized::Replacement(prepared) => Ok(Some(prepared)),
                Summarized::NothingToSummarize => nothing_to_summarize(next_input, wall),
                Summarized::Failed(reason) => failure(next_input, wall, reason),
            }
        })
    }

    /// Manual compaction (ADR-0076): ONE summary of the current history, made by
    /// [`SummarizingContext::summarize`] — the very function the threshold path
    /// calls — whatever `summarize_at_tokens` says.
    ///
    /// The no-op rule ("too short"): when no unit lies older than the verbatim
    /// tail `keep_recent_tokens` keeps, no request is made and the result is
    /// `Unchanged` with the history's estimate. What precedes such a tail is only
    /// the prelude — the task message, which the replacement keeps verbatim
    /// anyway, and an earlier summary — so a summary could only add to the
    /// history. An empty history is the same no-op, and so is the threshold
    /// path's own "nothing to summarize". A failed summary is an error here, not
    /// the threshold path's soft retry: the operator asked for it.
    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(async move {
            let history = input.history;
            let tokens = estimate_tokens(history);
            let segments = plan::segments(history);
            let tail_start = plan::tail_start(history, &segments, self.config.keep_recent_tokens);
            if tail_start <= segments.prelude_end {
                return Ok(Compaction::Unchanged { tokens });
            }
            match self.summarize(history, input.cancel).await? {
                Summarized::Replacement(prepared) => {
                    let tokens_after = estimate_tokens(&prepared.items);
                    Ok(Compaction::Replaced {
                        prepared,
                        tokens_before: tokens,
                        tokens_after,
                    })
                }
                Summarized::NothingToSummarize => Ok(Compaction::Unchanged { tokens }),
                Summarized::Failed(reason) => Err(ContextError::Failed(format!(
                    "summarizing failed: {reason}"
                ))),
            }
        })
    }
}

/// What one summarization of a history produced.
enum Summarized {
    /// The summary plus the verbatim tail: the replacement history.
    Replacement(Prepared),
    /// Everything outside the kept tail is already a summary (or there is nothing
    /// outside it): a request could only buy the same summary back.
    NothingToSummarize,
    /// The summary could not be made; the reason names why.
    Failed(String),
}

impl SummarizingContext {
    /// The ONE summarization, shared by the threshold path ([`ContextPolicy::prepare`])
    /// and the manual one ([`ContextPolicy::compact_now`]): what the caller does
    /// with a failure (soft below the wall, fatal at it, or an error for a manual
    /// request) is the caller's; `Err` is only a cancellation.
    async fn summarize(
        &self,
        history: &[Item],
        cancel: &CancellationToken,
    ) -> Result<Summarized, ContextError> {
        let wall = self.config.wall();
        // What is summarized is everything outside the verbatim tail.
        let segments = plan::segments(history);
        let tail_start = plan::tail_start(history, &segments, self.config.keep_recent_tokens);

        // "Nothing to summarize": when everything outside the kept tail units
        // is a previous summary, a request could only buy the same summary back
        // (context.md "Nothing to summarize"). Unit-less histories (a lone user
        // message) still count as material.
        let has_material = history.iter().enumerate().any(|(index, item)| {
            !plan::in_tail_unit(index, tail_start, &segments) && !plan::is_summary_item(item)
        });
        if !has_material {
            return Ok(Summarized::NothingToSummarize);
        }

        // The cap is this request's output limit and the room the transcript is
        // measured against (context.md "Revision 2026-09-20"). The wire field is
        // a `u32`, so a cap beyond it is clamped.
        let cap = self.summary_output_tokens;
        let limit = u32::try_from(cap).unwrap_or(u32::MAX);
        let render_budget = wall.saturating_sub(cap);
        let rendered = render::transcript(
            &history[..tail_start],
            self.config.tool_result_excerpt_chars,
            render_budget,
        );

        let mut options = self.options.clone();
        options.max_output_tokens = Some(options.max_output_tokens.unwrap_or(limit).min(limit));
        let mut request = ProviderRequest {
            system_prompt: self.prompt.clone(),
            history: vec![Item::User { text: rendered }],
            tools: Vec::new(),
            options,
        };
        // Whether this request still carries a cap: the route below may refuse it.
        let mut capped = true;
        if let Err(first) = self.provider.validate(&request) {
            if request.options.max_output_tokens.is_none() {
                return Ok(Summarized::Failed(first.to_string()));
            }
            // The Codex route refuses `max_output_tokens`: retry once without it.
            request.options.max_output_tokens = None;
            capped = false;
            if let Err(second) = self.provider.validate(&request) {
                return Ok(Summarized::Failed(second.to_string()));
            }
        }

        // A summary whose stop is not `EndTurn` is never accepted. A truncated
        // one (`MaxOutputTokens`) gets one retry with the cap doubled, and the
        // usage of both requests is reported.
        let mut usage: Option<Usage> = None;
        let mut attempts = 0u32;
        let answer = loop {
            attempts += 1;
            let response = match self.ask(&request, cancel).await {
                Ok(response) => response,
                Err(Ask::Cancelled) => return Err(ContextError::Cancelled),
                Err(Ask::Failed(reason)) => return Ok(Summarized::Failed(reason)),
            };
            usage = if attempts == 1 {
                response.usage
            } else {
                sum_usage(usage, response.usage)
            };
            match response.stop {
                StopReason::EndTurn => break response.item.text(),
                // The cap actually sent, doubled: an agent's own lower limit
                // would otherwise be sent again unchanged.
                StopReason::MaxOutputTokens if attempts == 1 && capped => {
                    let sent = request.options.max_output_tokens.unwrap_or(limit);
                    request.options.max_output_tokens = Some(sent.saturating_mul(2));
                    if let Err(error) = self.provider.validate(&request) {
                        return Ok(Summarized::Failed(error.to_string()));
                    }
                }
                StopReason::MaxOutputTokens if !capped => {
                    return Ok(Summarized::Failed(
                        "the summary was truncated and the route carries no cap to double"
                            .to_string(),
                    ));
                }
                StopReason::MaxOutputTokens => {
                    return Ok(Summarized::Failed(
                        "the summary was truncated twice".to_string(),
                    ));
                }
                stop => {
                    return Ok(Summarized::Failed(format!(
                        "the summary stopped before the end ({stop:?})"
                    )));
                }
            }
        };
        if answer.is_empty() {
            return Ok(Summarized::Failed(
                "the summarization produced an empty answer".to_string(),
            ));
        }

        // Issue #142: the summarizer is a second model whose output becomes a
        // history item, so it is masked with the same matcher the host wraps
        // every tool in. A summary can never carry a credential shape into the
        // history and every later request.
        let answer = p1_redact::redact(&answer).text;
        let summary = Item::User {
            text: format!("{SUMMARY_MARKER}\n{answer}"),
        };
        // The tail is the new history's floor and the spec halves
        // `keep_recent_tokens` no lower than one unit. A one-unit tail cannot
        // shrink, so the replacement is returned as it is instead of looping.
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
            return Ok(Summarized::Failed(
                "the replacement did not fit below summarize_at_tokens".to_string(),
            ));
        }
        Ok(Summarized::Replacement(Prepared { items, usage }))
    }
}

impl SummarizingContext {
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

/// How one summarization request ended without a completed response.
enum Ask {
    /// The stream failed to start, or reported a failure.
    Failed(String),
    /// The caller cancelled.
    Cancelled,
}

/// The usage of the two requests of a retried summarization, summed per part. A part
/// is summed only when BOTH requests reported it: when either one left it unknown the
/// sum is unknown too, because a sum over an unknown part would state a number the
/// route never reported (context.md, "unknown is not zero" — the same rule the
/// estimator's `remaining` follows). Whole usage is unknown when either request
/// reported none. A summarization that needed one request keeps that request's usage
/// as it is (the caller sums only from the second attempt on).
fn sum_usage(first: Option<Usage>, second: Option<Usage>) -> Option<Usage> {
    let (a, b) = (first?, second?);
    let part = |x: Option<u64>, y: Option<u64>| match (x, y) {
        (Some(x), Some(y)) => Some(x + y),
        _ => None,
    };
    Some(Usage {
        input_uncached: part(a.input_uncached, b.input_uncached),
        cache_read: part(a.cache_read, b.cache_read),
        cache_write: part(a.cache_write, b.cache_write),
        output: part(a.output, b.output),
        reasoning_output: part(a.reasoning_output, b.reasoning_output),
        cost_micro_usd: part(a.cost_micro_usd, b.cost_micro_usd),
    })
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

/// The "nothing to summarize" outcome: no request is made, and only a full
/// context is a hard failure.
fn nothing_to_summarize(next_input: u64, wall: u64) -> Result<Option<Prepared>, ContextError> {
    if next_input < wall {
        Ok(None)
    } else {
        Err(ContextError::Failed(format!(
            "context is full ({next_input} of {wall} tokens) and nothing is left to summarize"
        )))
    }
}
