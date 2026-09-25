//! The summary-output cap of `docs/design/context.md` "Revision 2026-09-20",
//! driven through the public API with the scripted provider: the cap that is sent,
//! the one retry with it doubled, the stops that are never accepted, and the
//! `## Files` section the default prompt gained.

use std::sync::Arc;
use std::time::Duration;

use p1_context::{ContextConfig, DEFAULT_SUMMARIZER_PROMPT, SummarizingContext, estimate_tokens};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, ContextError, ContextInput,
    ContextPolicy, InboxKind, Item, ModelOptions, Prepared, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RecordBody, RouteDescription, StopReason,
    StreamEvent, TurnEnd, Usage,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider, Step, completed,
    origin, text_block, text_response,
};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

/// The compiled-in cap, spelled out: the tests below pin the wire value.
const DEFAULT_CAP: u32 = 4_000;

fn config() -> ContextConfig {
    ContextConfig {
        window_tokens: 10_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: 500,
        keep_recent_tokens: 80,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

/// A config that summarizes as soon as the history is bigger than one token, with
/// a wall far above the cap so failures are soft.
fn force_config(history: &[Item]) -> ContextConfig {
    let total = estimate_tokens(history);
    ContextConfig {
        window_tokens: total + 5_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: total.max(2) - 1,
        keep_recent_tokens: 20,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

fn user(text: impl Into<String>) -> Item {
    Item::User { text: text.into() }
}

fn assistant_text(text: impl Into<String>) -> Item {
    Item::Assistant(AssistantItem {
        origin: origin(),
        blocks: vec![AssistantBlock::Text { text: text.into() }],
    })
}

/// A summary request that hit the output cap: text, then `MaxOutputTokens`.
fn truncated(usage: Option<Usage>) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "cut off".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("cut off")],
            StopReason::MaxOutputTokens,
            usage,
        )),
    ])
}

/// A completed response that stopped for another reason.
fn stopped(stop: StopReason, usage: Option<Usage>) -> Step {
    Step::Events(vec![StreamEvent::Finished(completed(
        vec![text_block("nope")],
        stop,
        usage,
    ))])
}

fn policy(
    provider: Arc<dyn Provider>,
    cfg: ContextConfig,
    options: ModelOptions,
) -> SummarizingContext {
    SummarizingContext::new(provider, options, cfg, "summary prompt".into()).unwrap()
}

/// The same policy with the `[context] summary_output_tokens` setting applied.
fn policy_with_cap(
    provider: Arc<dyn Provider>,
    cfg: ContextConfig,
    options: ModelOptions,
    cap: u64,
) -> SummarizingContext {
    policy(provider, cfg, options)
        .with_summary_output_tokens(cap)
        .unwrap()
}

async fn prepare(
    policy: &SummarizingContext,
    history: &[Item],
    usage: Option<&Usage>,
    cancel: &CancellationToken,
) -> Result<Option<Prepared>, ContextError> {
    timeout(
        LIMIT,
        policy.prepare(ContextInput {
            history,
            last_usage: usage,
            cancel,
        }),
    )
    .await
    .expect("prepare hung")
}

/// The `max_output_tokens` of every request the provider was streamed.
fn caps(provider: &ScriptedProvider) -> Vec<Option<u32>> {
    provider
        .requests()
        .iter()
        .map(|request| request.options.max_output_tokens)
        .collect()
}

fn history() -> Vec<Item> {
    vec![assistant_text("x".repeat(500)), assistant_text("tail")]
}

