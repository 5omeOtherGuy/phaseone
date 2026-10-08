//! The host's summarizing context (context.md §3, ADR-0071, ADR-0084 §1): the shipped
//! context-policy component `p1/context/summarizing`, an official-release host entry
//! (D083b 2), behind `p1_module_runtime::WasmContextPolicy`, with its `summary` import
//! answered by [`HostSummary`].
//!
//! The policy's decisions (the minimum-effort selection, the one cap-doubling retry, failure
//! below versus at the wall, the replacement it builds) are the component's; the core still
//! validates every replacement before it commits `ContextReplaced`. The host keeps what only
//! it knows: the effective context table ([`ContextTable`], folded from the environment and
//! the selected profile in `run.rs`), the summary-output cap, the summary's effort and prompt,
//! and the one native summary operation, which sends through the agent's own provider.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Compaction, ContextError, ContextInput, ContextPolicy, Effort,
    Item, ModelOptions, Outcome, Prepared, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest, StreamEvent,
};
use p1_module_runtime::{
    ExecutionLimits, SummaryError, SummaryRequest, SummaryResponse, SummaryService,
    WasmContextPolicy,
};

/// The manifest name of the shipped summarizing context policy.
pub const CONTEXT_POLICY: &str = "p1/context/summarizing";

/// The compiled-in summarizer prompt; an environment may override it with `summarize.md`.
/// The same text as the component's source crate states (`p1_context`), which a test holds
/// it to.
pub const DEFAULT_SUMMARIZER_PROMPT: &str = "\
Write a durable summary of the earlier part of this coding session. Reply with exactly the sections below, in this order, and keep each one short.

## Task
One or two sentences on what the user is ultimately trying to achieve.

## Constraints and instructions
Every rule the user or the repository imposed that still applies, copied forward from a previous summary and never dropped unless the user explicitly revoked it. Invent nothing: report only limits, time estimates and instructions that are in the transcript.

## Decisions
What was decided and why, including decisions carried forward from a previous summary; never drop one unless it was reversed.

## State of the work
What is done, what is in progress and what has not started, with the file paths involved. Include the working conclusions and candidate findings the assistant reached, including those that appear only in its reasoning.

## Files
For every file that was read or changed and still matters: its path and, in a few words each, the symbols and line ranges that matter in it, so the work can continue with ranged reads instead of reading whole files again. Copied forward from a previous summary while the file still matters.

## Verified facts
Commands that were run and their results, and other facts checked against a source, that still matter.

## Open problems
What is unresolved, broken or uncertain, and what was already tried.

## Next step
The single most useful next action.

Do not invent limits, time estimates or instructions that are not in the transcript. If something is not there, leave it out.";

/// The effective context table one agent gets: the component's `configure` keys, with the
/// selected profile's own capacity already folded in (`run.rs`, `config_for_route`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContextTable {
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
    /// Per reasoning block, when rendered for the summarizer; 0 omits reasoning.
    pub reasoning_excerpt_chars: usize,
    /// Where old tool results start being shortened (ADR-0127); `None` never trims.
    pub trim_at_tokens: Option<u64>,
}

/// Which context replacement was a trim of old tool results (ADR-0127), for the §3c
/// idle-summary count: a trim is routine above `trim_at_tokens` and not a summary. The
/// agent's policy (wrapped by [`trim_aware`]) sets it on every preparation and clears it on
/// a manual compaction; the guard reads it once, at the `ContextReplaced` that follows.
#[derive(Default)]
pub(crate) struct TrimSignal(AtomicBool);

impl TrimSignal {
    /// Records whether the replacement just returned is a trim.
    pub(crate) fn note(&self, trim: bool) {
        self.0.store(trim, Ordering::SeqCst);
    }

    /// Whether the replacement returned last was a trim; reading clears it.
    pub(crate) fn take(&self) -> bool {
        self.0.swap(false, Ordering::SeqCst)
    }
}

/// `inner`, reporting through `signal` whether each replacement it returns is a trim.
pub(crate) fn trim_aware(
    inner: Arc<dyn ContextPolicy>,
    signal: Arc<TrimSignal>,
) -> Arc<dyn ContextPolicy> {
    Arc::new(TrimAware { inner, signal })
}

struct TrimAware {
    inner: Arc<dyn ContextPolicy>,
    signal: Arc<TrimSignal>,
}

