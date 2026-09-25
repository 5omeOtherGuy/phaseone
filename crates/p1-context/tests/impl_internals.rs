//! Engine-level tests the frozen acceptance suite leaves implicit: the estimator,
//! the block rendering (blank-line separation, tool-result excerpts, omission of
//! dropped items) and the unit/tail rules, all driven through the public API.
//!
//! `docs/design/context.md` §2 is the specification.

use std::sync::Arc;
use std::time::Duration;

use p1_context::{
    ContextConfig, DEFAULT_SUMMARY_OUTPUT_TOKENS, SUMMARY_MARKER, SummarizingContext,
    estimate_tokens,
};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, ContextError, ContextInput,
    ContextPolicy, Effort, Item, ModelOptions, Prepared, Provider, RouteDescription, StopReason,
    StreamEvent, ToolResultItem, ToolStatus,
};
use p1_testkit::{ScriptedProvider, Step, completed, json_call, origin, text_block, text_response};
use tokio::time::timeout;

const LIMIT: Duration = Duration::from_secs(5);

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

/// A config that summarizes as soon as the history is bigger than one token.
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

fn assistant(blocks: Vec<AssistantBlock>) -> Item {
    Item::Assistant(AssistantItem {
        origin: origin(),
        blocks,
    })
}

fn assistant_text(text: impl Into<String>) -> Item {
    assistant(vec![AssistantBlock::Text { text: text.into() }])
}

fn result(id: &str, name: &str, content: impl Into<String>) -> Item {
    Item::ToolResult(ToolResultItem {
        call_id: id.into(),
        name: name.into(),
        content: content.into(),
        status: ToolStatus::Ok,
    })
}

fn policy(provider: Arc<dyn Provider>, cfg: ContextConfig) -> SummarizingContext {
    SummarizingContext::new(
        provider,
        ModelOptions::default(),
        cfg,
        "summary prompt".into(),
    )
    .unwrap()
}

async fn prepare(
    policy: &SummarizingContext,
    history: &[Item],
) -> Result<Option<Prepared>, ContextError> {
    timeout(
        LIMIT,
        policy.prepare(ContextInput {
            history,
            last_usage: None,
            cancel: &CancellationToken::new(),
        }),
    )
    .await
    .expect("prepare hung")
}

fn transcript(provider: &ScriptedProvider) -> String {
    match &provider.requests()[0].history[0] {
        Item::User { text } => text.clone(),
        other => panic!("the summary request holds one user item, got {other:?}"),
    }
}

// ---------------------------------------------------------------- estimator

#[test]
fn estimator_counts_visible_text_and_ceils() {
    assert_eq!(estimate_tokens(&[user("12345678")]), 3);
    assert_eq!(estimate_tokens(&[user("1234567")]), 2);
    assert_eq!(estimate_tokens(&[]), 0);
    // A tool call contributes its name and its raw input on top of the text.
    let call = assistant(vec![AssistantBlock::ToolCall(json_call(
        "c1",
        "lookup",
        "0123456789",
    ))]);
    assert!(estimate_tokens(&[call]) > estimate_tokens(&[user("0123456789")]));
}

// ---------------------------------------------------------------- rendering

#[tokio::test(start_paused = true)]
async fn transcript_separates_blocks_with_one_blank_line_and_excerpts_results() {
    let history = vec![
        user(format!("{SUMMARY_MARKER}\nprior")),
        user("task"),
        assistant(vec![
            AssistantBlock::Text {
                text: "answer".into(),
            },
            AssistantBlock::ToolCall(json_call("c1", "lookup", "{}")),
        ]),
        result(
            "c1",
            "lookup",
            format!("{}{}", "H".repeat(6), "T".repeat(6)),
        ),
        assistant_text("tail"),
    ];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.keep_recent_tokens = 1;
    cfg.tool_result_excerpt_chars = 10;
    let prepared = prepare(&policy(provider.clone(), cfg), &history)
        .await
        .unwrap();
    assert!(prepared.is_some());

    let rendered = transcript(&provider);
    assert!(rendered.contains("## Previous summary\nprior"));
    assert!(rendered.contains("\n\n## User\ntask"));
    assert!(rendered.contains("## Assistant\nanswer\n→ lookup({})"));
    assert!(rendered.contains("## Result of lookup [ok]\nHHHHH\n[… 2 chars omitted …]\nTTTTT"));
    // Blocks are separated by exactly one blank line.
    assert!(!rendered.contains("\n\n\n"));
}

// ---------------------------------------------------------------- units