// A truncated summary is retried once with the cap doubled. A usage part is reported only
// when BOTH attempts reported it: `cache_read` and `reasoning_output` are missing on the
// second attempt here, so the sum stays unknown for them — never zero.
#[tokio::test(start_paused = true)]
async fn a_truncated_summary_is_retried_once_with_the_cap_doubled() {
    let history = history();
    let first = Usage {
        input_uncached: Some(10),
        cache_read: Some(2),
        cache_write: Some(3),
        output: Some(DEFAULT_CAP as u64),
        reasoning_output: Some(1),
        cost_micro_usd: Some(5),
    };
    let second = Usage {
        input_uncached: Some(20),
        cache_read: None,
        cache_write: Some(1),
        output: Some(8),
        reasoning_output: None,
        cost_micro_usd: Some(9),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(Some(first)),
        Step::Events(vec![
            StreamEvent::TextDelta {
                block: 0,
                text: "whole".into(),
            },
            StreamEvent::Finished(completed(
                vec![text_block("whole")],
                StopReason::EndTurn,
                Some(second),
            )),
        ]),
    ]));
    let prepared = prepare(
        &policy(
            provider.clone(),
            force_config(&history),
            ModelOptions::default(),
        ),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("the completed retry must be accepted");
    assert_eq!(
        caps(&provider),
        vec![Some(DEFAULT_CAP), Some(2 * DEFAULT_CAP)]
    );
    // The retry repeats the request with the same envelope.
    let requests = provider.requests();
    assert_eq!(requests[0].history, requests[1].history);
    assert_eq!(requests[0].system_prompt, requests[1].system_prompt);
    assert_eq!(
        prepared.usage,
        Some(Usage {
            input_uncached: Some(30),
            cache_read: None,
            cache_write: Some(4),
            output: Some(DEFAULT_CAP as u64 + 8),
            reasoning_output: None,
            cost_micro_usd: Some(14),
        })
    );
}

// The parts both attempts reported are summed, and usage of a retry where NEITHER attempt
// reported anything stays unknown.
#[tokio::test(start_paused = true)]
async fn the_retry_sums_the_parts_both_attempts_reported() {
    let history = history();
    let first = Usage {
        input_uncached: Some(10),
        cache_read: Some(2),
        cache_write: Some(3),
        output: Some(4),
        reasoning_output: Some(1),
        cost_micro_usd: Some(5),
    };
    let second = Usage {
        input_uncached: Some(20),
        cache_read: Some(7),
        cache_write: Some(1),
        output: Some(8),
        reasoning_output: Some(9),
        cost_micro_usd: Some(9),
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(Some(first)),
        Step::Events(vec![
            StreamEvent::TextDelta {
                block: 0,
                text: "whole".into(),
            },
            StreamEvent::Finished(completed(
                vec![text_block("whole")],
                StopReason::EndTurn,
                Some(second),
            )),
        ]),
    ]));
    let prepared = prepare(
        &policy(
            provider.clone(),
            force_config(&history),
            ModelOptions::default(),
        ),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("the completed retry must be accepted");
    assert_eq!(
        prepared.usage,
        Some(Usage {
            input_uncached: Some(30),
            cache_read: Some(9),
            cache_write: Some(4),
            output: Some(12),
            reasoning_output: Some(10),
            cost_micro_usd: Some(14),
        }),
        "every part both attempts reported is summed"
    );

    // Neither attempt reported usage: the replacement's cost is unknown, never zero.
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        text_response("whole"),
    ]));
    let prepared = prepare(
        &policy(
            provider.clone(),
            force_config(&history),
            ModelOptions::default(),
        ),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("the completed retry must be accepted");
    assert_eq!(prepared.usage, None, "unknown usage is not zero usage");
}

// The setting, not the default, is what the request carries and what is doubled.
#[tokio::test(start_paused = true)]
async fn the_configured_cap_is_sent_and_doubled_on_the_retry() {
    let history = history();
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        text_response("whole"),
    ]));
    let prepared = prepare(
        &policy_with_cap(
            provider.clone(),
            force_config(&history),
            ModelOptions::default(),
            1_000,
        ),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .expect("the completed retry must be accepted");
    assert_eq!(caps(&provider), vec![Some(1_000), Some(2_000)]);
    assert!(prepared.usage.is_none(), "neither request reported usage");
}

// The cap is still an upper bound on an agent's own limit, and the retry doubles
// the cap that was actually sent.
#[tokio::test(start_paused = true)]
async fn the_configured_cap_obeys_a_smaller_existing_limit() {
    let history = history();
    let small = ModelOptions {
        max_output_tokens: Some(300),
        ..ModelOptions::default()
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        text_response("whole"),
    ]));
    prepare(
        &policy_with_cap(provider.clone(), force_config(&history), small, 1_000),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(caps(&provider), vec![Some(300), Some(600)]);

    let large = ModelOptions {
        max_output_tokens: Some(5_000),
        ..ModelOptions::default()
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        text_response("whole"),
    ]));
    prepare(
        &policy_with_cap(provider.clone(), force_config(&history), large, 1_000),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(caps(&provider), vec![Some(1_000), Some(2_000)]);
}

