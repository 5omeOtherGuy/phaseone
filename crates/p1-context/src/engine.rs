//! The summarizing policy's decisions, with no I/O: the one engine both the native
//! [`SummarizingContext`](crate::SummarizingContext) and the context-policy component run.
//!
//! Sending a summary request is the caller's. The engine returns a [`Step`]: either the
//! answer, or a [`Summarization`] that names the request to send and takes what came back.
//! Sans I/O because the two drivers reach the summarizer differently (the native one
//! through a provider it awaits, the component through the host's blocking `summary`
//! import), and the rules must be the same code in both. The summary text a driver feeds
//! back must already be masked: masking is native (issue #142, wit.md "Summaries are
//! masked natively").

use p1_contracts::{Compaction, ContextError, Item, Prepared, StopReason, Usage};

use crate::estimate::estimate_tokens;
use crate::{ContextConfig, SUMMARY_MARKER, plan, render};

/// One summary request as the engine wants it sent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SummaryRequest {
    /// The rendered part of the history to summarize, the request's one user message.
    pub transcript: String,
    /// The output cap; `None` sends no cap.
    pub max_output_tokens: Option<u32>,
}

/// What the engine asks of its driver next.
pub enum Step<'h, G: Goal> {
    /// Send [`Summarization::request`] and feed the result back. Boxed: it carries the
    /// request and the plan, the answer is small.
    Summarize(Box<Summarization<'h, G>>),
    /// The answer to the call that started the engine.
    Done(G::Output),
}

/// What one engine call is for: the threshold path or a manual compaction. Each turns the
/// shared summarization's result into its own answer.
pub trait Goal: Sized {
    type Output;
    #[doc(hidden)]
    fn finish(self, summarized: Summarized) -> Self::Output;
    #[doc(hidden)]
    fn cancelled(self) -> Self::Output;
}

/// The threshold path ([`p1_contracts::ContextPolicy::prepare`]).
pub struct Prepare {
    next_input: u64,
    wall: u64,
}

/// Manual compaction (ADR-0076, [`p1_contracts::ContextPolicy::compact_now`]).
pub struct Compact {
    tokens: u64,
}

/// What one summarization of a history produced.
#[doc(hidden)]
pub enum Summarized {
    /// The summary plus the verbatim tail: the replacement history.
    Replacement(Prepared),
    /// Everything outside the kept tail is already a summary (or there is nothing
    /// outside it): a request could only buy the same summary back.
    NothingToSummarize,
    /// The summary could not be made; the reason names why.
    Failed(String),
}

impl Goal for Prepare {
    type Output = Result<Option<Prepared>, ContextError>;

    fn finish(self, summarized: Summarized) -> Self::Output {
        match summarized {
            Summarized::Replacement(prepared) => Ok(Some(prepared)),
            Summarized::NothingToSummarize => nothing_to_summarize(self.next_input, self.wall),
            Summarized::Failed(reason) => failure(self.next_input, self.wall, reason),
        }
    }

    fn cancelled(self) -> Self::Output {
        Err(ContextError::Cancelled)
    }
}

impl Goal for Compact {
    type Output = Result<Compaction, ContextError>;

    /// A failed summary is an error here, not the threshold path's soft retry: the
    /// operator asked for it.
    fn finish(self, summarized: Summarized) -> Self::Output {
        match summarized {
            Summarized::Replacement(prepared) => {
                let tokens_after = estimate_tokens(&prepared.items);
                Ok(Compaction::Replaced {
                    prepared,
                    tokens_before: self.tokens,
                    tokens_after,
                })
            }
            Summarized::NothingToSummarize => Ok(Compaction::Unchanged {
                tokens: self.tokens,
            }),
            Summarized::Failed(reason) => Err(ContextError::Failed(format!(
                "summarizing failed: {reason}"
            ))),
        }
    }

    fn cancelled(self) -> Self::Output {
        Err(ContextError::Cancelled)
    }
}

/// How the summary requests of one policy are capped.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caps {
    /// The validated summary cap (`[context] summary_output_tokens`).
    pub summary_output_tokens: u64,
    /// The agent's own output limit, if its options carry one: a lower one wins, and it is
    /// what the one retry doubles.
    pub agent_max_output_tokens: Option<u32>,
}