impl ContextPolicy for TrimAware {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        let history = input.history;
        Box::pin(async move {
            let result = self.inner.prepare(input).await;
            self.signal
                .note(matches!(&result, Ok(Some(prepared)) if is_trim(history, &prepared.items)));
            result
        })
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        self.signal.note(false);
        self.inner.compact_now(input)
    }
}

/// A trim keeps every item in place and changes only the content of some tool results. A
/// summary keeps every result it keeps byte-exact, and a call id names one result, so a result
/// with the same call id at the same index and other content can only come from a trim.
fn is_trim(before: &[Item], after: &[Item]) -> bool {
    before.len() == after.len()
        && before.iter().zip(after).any(|pair| {
            matches!(pair, (Item::ToolResult(old), Item::ToolResult(new))
                if old.call_id == new.call_id && old.content != new.content)
        })
}

/// The settings `configure` takes: the table, the summary-output cap and, when the agent's
/// options carry one, its own output limit. The component validates them as the native
/// policy did; a refusal is an assembly error.
fn settings(table: &ContextTable, summary_output_tokens: u64, options: &ModelOptions) -> String {
    let mut settings = serde_json::json!({
        "window_tokens": table.window_tokens,
        "output_headroom_tokens": table.output_headroom_tokens,
        "summarize_at_tokens": table.summarize_at_tokens,
        "keep_recent_tokens": table.keep_recent_tokens,
        "user_verbatim_tokens": table.user_verbatim_tokens,
        "tool_result_excerpt_chars": table.tool_result_excerpt_chars,
        "reasoning_excerpt_chars": table.reasoning_excerpt_chars,
        "summary_output_tokens": summary_output_tokens,
    });
    if let Some(trim) = table.trim_at_tokens {
        settings["trim_at_tokens"] = serde_json::json!(trim);
    }
    if let Some(cap) = options.max_output_tokens {
        settings["max_output_tokens"] = serde_json::json!(cap);
    }
    settings.to_string()
}

/// The summarizing context policy of one agent: the host entry [`CONTEXT_POLICY`], configured
/// with `table` and `summary_output_tokens`, summarizing through `provider` with `options` at
/// `effort` (`None` clears the effort, so the route's default applies) under `prompt`. Must be
/// called inside a Tokio runtime, which runs the policy's executor.
#[cfg(test)]
pub(crate) fn summarizing_context(
    provider: Arc<dyn Provider>,
    options: ModelOptions,
    table: &ContextTable,
    summary_output_tokens: u64,
    prompt: String,
    effort: Option<Effort>,
) -> Result<WasmContextPolicy, String> {
    if prompt.is_empty() {
        return Err("the summarizer prompt must not be empty".to_string());
    }
    let module = crate::policy::host_entry(CONTEXT_POLICY)?;
    summarizing_context_from_module(
        module,
        provider,
        options,
        table,
        summary_output_tokens,
        prompt,
        effort,
    )
}

/// Compose the context policy from the particular verified package recorded by the host.
pub(crate) fn summarizing_context_from_module(
    module: Arc<p1_module_runtime::LoadedModule>,
    provider: Arc<dyn Provider>,
    options: ModelOptions,
    table: &ContextTable,
    summary_output_tokens: u64,
    prompt: String,
    effort: Option<Effort>,
) -> Result<WasmContextPolicy, String> {
    if prompt.is_empty() {
        return Err("the summarizer prompt must not be empty".to_string());
    }
    let settings = settings(table, summary_output_tokens, &options);
    let mut options = options;
    options.reasoning_effort = effort;
    let service = Arc::new(HostSummary {
        provider,
        options,
        prompt,
    });
    WasmContextPolicy::new(&module, &settings, service, ExecutionLimits::default())
        .map_err(|error| error.to_string())
}

/// The native summary operation the component's `summary` import is linked to: one request
/// through the agent's own provider, under the summarizer prompt, at the summary's effort,
/// streamed to its terminal event. It holds no policy state and never calls back into a
/// policy, so a component waiting on it is never re-entered.
pub struct HostSummary {
    provider: Arc<dyn Provider>,
    options: ModelOptions,
    prompt: String,
}

