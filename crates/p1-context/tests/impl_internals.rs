//! Engine-level tests the frozen acceptance suite leaves implicit: the estimator,
//! the block rendering (blank-line separation, tool-result excerpts, omission of
//! dropped items) and the unit/tail rules, all driven through the public API.
//!
//! `docs/design/context.md` §2 is the specification.

use std::sync::Arc;
use std::time::Duration;

use p1_context::{ContextConfig, SUMMARY_MARKER, SummarizingContext, estimate_tokens};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, ContextError, ContextInput,
    ContextPolicy, Item, ModelOptions, Prepared, Provider, RouteDescription, ToolResultItem,
    ToolStatus,
};
use p1_testkit::{ScriptedProvider, json_call, origin, text_response};
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