/// The threshold path: `None` below `summarize_at_tokens`, otherwise one summarization.
pub fn prepare<'h>(
    config: &ContextConfig,
    caps: Caps,
    history: &'h [Item],
    last_usage: Option<&Usage>,
) -> Step<'h, Prepare> {
    if history.is_empty() {
        return Step::Done(Ok(None));
    }
    let wall = config.wall();
    let next_input = next_input(history, last_usage);
    if next_input < config.summarize_at_tokens {
        return Step::Done(Ok(None));
    }
    summarize(config, caps, history, Prepare { next_input, wall })
}

/// Manual compaction (ADR-0076): ONE summary of the current history, made by the very
/// summarization the threshold path runs, whatever `summarize_at_tokens` says.
///
/// The no-op rule ("too short"): when no unit lies older than the verbatim tail
/// `keep_recent_tokens` keeps, no request is made and the result is `Unchanged` with the
/// history's estimate. What precedes such a tail is only the prelude — the task message,
/// which the replacement keeps verbatim anyway, and an earlier summary — so a summary
/// could only add to the history. An empty history is the same no-op, and so is the
/// threshold path's own "nothing to summarize".
pub fn compact_now<'h>(
    config: &ContextConfig,
    caps: Caps,
    history: &'h [Item],
) -> Step<'h, Compact> {
    let tokens = estimate_tokens(history);
    let segments = plan::segments(history);
    let tail_start = plan::tail_start(history, &segments, config.keep_recent_tokens);
    if tail_start <= segments.prelude_end {
        return Step::Done(Ok(Compaction::Unchanged { tokens }));
    }
    summarize(config, caps, history, Compact { tokens })
}

/// One summarization in flight: the request to send and what the engine needs to judge
/// the answer.
pub struct Summarization<'h, G: Goal> {
    goal: G,
    history: &'h [Item],
    segments: plan::Segments,
    keep_recent_tokens: u64,
    user_verbatim_tokens: u64,
    summarize_at_tokens: u64,
    request: SummaryRequest,
    /// The cap the engine starts from, and doubles once.
    limit: u32,
    /// Whether this request still carries a cap: the route may refuse one.
    capped: bool,
    /// Completed responses so far.
    attempts: u32,
    usage: Option<Usage>,
}

/// The ONE summarization, shared by the threshold path and the manual one: what the goal
/// does with a failure (soft below the wall, fatal at it, or an error for a manual
/// request) is the goal's.
fn summarize<'h, G: Goal>(
    config: &ContextConfig,
    caps: Caps,
    history: &'h [Item],
    goal: G,
) -> Step<'h, G> {
    let wall = config.wall();
    // What is summarized is everything outside the verbatim tail.
    let segments = plan::segments(history);
    let tail_start = plan::tail_start(history, &segments, config.keep_recent_tokens);

    // "Nothing to summarize": when everything outside the kept tail units is a previous
    // summary, a request could only buy the same summary back (context.md "Nothing to
    // summarize"). Unit-less histories (a lone user message) still count as material.
    let has_material = history.iter().enumerate().any(|(index, item)| {
        !plan::in_tail_unit(index, tail_start, &segments) && !plan::is_summary_item(item)
    });
    if !has_material {
        return Step::Done(goal.finish(Summarized::NothingToSummarize));
    }

    // The cap is this request's output limit and the room the transcript is measured
    // against (context.md "Revision 2026-09-20"). The wire field is a `u32`, so a cap
    // beyond it is clamped.
    let cap = caps.summary_output_tokens;
    let limit = u32::try_from(cap).unwrap_or(u32::MAX);
    let render_budget = wall.saturating_sub(cap);
    let transcript = render::transcript(
        &history[..tail_start],
        config.tool_result_excerpt_chars,
        render_budget,
    );
    Step::Summarize(Box::new(Summarization {
        goal,
        history,
        segments,
        keep_recent_tokens: config.keep_recent_tokens,
        user_verbatim_tokens: config.user_verbatim_tokens,
        summarize_at_tokens: config.summarize_at_tokens,
        request: SummaryRequest {
            transcript,
            max_output_tokens: Some(caps.agent_max_output_tokens.unwrap_or(limit).min(limit)),
        },
        limit,
        capped: true,
        attempts: 0,
        usage: None,
    }))
}