impl HostSummary {
    /// The request: the transcript as the one user item, the cap exactly as given.
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

impl SummaryService for HostSummary {
    fn summarize(
        &self,
        request: SummaryRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>> {
        Box::pin(async move {
            let request = self.provider_request(&request);
            self.provider
                .validate(&request)
                .map_err(SummaryError::Refused)?;
            // The setup future itself races `cancel`, exactly like the core's provider call:
            // a cancel before the stream exists is `Cancelled`.
            let stream = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err(SummaryError::Cancelled),
                result = self.provider.stream(request, cancel.clone()) => result,
            };
            let mut stream = stream.map_err(SummaryError::Failed)?;
            loop {
                let event = tokio::select! {
                    biased;
                    _ = cancel.cancelled() => return Err(SummaryError::Cancelled),
                    event = stream.next() => event,
                };
                let Some(event) = event else {
                    return Err(SummaryError::Failed(ProviderError::new(
                        ProviderErrorKind::Transport,
                        "the summarization stream ended without a terminal event",
                    )));
                };
                match event {
                    // Issue #142: the summarizer's output becomes a history item, so it is
                    // masked with the matcher the host wraps every tool in (the adapter masks
                    // it again before the guest sees it).
                    StreamEvent::Finished(Outcome::Completed(response)) => {
                        return Ok(SummaryResponse {
                            text: p1_redact::redact(&response.item.text()).text,
                            stop: response.stop,
                            usage: response.usage,
                        });
                    }
                    StreamEvent::Finished(Outcome::Failed(error)) => {
                        return Err(SummaryError::Failed(error));
                    }
                    StreamEvent::Finished(Outcome::Cancelled) => {
                        return Err(SummaryError::Cancelled);
                    }
                    _ => {}
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_prompt_is_the_component_sources_prompt() {
        assert_eq!(
            DEFAULT_SUMMARIZER_PROMPT,
            p1_context::DEFAULT_SUMMARIZER_PROMPT
        );
    }

    #[test]
    fn the_settings_carry_the_table_the_cap_and_the_agents_own_limit() {
        let table = ContextTable {
            window_tokens: 10_000,
            output_headroom_tokens: 1_000,
            summarize_at_tokens: 100,
            keep_recent_tokens: 80,
            user_verbatim_tokens: 50,
            tool_result_excerpt_chars: 2_000,
            reasoning_excerpt_chars: 4_000,
            trim_at_tokens: None,
        };
        let without: serde_json::Value =
            serde_json::from_str(&settings(&table, 4_000, &ModelOptions::default())).unwrap();
        assert_eq!(
            without,
            serde_json::json!({
                "window_tokens": 10_000,
                "output_headroom_tokens": 1_000,
                "summarize_at_tokens": 100,
                "keep_recent_tokens": 80,
                "user_verbatim_tokens": 50,
                "tool_result_excerpt_chars": 2_000,
                "reasoning_excerpt_chars": 4_000,
                "summary_output_tokens": 4_000,
            })
        );
        let options = ModelOptions {
            max_output_tokens: Some(8_000),
            ..ModelOptions::default()
        };
        let with: serde_json::Value =
            serde_json::from_str(&settings(&table, 4_000, &options)).unwrap();
        assert_eq!(with["max_output_tokens"], 8_000);
        // ADR-0127: the trim threshold is sent only when the table has one.
        assert!(with.get("trim_at_tokens").is_none());
        let trimming = ContextTable {
            trim_at_tokens: Some(60),
            ..table
        };
        let trimmed: serde_json::Value =
            serde_json::from_str(&settings(&trimming, 4_000, &ModelOptions::default())).unwrap();
        assert_eq!(trimmed["trim_at_tokens"], 60);
    }

    /// ADR-0127: only a replacement that changed a kept result in place is a trim; a summary
    /// whose replacement happens to have the history's length, or to equal it, is not.
    #[test]
    fn a_trim_is_told_from_a_summary_by_a_result_changed_in_place() {
        let result = |content: &str| {
            Item::ToolResult(p1_contracts::ToolResultItem {
                call_id: "c1".into(),
                name: "read".into(),
                status: p1_contracts::ToolStatus::Ok,
                content: content.into(),
            })
        };
        let user = |text: &str| Item::User { text: text.into() };
        let before = vec![user("task"), result("long output")];
        assert!(is_trim(&before, &[user("task"), result("short")]));
        assert!(!is_trim(&before, &before));
        assert!(!is_trim(&before, &[user("summary"), result("long output")]));
        assert!(!is_trim(&before, &[result("short")]));
    }
}