#[tokio::test(start_paused = true)]
async fn a_multi_call_unit_and_its_results_stay_together() {
    let calls = assistant(
        (1..=2)
            .map(|n| AssistantBlock::ToolCall(json_call(&format!("c{n}"), "t", "{}")))
            .collect(),
    );
    let unit = vec![
        calls.clone(),
        result("c1", "t", "one"),
        result("c2", "t", "two"),
    ];
    let mut history = vec![assistant_text("old ".repeat(500))];
    history.extend(unit.clone());
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.keep_recent_tokens = 1;
    let prepared = prepare(&policy(provider, cfg), &history)
        .await
        .unwrap()
        .unwrap();
    assert!(prepared.items.ends_with(&unit));
}

#[tokio::test(start_paused = true)]
async fn a_user_message_after_the_last_unit_is_always_kept() {
    let history = vec![
        assistant_text("x".repeat(500)),
        assistant_text("y".repeat(500)),
        user("TRAILING"),
    ];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.keep_recent_tokens = 0;
    let prepared = prepare(&policy(provider.clone(), cfg), &history)
        .await
        .unwrap()
        .unwrap();
    // The trailing user is part of the tail; the older assistant was summarized.
    assert_eq!(prepared.items.last(), Some(&user("TRAILING")));
    assert!(!prepared.items.contains(&history[0]));
    let rendered = transcript(&provider);
    assert!(!rendered.contains("TRAILING"));
}

// ---------------------------------------------------------------- no-op paths

#[tokio::test(start_paused = true)]
async fn an_empty_history_is_passthrough() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let cfg = ContextConfig {
        summarize_at_tokens: 1,
        ..config()
    };
    assert!(
        prepare(&policy(provider.clone(), cfg), &[])
            .await
            .unwrap()
            .is_none()
    );
    assert!(provider.requests().is_empty());
}

#[test]
fn validate_rejects_a_zero_window_and_a_threshold_at_the_wall() {
    assert!(config().validate().is_ok());
    let mut cfg = config();
    cfg.summarize_at_tokens = cfg.window_tokens - cfg.output_headroom_tokens;
    assert!(cfg.validate().is_err());
    cfg.summarize_at_tokens -= 1;
    assert!(cfg.validate().is_ok());
    cfg.window_tokens = 0;
    assert!(cfg.validate().is_err());
}

// The summary-output cap of "Revision 2026-09-20": the compiled-in default, what
// the setting may be, and the fact that an untouched policy sends the default.
#[tokio::test(start_paused = true)]
async fn the_summary_output_cap_setting_is_validated_and_defaults_to_the_compiled_in_one() {
    assert_eq!(DEFAULT_SUMMARY_OUTPUT_TOKENS, 4_000);
    let cfg = config();
    let wall = cfg.window_tokens - cfg.output_headroom_tokens;
    assert!(cfg.validate_summary_output_tokens(1).is_ok());
    assert!(cfg.validate_summary_output_tokens(wall - 1).is_ok());
    assert!(cfg.validate_summary_output_tokens(wall).is_err());
    assert!(cfg.validate_summary_output_tokens(0).is_err());

    // The setting is refused by the policy too, and an untouched policy sends the
    // compiled-in cap.
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let untouched = policy(provider.clone(), cfg.clone());
    assert!(untouched.with_summary_output_tokens(0).is_err());
    let refused = policy(provider.clone(), cfg.clone())
        .with_summary_output_tokens(wall)
        .is_err();
    assert!(refused);
    let history = vec![assistant_text("x".repeat(500)), assistant_text("tail")];
    let mut cfg = cfg;
    cfg.summarize_at_tokens = 1;
    let prepared = prepare(&policy(provider.clone(), cfg), &history)
        .await
        .unwrap();
    assert!(prepared.is_some());
    assert_eq!(
        provider.requests()[0].options.max_output_tokens,
        Some(DEFAULT_SUMMARY_OUTPUT_TOKENS as u32)
    );
}

// A `Provider` that is never reached: the threshold check runs first.
struct UnreachableProvider;

impl Provider for UnreachableProvider {
    fn describe(&self) -> RouteDescription {
        origin_route()
    }
    fn validate(
        &self,
        _request: &p1_contracts::ProviderRequest,
    ) -> Result<(), p1_contracts::ProviderError> {
        panic!("validate must not run below the threshold")
    }
    fn stream<'a>(
        &'a self,
        _request: p1_contracts::ProviderRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<p1_contracts::ProviderStream, p1_contracts::ProviderError>> {
        panic!("stream must not run below the threshold")
    }
}

