//! The summarizing context policy as a component (`p1/context/summarizing`): the
//! `context-policy` world of `modules/wit/` over `p1-context`'s engine.
//!
//! Every decision is the engine's, the same code the native `SummarizingContext` runs:
//! the threshold, the wall rule, tail selection, the transcript, the one cap-doubling
//! retry, the retry without a cap and summary acceptance. The component adds only the
//! wire conversions and the one import it needs, `summary.summarize`, which the host
//! answers from a provider it drives outside this component's Store; the summary text
//! arrives masked. Nothing is kept between calls but the settings `configure` stored.
#![forbid(unsafe_code)]

mod settings;
mod wire;

use p1_bindings_context_policy::generated::p1::module::control;
use p1_bindings_context_policy::generated::p1::module::summary::{self, SummaryError};
use p1_bindings_context_policy::generated::{
    Compaction as WitCompaction, ContextError as WitContextError, Guest, HistoryItem, Json,
    Prepared as WitPrepared, Replacement, Usage as WitUsage,
};
use p1_context::engine::{self, Goal, Step};
use p1_contracts::{Compaction, ContextError};

use crate::settings::Settings;

struct SummarizingPolicy;

impl Guest for SummarizingPolicy {
    fn configure(settings: Json) -> Result<(), String> {
        settings::store(Settings::parse(&settings)?)
    }

    fn prepare(
        history: Vec<HistoryItem>,
        last_usage: Option<WitUsage>,
    ) -> Result<Option<WitPrepared>, WitContextError> {
        let settings = settings::get().map_err(WitContextError::Failed)?;
        let history = wire::history(&history).map_err(WitContextError::Failed)?;
        let last_usage = match last_usage {
            Some(text) => Some(wire::usage(&text).map_err(WitContextError::Failed)?),
            None => None,
        };
        let step = engine::prepare(
            &settings.config,
            settings.caps,
            &history,
            last_usage.as_ref(),
        );
        match drive(step) {
            Ok(Some(prepared)) => Ok(Some(wire::prepared(prepared))),
            Ok(None) => Ok(None),
            Err(error) => Err(wit_error(error)),
        }
    }

    fn compact_now(
        history: Vec<HistoryItem>,
        _last_usage: Option<WitUsage>,
    ) -> Result<WitCompaction, WitContextError> {
        let settings = settings::get().map_err(WitContextError::Failed)?;
        let history = wire::history(&history).map_err(WitContextError::Failed)?;
        let step = engine::compact_now(&settings.config, settings.caps, &history);
        match drive(step) {
            Ok(Compaction::Replaced {
                prepared,
                tokens_before,
                tokens_after,
            }) => Ok(WitCompaction::Replaced(Replacement {
                prepared: wire::prepared(prepared),
                tokens_before,
                tokens_after,
            })),
            Ok(Compaction::Unchanged { tokens }) => Ok(WitCompaction::Unchanged(tokens)),
            Err(error) => Err(wit_error(error)),
        }
    }
}

/// Runs the engine to its answer, sending each summary request through the host.
/// `summarize` blocks this component while the host streams; it returns `cancelled` when
/// the call is cancelled, and the engine is asked nothing more then.
fn drive<G: Goal>(mut step: Step<'_, G>) -> G::Output {
    loop {
        let summarization = match step {
            Step::Done(output) => return output,
            Step::Summarize(summarization) => *summarization,
        };
        if control::cancelled() {
            return summarization.cancelled();
        }
        let wanted = summarization.request();
        let request = summary::SummaryRequest {
            transcript: wanted.transcript.clone(),
            max_output_tokens: wanted.max_output_tokens,
        };
        step = match summary::summarize(&request) {
            Ok(response) => match response.usage.as_deref().map(wire::usage).transpose() {
                Ok(usage) => {
                    summarization.completed(response.text, wire::stop(response.stop), usage)
                }
                // The host's own usage text is not the usage family: the summary is not
                // accepted on a guess.
                Err(reason) => summarization.failed(reason),
            },
            Err(SummaryError::Refused(error)) => summarization.refused(wire::error_text(&error)),
            Err(SummaryError::Failed(error)) => summarization.failed(wire::error_text(&error)),
            Err(SummaryError::Cancelled) => return summarization.cancelled(),
        };
    }
}

fn wit_error(error: ContextError) -> WitContextError {
    match error {
        ContextError::Cancelled => WitContextError::Cancelled,
        ContextError::Failed(reason) => WitContextError::Failed(reason),
    }
}

p1_bindings_context_policy::generated::export!(SummarizingPolicy);