// A second truncation is a failure like an empty answer: nothing is accepted, and
// the history is not replaced.
#[tokio::test(start_paused = true)]
async fn a_summary_truncated_twice_is_a_failure_and_replaces_nothing() {
    let history = history();
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        truncated(None),
    ]));
    let answer = prepare(
        &policy(
            provider.clone(),
            force_config(&history),
            ModelOptions::default(),
        ),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(answer.unwrap().is_none(), "a soft failure below the wall");
    assert_eq!(
        caps(&provider),
        vec![Some(DEFAULT_CAP), Some(2 * DEFAULT_CAP)]
    );
}

// At the wall the same failure is fatal and names both numbers and the reason.
#[tokio::test(start_paused = true)]
async fn a_summary_truncated_twice_at_the_wall_is_fatal() {
    let history = history();
    let next = estimate_tokens(&history);
    let cfg = ContextConfig {
        window_tokens: next + 10,
        output_headroom_tokens: 10,
        summarize_at_tokens: next - 1,
        keep_recent_tokens: 1,
        user_verbatim_tokens: 10,
        tool_result_excerpt_chars: 2_000,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        truncated(None),
    ]));
    let answer = prepare(
        &policy(provider.clone(), cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await;
    let Err(ContextError::Failed(message)) = answer else {
        panic!("expected ContextError::Failed");
    };
    assert_eq!(
        message,
        format!(
            "context is full ({next} of {next} tokens) and summarizing failed: the summary was truncated twice"
        )
    );
    assert_eq!(provider.requests().len(), 2);
}

// Any stop other than `EndTurn` is a failure, and never triggers a retry.
#[tokio::test(start_paused = true)]
async fn a_stop_other_than_end_turn_fails_without_a_retry() {
    for stop in [StopReason::Refusal, StopReason::Other, StopReason::ToolUse] {
        let history = history();
        let provider = Arc::new(ScriptedProvider::new(vec![stopped(stop, None)]));
        let answer = prepare(
            &policy(
                provider.clone(),
                force_config(&history),
                ModelOptions::default(),
            ),
            &history,
            None,
            &CancellationToken::new(),
        )
        .await;
        assert!(answer.unwrap().is_none(), "{stop:?}");
        assert_eq!(provider.requests().len(), 1, "{stop:?}");
    }
}

/// A route that refuses `max_output_tokens` (the Codex one), like the frozen
/// suite's wrapper: the field is dropped for the request that is sent.
#[derive(Clone)]
struct RejectExplicitMax {
    inner: Arc<ScriptedProvider>,
}

impl Provider for RejectExplicitMax {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        if request.options.max_output_tokens.is_some() {
            Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "max_output_tokens unsupported",
            ))
        } else {
            self.inner.validate(request)
        }
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.inner.stream(request, cancel)
    }
}

