//! Context control — a summarizing [`ContextPolicy`](p1_contracts::ContextPolicy). Specification:
//! `docs/design/context.md` §2. The module keeps no state across calls and
//! summarizes through the ordinary provider interface.
//!
//! The policy's decisions are the [`engine`], which does no I/O and builds without the
//! `native` feature, so the context-policy component (`modules/p1-module-context/`)
//! runs the same code. The `native` feature (on by default) adds `SummarizingContext`,
//! the driver that sends the engine's summary requests through `ProviderSummary`, the
//! native summary operation the component's `summary` import is answered with.

pub mod engine;
mod estimate;
#[cfg(feature = "native")]
mod native;
mod plan;
mod render;
#[cfg(feature = "native")]
mod summary;

pub use estimate::estimate_tokens;
#[cfg(feature = "native")]
pub use native::SummarizingContext;
#[cfg(feature = "native")]
pub use summary::{ProviderSummary, SummaryAnswer, SummaryFailure};

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
    pub(crate) fn wall(&self) -> u64 {
        self.window_tokens
            .saturating_sub(self.output_headroom_tokens)
    }
}