fn origin_route() -> RouteDescription {
    RouteDescription {
        origin: origin(),
        supports_freeform_tools: true,
        mandatory_prompt_prefix: None,
        reports_cost: false,
        cache_key: p1_contracts::CacheKeySupport::Unsupported,
    }
}

#[tokio::test(start_paused = true)]
async fn below_the_threshold_no_provider_call_is_made() {
    let history = vec![assistant_text("x".repeat(350))];
    let mut cfg = config();
    cfg.summarize_at_tokens = estimate_tokens(&history) + 1;
    let answer = prepare(
        &SummarizingContext::new(
            Arc::new(UnreachableProvider),
            ModelOptions::default(),
            cfg,
            "p".into(),
        )
        .unwrap(),
        &history,
    )
    .await;
    assert!(answer.unwrap().is_none());
}

// ----------------------------------------------- nothing to summarize

/// A config that always passes the threshold but stays below the wall.
fn nothing_config(history: &[Item]) -> ContextConfig {
    let total = estimate_tokens(history);
    ContextConfig {
        window_tokens: total + 5_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: total.max(2) - 1,
        keep_recent_tokens: 80,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

// (a) one oversized unit: the tail covers everything, so there is nothing outside
// it but the unit itself — and that unit is kept. No request, on either call.
#[tokio::test(start_paused = true)]
async fn a_single_oversized_unit_has_nothing_to_summarize() {
    let history = vec![assistant_text("x".repeat(2_000))];
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let policy = policy(provider.clone(), nothing_config(&history));
    assert!(prepare(&policy, &history).await.unwrap().is_none());
    assert!(prepare(&policy, &history).await.unwrap().is_none());
    assert!(provider.requests().is_empty());
}

// (b) the loop case: a real replacement leaves `[summary, single big unit]`; the
// next prepare has nothing left to summarize and makes no further request.
#[tokio::test(start_paused = true)]
async fn after_a_replacement_the_loop_stops_with_no_further_request() {
    let history = vec![
        assistant_text("older ".repeat(500)),
        assistant_text("y".repeat(200)),
    ];
    let mut cfg = config();
    cfg.keep_recent_tokens = 1;
    cfg.summarize_at_tokens = 100;

    let first = Arc::new(ScriptedProvider::new(vec![text_response("prior")]));
    let prepared = prepare(&policy(first.clone(), cfg.clone()), &history)
        .await
        .unwrap()
        .expect("the older unit is summarized");
    assert_eq!(first.requests().len(), 1);
    assert_eq!(prepared.items.len(), 2);
    assert!(
        matches!(&prepared.items[0], Item::User { text } if text.starts_with(SUMMARY_MARKER)),
        "the replacement starts with the summary item: {:?}",
        prepared.items[0]
    );

    // The replacement is `[summary, single big unit]`: nothing is left to render.
    let second = Arc::new(ScriptedProvider::new(vec![]));
    let policy = policy(second.clone(), cfg);
    assert!(prepare(&policy, &prepared.items).await.unwrap().is_none());
    assert!(prepare(&policy, &prepared.items).await.unwrap().is_none());
    assert!(second.requests().is_empty());
}

// (c) the same shape at the wall: no request, and the exact failure message.
#[tokio::test(start_paused = true)]
async fn nothing_to_summarize_at_the_wall_has_the_exact_message() {
    let history = vec![assistant_text("x".repeat(2_000))];
    let next = estimate_tokens(&history);
    let cfg = ContextConfig {
        window_tokens: next + 1_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: 100,
        keep_recent_tokens: 80,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    };
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let answer = prepare(&policy(provider.clone(), cfg), &history).await;
    let Err(ContextError::Failed(message)) = answer else {
        panic!("expected ContextError::Failed");
    };
    assert_eq!(
        message,
        format!("context is full ({next} of {next} tokens) and nothing is left to summarize")
    );
    assert!(provider.requests().is_empty());
}

// (d) a previous summary plus new older material still rolls: one request.
#[tokio::test(start_paused = true)]
async fn a_previous_summary_plus_new_older_material_still_summarizes() {
    let history = vec![
        user(format!("{SUMMARY_MARKER}\nprior")),
        assistant_text("older ".repeat(100)),
        assistant_text("tail"),
    ];
    let mut cfg = config();
    cfg.keep_recent_tokens = 80;
    cfg.summarize_at_tokens = 100;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("rolled")]));
    let prepared = prepare(&policy(provider.clone(), cfg), &history)
        .await
        .unwrap();
    assert!(prepared.is_some());
    assert_eq!(provider.requests().len(), 1);
    let rendered = transcript(&provider);
    assert!(rendered.contains("## Previous summary"));
    assert!(rendered.contains("older"));
}