impl<'h, G: Goal> Summarization<'h, G> {
    /// The request to send now.
    pub fn request(&self) -> &SummaryRequest {
        &self.request
    }

    /// The route refused the request before sending it (`reason` is its error).
    ///
    /// The Codex route refuses `max_output_tokens`: the first request is retried once
    /// without it. Any other refusal fails the summarization.
    pub fn refused(mut self, reason: String) -> Step<'h, G> {
        if self.attempts == 0 && self.capped && self.request.max_output_tokens.is_some() {
            self.request.max_output_tokens = None;
            self.capped = false;
            return Step::Summarize(Box::new(self));
        }
        self.fail(reason)
    }

    /// The request was sent and failed, or its stream ended without a terminal event.
    pub fn failed(self, reason: String) -> Step<'h, G> {
        self.fail(reason)
    }

    /// The call was cancelled while the request was in flight.
    pub fn cancelled(self) -> G::Output {
        self.goal.cancelled()
    }

    /// The request completed. `text` is the response's text, already masked.
    ///
    /// A summary whose stop is not `EndTurn` is never accepted. A truncated one
    /// (`MaxOutputTokens`) gets one retry with the cap doubled, and the usage of both
    /// requests is reported.
    pub fn completed(
        mut self,
        text: String,
        stop: StopReason,
        usage: Option<Usage>,
    ) -> Step<'h, G> {
        self.attempts += 1;
        self.usage = if self.attempts == 1 {
            usage
        } else {
            sum_usage(self.usage, usage)
        };
        match stop {
            StopReason::EndTurn => self.accept(text),
            // The cap actually sent, doubled: an agent's own lower limit would otherwise
            // be sent again unchanged.
            StopReason::MaxOutputTokens if self.attempts == 1 && self.capped => {
                let sent = self.request.max_output_tokens.unwrap_or(self.limit);
                self.request.max_output_tokens = Some(sent.saturating_mul(2));
                Step::Summarize(Box::new(self))
            }
            StopReason::MaxOutputTokens if !self.capped => self.fail(
                "the summary was truncated and the route carries no cap to double".to_string(),
            ),
            StopReason::MaxOutputTokens => self.fail("the summary was truncated twice".to_string()),
            stop => self.fail(format!("the summary stopped before the end ({stop:?})")),
        }
    }

    fn fail(self, reason: String) -> Step<'h, G> {
        Step::Done(self.goal.finish(Summarized::Failed(reason)))
    }

    fn accept(self, answer: String) -> Step<'h, G> {
        if answer.is_empty() {
            return self.fail("the summarization produced an empty answer".to_string());
        }
        let history = self.history;
        let segments = &self.segments;
        let summary = Item::User {
            text: format!("{SUMMARY_MARKER}\n{answer}"),
        };
        // The tail is the new history's floor and the spec halves `keep_recent_tokens` no
        // lower than one unit. A one-unit tail cannot shrink, so the replacement is
        // returned as it is instead of looping.
        let mut keep = self.keep_recent_tokens;
        let mut tail_start = plan::tail_start(history, segments, keep);
        let mut items = plan::build_replacement(
            history,
            tail_start,
            self.user_verbatim_tokens,
            summary.clone(),
        );
        let mut halvings = 0;
        while estimate_tokens(&items) >= self.summarize_at_tokens
            && halvings < 3
            && plan::tail_units(segments, tail_start) > 1
        {
            keep /= 2;
            tail_start = plan::tail_start(history, segments, keep);
            items = plan::build_replacement(
                history,
                tail_start,
                self.user_verbatim_tokens,
                summary.clone(),
            );
            halvings += 1;
        }
        if estimate_tokens(&items) >= self.summarize_at_tokens
            && plan::tail_units(segments, tail_start) > 1
        {
            return self.fail("the replacement did not fit below summarize_at_tokens".to_string());
        }
        let usage = self.usage;
        Step::Done(
            self.goal
                .finish(Summarized::Replacement(Prepared { items, usage })),
        )
    }
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