// Where no cap is being sent there is nothing to double: a truncated summary is a
// failure at once, with no second request.
#[tokio::test(start_paused = true)]
async fn a_route_without_a_cap_truncates_and_fails_without_a_retry() {
    let history = history();
    let inner = Arc::new(ScriptedProvider::new(vec![truncated(None)]));
    let wrapper = Arc::new(RejectExplicitMax {
        inner: inner.clone(),
    });
    let answer = prepare(
        &policy(wrapper, force_config(&history), ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await;
    assert!(answer.unwrap().is_none());
    let requests = inner.requests();
    assert_eq!(requests.len(), 1, "no retry without a cap to double");
    assert_eq!(requests[0].options.max_output_tokens, None);
}

// Cancelling while the retry is in flight is `Cancelled`, like any other request.
#[tokio::test(start_paused = true)]
async fn a_cancel_during_the_retry_request_is_cancelled() {
    let history = history();
    let provider = Arc::new(ScriptedProvider::new(vec![
        Step::EventsThenHang(vec![StreamEvent::Finished(completed(
            vec![text_block("cut off")],
            StopReason::MaxOutputTokens,
            None,
        ))]),
        Step::EventsThenHang(vec![]),
    ]));
    let drained = provider.drained.clone();
    let policy = Arc::new(policy(
        provider.clone(),
        force_config(&history),
        ModelOptions::default(),
    ));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let task = tokio::spawn(async move { prepare(&policy, &history, None, &cancel).await });
    timeout(LIMIT, drained.notified())
        .await
        .expect("the retry never started");
    trigger.cancel();
    let answer = timeout(LIMIT, task)
        .await
        .expect("prepare hung after cancellation")
        .expect("task joined");
    assert!(matches!(answer, Err(ContextError::Cancelled)));
    assert_eq!(provider.requests().len(), 2, "the retry was in flight");
}

// The cap is the request's reserve everywhere, the render wall included: a bigger
// cap leaves less room for the rendered transcript.
#[tokio::test(start_paused = true)]
async fn the_configured_cap_bounds_the_rendered_transcript() {
    let mut history = vec![user("old summary")];
    for n in 0..20 {
        history.push(Item::Inbox {
            kind: InboxKind::Notification,
            text: format!("item-{n:02}-{}", "x".repeat(100)),
        });
    }
    history.push(assistant_text("tail"));
    let cfg = ContextConfig {
        window_tokens: 4_300,
        output_headroom_tokens: 0,
        summarize_at_tokens: 300,
        keep_recent_tokens: 1,
        user_verbatim_tokens: 20,
        tool_result_excerpt_chars: 2_000,
    };

    // The default cap reserves 4_000 of the 4_300-token wall, so the oldest items go.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    prepare(
        &policy(provider.clone(), cfg.clone(), ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    let rendered = match &provider.requests()[0].history[0] {
        Item::User { text } => text.clone(),
        other => panic!("expected the rendered transcript, got {other:?}"),
    };
    assert!(rendered.contains("earlier items omitted"), "{rendered}");

    // A cap small enough to fit the whole transcript leaves every item in place.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    prepare(
        &policy_with_cap(provider.clone(), cfg, ModelOptions::default(), 300),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    let request = &provider.requests()[0];
    assert_eq!(request.options.max_output_tokens, Some(300));
    let rendered = match &request.history[0] {
        Item::User { text } => text.clone(),
        other => panic!("expected the rendered transcript, got {other:?}"),
    };
    assert!(!rendered.contains("earlier items omitted"), "{rendered}");
    assert!(rendered.contains("item-00-"), "{rendered}");
}

// The prompt asks for the `## Files` section between "State of the work" and
// "Verified facts", and says what it is for.
#[test]
fn the_default_prompt_asks_for_files_between_state_of_the_work_and_verified_facts() {
    let prompt = DEFAULT_SUMMARIZER_PROMPT;
    let state = prompt.find("## State of the work").expect("state section");
    let files = prompt.find("## Files").expect("files section");
    let verified = prompt.find("## Verified facts").expect("verified section");
    assert!(state < files, "## Files must follow ## State of the work");
    assert!(files < verified, "## Files must precede ## Verified facts");
    let section = &prompt[files..verified];
    assert!(section.contains("read or changed"), "{section}");
    assert!(section.contains("line ranges"), "{section}");
    assert!(section.contains("ranged reads"), "{section}");
    assert!(section.contains("carried") || section.contains("Copied forward"));
}

// The core half of the same rule: a failed retry records no replacement, so the
// turn continues on the history it had.
#[tokio::test(start_paused = true)]
async fn a_failed_retry_through_the_core_replaces_nothing() {
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(None),
        truncated(None),
        text_response("answer"),
    ]));
    let context = Arc::new(policy(
        provider.clone(),
        ContextConfig {
            summarize_at_tokens: 1,
            ..config()
        },
        ModelOptions::default(),
    ));
    let journal = Arc::new(RecordingJournal::new());
    let events = Arc::new(RecordingEvents::new());
    let mut agent = Agent::new(AgentParts {
        provider: provider.clone(),
        tools: vec![],
        system_prompt: "agent prompt".into(),
        options: ModelOptions::default(),
        context,
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: journal.clone(),
        events,
    })
    .unwrap();
    let end = agent
        .run_turn("long input".repeat(100), CancellationToken::new())
        .await;
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert_eq!(provider.requests().len(), 3, "two summaries, then the turn");
    let records = journal.records();
    assert!(
        records
            .iter()
            .all(|record| !matches!(record.body, RecordBody::ContextReplaced { .. })),
        "a failed retry must not replace the history"
    );
}