// A history with no units (a lone user message) is material: it is not a previous
// summary, so it is still summarized.
#[tokio::test(start_paused = true)]
async fn a_unitless_history_is_still_material() {
    let history = vec![user("only the user's words")];
    let mut cfg = config();
    cfg.summarize_at_tokens = 1;
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let prepared = prepare(&policy(provider.clone(), cfg), &history)
        .await
        .unwrap();
    assert!(prepared.is_some());
    assert_eq!(provider.requests().len(), 1);
}

// ------------------------------------------- #125: the summary's own effort

/// A summary request that hit the output cap: text, then `MaxOutputTokens`.
fn truncated() -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: "cut off".into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block("cut off")],
            StopReason::MaxOutputTokens,
            None,
        )),
    ])
}

fn options_with(effort: Effort) -> ModelOptions {
    ModelOptions {
        reasoning_effort: Some(effort),
        ..ModelOptions::default()
    }
}

/// The two-item history the effort tests summarize: one summable unit and a tail.
fn effort_history() -> Vec<Item> {
    vec![assistant_text("x".repeat(500)), assistant_text("tail")]
}

/// A config that summarizes that history once `keep_recent_tokens = 80` leaves a one-unit tail.
fn effort_config() -> ContextConfig {
    let mut cfg = config();
    cfg.summarize_at_tokens = 100;
    cfg
}

// #125: the summarization request must not inherit the agent's reasoning effort, whatever
// that effort is. The host passes the LOWEST effort the model profile supports; every
// request then carries it, so the summary cap buys summary text and not reasoning.
#[tokio::test(start_paused = true)]
async fn the_summary_request_carries_the_lowered_effort_whatever_the_agents_effort_is() {
    let history = effort_history();
    for agent_effort in [Effort::Medium, Effort::High, Effort::ExtraHigh, Effort::Max] {
        let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
        let policy = SummarizingContext::new(
            provider.clone(),
            options_with(agent_effort),
            effort_config(),
            "summary prompt".into(),
        )
        .unwrap()
        .with_summary_effort(Some(Effort::Low));
        assert!(
            prepare(&policy, &history).await.unwrap().is_some(),
            "an agent at {agent_effort:?} still summarizes"
        );
        assert_eq!(
            provider.requests()[0].options.reasoning_effort,
            Some(Effort::Low),
            "the summary request of an agent at {agent_effort:?}"
        );
    }
}

// Without the setting the request keeps the effort its options carry. The host never takes this
// path — it always passes a floor (the profile's lowest level, else `Low`) — but the module has
// no effort scale of its own, so an unset effort must leave the options alone.
#[tokio::test(start_paused = true)]
async fn an_unset_summary_effort_keeps_the_effort_the_options_carry() {
    let history = effort_history();
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let policy = SummarizingContext::new(
        provider.clone(),
        options_with(Effort::Max),
        effort_config(),
        "summary prompt".into(),
    )
    .unwrap();
    assert!(prepare(&policy, &history).await.unwrap().is_some());
    assert_eq!(
        provider.requests()[0].options.reasoning_effort,
        Some(Effort::Max)
    );
}

// The one cap-doubling retry of "Revision 2026-09-20" is unchanged by #125, and the retry
// carries the same lowered effort as the first request.
#[tokio::test(start_paused = true)]
async fn the_cap_doubling_retry_keeps_the_lowered_effort() {
    let history = effort_history();
    let provider = Arc::new(ScriptedProvider::new(vec![
        truncated(),
        text_response("whole"),
    ]));
    let policy = SummarizingContext::new(
        provider.clone(),
        options_with(Effort::Max),
        effort_config(),
        "summary prompt".into(),
    )
    .unwrap()
    .with_summary_effort(Some(Effort::Low));
    assert!(prepare(&policy, &history).await.unwrap().is_some());

    let requests = provider.requests();
    assert_eq!(requests.len(), 2, "one retry");
    assert_eq!(
        requests[0].options.max_output_tokens,
        Some(DEFAULT_SUMMARY_OUTPUT_TOKENS as u32)
    );
    assert_eq!(
        requests[1].options.max_output_tokens,
        Some(DEFAULT_SUMMARY_OUTPUT_TOKENS as u32 * 2),
        "the retry doubles the cap"
    );
    assert_eq!(requests[0].options.reasoning_effort, Some(Effort::Low));
    assert_eq!(requests[1].options.reasoning_effort, Some(Effort::Low));
}
