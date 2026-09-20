//! Black-box acceptance tests for `docs/design/context.md` §2 and §4.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use p1_context::{
    ContextConfig, DEFAULT_SUMMARIZER_PROMPT, SUMMARY_MARKER, SummarizingContext, estimate_tokens,
};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, ContextError, ContextInput,
    ContextPolicy, InboxKind, InterruptionReason, Item, ModelOptions, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RecordBody, ReplayData, RouteDescription,
    StopReason, StreamEvent, ToolResultItem, ToolStatus, TurnEnd, Usage,
};
use p1_core::{Agent, AgentParts};
use p1_journal::{JsonlJournal, SyncPolicy};
use p1_testkit::{
    RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider, Step, completed,
    json_call, origin, text_block, text_response,
};
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
        status: ToolStatus::Ok,
        content: content.into(),
    })
}

fn summary_text(item: &Item) -> &str {
    match item {
        Item::User { text } => text,
        other => panic!("expected summary user item, got {other:?}"),
    }
}

fn scripted_summary(text: &str, usage: Option<Usage>) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(completed(
            vec![text_block(text)],
            StopReason::EndTurn,
            usage,
        )),
    ])
}

fn policy(
    provider: Arc<dyn Provider>,
    cfg: ContextConfig,
    options: ModelOptions,
) -> SummarizingContext {
    SummarizingContext::new(provider, options, cfg, "summary prompt".into()).unwrap()
}

async fn prepare(
    policy: &SummarizingContext,
    history: &[Item],
    usage: Option<&Usage>,
    cancel: &CancellationToken,
) -> Result<Option<p1_contracts::Prepared>, ContextError> {
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

fn assert_pairing(items: &[Item]) {
    let mut calls = HashMap::<String, usize>::new();
    let mut results = HashSet::<String>::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Assistant(item) => {
                for call in item.tool_calls() {
                    calls.insert(call.call_id.clone(), index);
                }
            }
            Item::ToolResult(item) => {
                let call_index = calls
                    .get(&item.call_id)
                    .unwrap_or_else(|| panic!("result {} has no call", item.call_id));
                assert!(*call_index < index, "result must follow its call");
                results.insert(item.call_id.clone());
            }
            Item::User { .. } | Item::Inbox { .. } => {}
        }
    }
    for (index, item) in items.iter().enumerate() {
        if let Item::Assistant(item) = item
            && index + 1 != items.len()
        {
            for call in item.tool_calls() {
                assert!(
                    results.contains(&call.call_id),
                    "non-final call {} has no result",
                    call.call_id
                );
            }
        }
    }
}

// ContextConfig::validate.
#[tokio::test(start_paused = true)]
async fn validate_rejects_threshold_at_or_above_input_wall_and_zero_window() {
    let mut cfg = config();
    cfg.window_tokens = 100;
    cfg.output_headroom_tokens = 20;
    cfg.summarize_at_tokens = 80;
    assert!(cfg.validate().is_err());
    cfg.summarize_at_tokens = 79;
    assert!(cfg.validate().is_ok());
    cfg.window_tokens = 0;
    assert!(cfg.validate().is_err());
}

// ContextConfig::validate and constructor preconditions.
#[tokio::test(start_paused = true)]
async fn constructor_refuses_invalid_config_and_empty_prompt() {
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut invalid = config();
    invalid.summarize_at_tokens = invalid.window_tokens;
    assert!(
        SummarizingContext::new(
            provider.clone(),
            ModelOptions::default(),
            invalid,
            "prompt".into()
        )
        .is_err()
    );
    assert!(
        SummarizingContext::new(provider, ModelOptions::default(), config(), String::new())
            .is_err()
    );
}

// estimate_tokens definition.
#[tokio::test(start_paused = true)]
async fn token_estimate_uses_ceiling_and_counts_tool_input_and_replay_payload() {
    assert_eq!(estimate_tokens(&[user("12345678")]), 3);
    let replay = ReplayData {
        origin: origin(),
        version: 1,
        payload: p1_contracts::serde_json::json!({"opaque":"replay payload"}),
    };
    let rich = assistant(vec![
        AssistantBlock::Reasoning {
            text: "thought".into(),
            replay: Some(replay),
        },
        AssistantBlock::ToolCall(json_call("c", "tool", "{\"long\":\"input\"}")),
    ]);
    assert!(estimate_tokens(&[rich]) > estimate_tokens(&[user("thought")]));
}

// Must-pass (a): strict threshold boundary.
#[tokio::test(start_paused = true)]
async fn one_token_below_threshold_returns_none_without_provider_request() {
    let history = vec![assistant_text("x".repeat(350))];
    let estimated = estimate_tokens(&history);
    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut cfg = config();
    cfg.summarize_at_tokens = estimated + 1;
    let policy = policy(provider.clone(), cfg, ModelOptions::default());
    let answer = prepare(&policy, &history, None, &CancellationToken::new())
        .await
        .unwrap();
    assert!(answer.is_none());
    assert!(provider.requests().is_empty());
}

// Must-pass (a): equality triggers summarization.
#[tokio::test(start_paused = true)]
async fn exactly_at_threshold_makes_one_provider_request() {
    let history = vec![assistant_text("x".repeat(700)), assistant_text("tail")];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("short summary")]));
    let mut cfg = config();
    cfg.summarize_at_tokens = estimate_tokens(&history);
    let policy = policy(provider.clone(), cfg, ModelOptions::default());
    let answer = prepare(&policy, &history, None, &CancellationToken::new())
        .await
        .unwrap();
    assert!(answer.is_some());
    assert_eq!(provider.requests().len(), 1);
}

// Must-pass (e): known usage includes cache fields and only items after the last assistant.
#[tokio::test(start_paused = true)]
async fn known_usage_plus_later_items_controls_all_three_arithmetic_cases() {
    let later = user("1234567"); // exactly two estimated tokens
    let history = vec![assistant_text("old history ".repeat(100)), later];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = config();
    cfg.summarize_at_tokens = 17;
    let policy = policy(provider.clone(), cfg, ModelOptions::default());
    let cancel = CancellationToken::new();

    let below = Usage {
        input_uncached: Some(5),
        cache_read: Some(3),
        cache_write: Some(2),
        output: Some(4),
        ..Usage::default()
    };
    assert!(
        prepare(&policy, &history, Some(&below), &cancel)
            .await
            .unwrap()
            .is_none()
    );
    assert!(provider.requests().is_empty());

    let at = Usage {
        input_uncached: Some(6),
        cache_read: Some(3),
        cache_write: Some(2),
        output: Some(4),
        ..Usage::default()
    };
    assert!(
        prepare(&policy, &history, Some(&at), &cancel)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(provider.requests().len(), 1);
}

// Must-pass (e): absent cache counts as zero, but absent required usage falls back to history.
#[tokio::test(start_paused = true)]
async fn missing_cache_is_zero_and_missing_required_usage_estimates_whole_history() {
    let history = vec![assistant_text("old ".repeat(300)), user("new")];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = config();
    cfg.summarize_at_tokens = 20;
    let incomplete_policy = policy(provider.clone(), cfg, ModelOptions::default());
    let incomplete = Usage {
        input_uncached: Some(2),
        output: None,
        ..Usage::default()
    };
    assert!(
        prepare(
            &incomplete_policy,
            &history,
            Some(&incomplete),
            &CancellationToken::new()
        )
        .await
        .unwrap()
        .is_some()
    );
    assert_eq!(provider.requests().len(), 1);

    let provider = Arc::new(ScriptedProvider::new(vec![]));
    let mut missing_cache_cfg = config();
    missing_cache_cfg.summarize_at_tokens = 4;
    let policy = policy(provider.clone(), missing_cache_cfg, ModelOptions::default());
    let complete_without_cache = Usage {
        input_uncached: Some(1),
        output: Some(1),
        ..Usage::default()
    };
    assert!(
        prepare(
            &policy,
            &history,
            Some(&complete_without_cache),
            &CancellationToken::new()
        )
        .await
        .unwrap()
        .is_none()
    );
    assert!(provider.requests().is_empty());
}

// Must-pass (b): replacement shape, exact marker, user bytes/order, and replay-exact tail.
#[tokio::test(start_paused = true)]
async fn replacement_has_summary_then_verbatim_users_then_exact_tail() {
    let replay = ReplayData {
        origin: origin(),
        version: 9,
        payload: p1_contracts::serde_json::json!({"signature":[0,255,"opaque"]}),
    };
    let tail = assistant(vec![
        AssistantBlock::Reasoning {
            text: "private reasoning text".into(),
            replay: Some(replay),
        },
        AssistantBlock::Text {
            text: "answer".into(),
        },
    ]);
    let task = "task\0with\r\nexact bytes";
    let constraint = "constraint: preserve λ";
    let history = vec![
        user(task),
        assistant_text("old ".repeat(500)),
        user(constraint),
        tail.clone(),
    ];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        "SCRIPTED ANSWER",
    )]));
    let mut cfg = force_config(&history);
    cfg.keep_recent_tokens = estimate_tokens(std::slice::from_ref(&tail));
    let prepared = prepare(
        &policy(provider, cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prepared.items,
        vec![
            user(format!("{SUMMARY_MARKER}\nSCRIPTED ANSWER")),
            user(task),
            user(constraint),
            tail,
        ]
    );
}

// Must-pass (b): overflow preserves first/newest and renders displaced user words.
#[tokio::test(start_paused = true)]
async fn user_budget_overflow_keeps_first_and_newest_and_renders_the_middle() {
    let first = "FIRST-TASK";
    let middle = "MIDDLE-DISPLACED";
    let newest = "NEWEST-RULE";
    let tail = assistant_text("tail");
    let history = vec![
        user(first),
        assistant_text("a".repeat(500)),
        user(middle),
        assistant_text("b".repeat(500)),
        user(newest),
        assistant_text("c".repeat(500)),
        tail.clone(),
    ];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.user_verbatim_tokens = estimate_tokens(&[user(first), user(newest)]);
    cfg.keep_recent_tokens = estimate_tokens(std::slice::from_ref(&tail));
    let prepared = prepare(
        &policy(provider.clone(), cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        prepared.items,
        vec![
            user(format!("{SUMMARY_MARKER}\ns")),
            user(first),
            user(newest),
            tail
        ]
    );
    let requests = provider.requests();
    let rendered = match &requests[0].history[0] {
        Item::User { text } => text,
        _ => unreachable!(),
    };
    assert!(rendered.contains(middle));
}

async fn assert_boundary_history(history: Vec<Item>, expected_tail: &[Item]) {
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.keep_recent_tokens = 1;
    let prepared = prepare(
        &policy(provider, cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(prepared.items.ends_with(expected_tail));
    assert_pairing(&prepared.items);
}

// Must-pass (c): a call/result unit is indivisible.
#[tokio::test(start_paused = true)]
async fn tail_boundary_between_call_and_result_keeps_both() {
    let call = assistant(vec![AssistantBlock::ToolCall(json_call(
        "c1", "read", "{}",
    ))]);
    let res = result("c1", "read", "ok");
    let history = vec![
        assistant_text("old ".repeat(500)),
        call.clone(),
        res.clone(),
    ];
    assert_boundary_history(history, &[call, res]).await;
}

// Must-pass (c): all results of a multi-call response stay with it.
#[tokio::test(start_paused = true)]
async fn tail_boundary_inside_three_call_response_keeps_the_whole_unit() {
    let calls = assistant(
        (1..=3)
            .map(|n| AssistantBlock::ToolCall(json_call(&format!("c{n}"), "t", "{}")))
            .collect(),
    );
    let mut unit = vec![calls];
    for n in 1..=3 {
        unit.push(result(&format!("c{n}"), "t", "ok"));
    }
    let mut history = vec![assistant_text("old ".repeat(500))];
    history.extend(unit.clone());
    assert_boundary_history(history, &unit).await;
}

// Must-pass (c): an inbox immediately before an assistant belongs to that unit.
#[tokio::test(start_paused = true)]
async fn tail_boundary_after_inbox_keeps_inbox_with_following_assistant() {
    let inbox = Item::Inbox {
        kind: InboxKind::Steering,
        text: "steer".into(),
    };
    let answer = assistant_text("tail answer");
    let history = vec![
        assistant_text("old ".repeat(500)),
        inbox.clone(),
        answer.clone(),
    ];
    assert_boundary_history(history, &[inbox, answer]).await;
}

// Must-pass (c): deterministic generated histories always satisfy core pairing invariants.
#[tokio::test(start_paused = true)]
async fn generated_replacements_always_pair_calls_and_results() {
    let provider = Arc::new(ScriptedProvider::new(
        (0..18).map(|_| text_response("s")).collect(),
    ));
    for seed in 0..18 {
        let mut history = vec![assistant_text(format!("old-{seed}-{}", "x".repeat(500)))];
        for unit in 0..1 + seed % 4 {
            if (seed + unit) % 3 == 0 {
                history.push(Item::Inbox {
                    kind: InboxKind::Notification,
                    text: format!("n-{seed}-{unit}"),
                });
            }
            let count = 1 + (seed + unit) % 3;
            history.push(assistant(
                (0..count)
                    .map(|call| {
                        let id = format!("g-{seed}-{unit}-{call}");
                        AssistantBlock::ToolCall(json_call(&id, "generated", "{}"))
                    })
                    .collect(),
            ));
            for call in 0..count {
                let id = format!("g-{seed}-{unit}-{call}");
                history.push(result(&id, "generated", "ok"));
            }
        }
        let mut cfg = force_config(&history);
        cfg.keep_recent_tokens = (seed % 17 + 1) as u64;
        let prepared = prepare(
            &policy(provider.clone(), cfg, ModelOptions::default()),
            &history,
            None,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
        assert_pairing(&prepared.items);
    }
}

// The summarization request shape and transcript rendering.
#[tokio::test(start_paused = true)]
async fn summarization_request_has_exact_envelope_headings_order_and_redactions() {
    let long_input = "i".repeat(510);
    let long_result = format!("{}{}", "H".repeat(6), "T".repeat(6));
    let history = vec![
        user(format!("{SUMMARY_MARKER}\nold summary")),
        user("task"),
        Item::Inbox {
            kind: InboxKind::Notification,
            text: "notice".into(),
        },
        Item::Inbox {
            kind: InboxKind::Steering,
            text: "steer".into(),
        },
        assistant(vec![
            AssistantBlock::Reasoning {
                text: "SECRET REASONING".into(),
                replay: None,
            },
            AssistantBlock::Text {
                text: "visible answer".into(),
            },
            AssistantBlock::ToolCall(json_call("c1", "lookup", &long_input)),
        ]),
        result("c1", "lookup", long_result),
        assistant_text("recent"),
    ];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.keep_recent_tokens = 1;
    cfg.tool_result_excerpt_chars = 10;
    let mut options = ModelOptions {
        max_output_tokens: Some(8_000),
        ..ModelOptions::default()
    };
    options.cache_key = Some("preserved".into());
    prepare(
        &policy(provider.clone(), cfg, options),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    let request = &provider.requests()[0];
    assert_eq!(request.system_prompt, "summary prompt");
    assert!(request.tools.is_empty());
    assert_eq!(request.history.len(), 1);
    assert_eq!(request.options.max_output_tokens, Some(4_000));
    assert_eq!(request.options.cache_key.as_deref(), Some("preserved"));
    let rendered = summary_text(&request.history[0]);
    let headings: Vec<&str> = rendered
        .lines()
        .filter(|line| line.starts_with("## "))
        .collect();
    assert_eq!(
        &headings[..5],
        [
            "## Previous summary",
            "## User",
            "## Notification",
            "## Steering",
            "## Assistant",
        ]
    );
    assert_eq!(headings.len(), 6);
    assert!(headings[5].starts_with("## Result of lookup ["));
    assert!(headings[5].ends_with(']'));
    assert!(!rendered.contains("SECRET REASONING"));
    assert!(rendered.contains(&format!("→ lookup({})", "i".repeat(500))));
    assert!(!rendered.contains(&"i".repeat(501)));
    assert!(rendered.contains("HHHHH\n[… 2 chars omitted …]\nTTTTT"));
}

// Summarization options preserve a smaller explicit maximum.
#[tokio::test(start_paused = true)]
async fn summarization_preserves_smaller_existing_output_limit() {
    let history = vec![assistant_text("x".repeat(500)), assistant_text("tail")];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let options = ModelOptions {
        max_output_tokens: Some(123),
        ..ModelOptions::default()
    };
    prepare(
        &policy(provider.clone(), force_config(&history), options),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(provider.requests()[0].options.max_output_tokens, Some(123));
}

// Must-pass (h): oversized rendering drops a counted oldest prefix and fits its budget.
#[tokio::test(start_paused = true)]
async fn oversized_transcript_has_correct_omission_count_and_fits_render_wall() {
    let mut history = vec![user(format!("{SUMMARY_MARKER}\nold"))];
    for n in 0..20 {
        history.push(Item::Inbox {
            kind: InboxKind::Notification,
            text: format!("item-{n:02}-{}", "x".repeat(100)),
        });
    }
    history.push(assistant_text("tail"));
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = ContextConfig {
        window_tokens: 4_300,
        output_headroom_tokens: 0,
        summarize_at_tokens: 300,
        keep_recent_tokens: 1,
        user_verbatim_tokens: 20,
        tool_result_excerpt_chars: 2_000,
    };
    if estimate_tokens(&history) < cfg.summarize_at_tokens {
        cfg.summarize_at_tokens = estimate_tokens(&history);
    }
    prepare(
        &policy(provider.clone(), cfg.clone(), ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    let request = &provider.requests()[0];
    let rendered = summary_text(&request.history[0]);
    let first_kept = (0..20)
        .find(|n| rendered.contains(&format!("item-{n:02}-")))
        .expect("some item remains");
    assert!(rendered.contains(&format!(
        "[{first_kept} earlier items omitted: the session was too long to summarize in one pass]"
    )));
    for n in 0..first_kept {
        assert!(!rendered.contains(&format!("item-{n:02}-")));
    }
    assert!(
        estimate_tokens(&request.history) <= cfg.window_tokens - cfg.output_headroom_tokens - 4_000
    );
}

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

// Must-pass (j): retry validation once without a rejected max_output_tokens field.
#[tokio::test(start_paused = true)]
async fn route_rejecting_output_limit_receives_request_without_it() {
    let history = vec![assistant_text("x".repeat(500)), assistant_text("tail")];
    let inner = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let wrapper = Arc::new(RejectExplicitMax {
        inner: inner.clone(),
    });
    prepare(
        &policy(wrapper, force_config(&history), ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(inner.requests().len(), 1);
    assert_eq!(inner.requests()[0].options.max_output_tokens, None);
}

// Must-pass (d): rolling summaries preserve user words and exactly one marker for five rounds.
#[tokio::test(start_paused = true)]
async fn five_replacements_roll_previous_summary_and_preserve_task_and_constraint() {
    let provider = Arc::new(ScriptedProvider::new(
        (1..=5)
            .map(|n| text_response(&format!("summary-{n}")))
            .collect(),
    ));
    let task = "original task";
    let constraint = "constraint: never alter this";
    let mut history = vec![
        user(task),
        assistant_text("old ".repeat(500)),
        user(constraint),
        assistant_text("tail"),
    ];
    for round in 1..=5 {
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
        .unwrap();
        assert!(prepared.items.contains(&user(task)));
        assert!(prepared.items.contains(&user(constraint)));
        assert_eq!(
            prepared
                .items
                .iter()
                .filter(|item| summary_text_if_any(item).is_some())
                .count(),
            1
        );
        if round > 1 {
            let requests = provider.requests();
            let rendered = summary_text(&requests[round - 1].history[0]);
            assert!(rendered.contains("## Previous summary"));
            assert!(rendered.contains(&format!("summary-{}", round - 1)));
        }
        history = prepared.items;
        history.insert(history.len() - 1, assistant_text("growth ".repeat(500)));
    }
}

fn summary_text_if_any(item: &Item) -> Option<&str> {
    match item {
        Item::User { text } if text.starts_with(SUMMARY_MARKER) => Some(text),
        _ => None,
    }
}

// Must-pass (f), module half: cancelling an in-flight stream returns Cancelled.
#[tokio::test(start_paused = true)]
async fn cancellation_during_summary_drops_stream_and_returns_cancelled() {
    let history = vec![assistant_text("x".repeat(500)), assistant_text("tail")];
    let provider = Arc::new(ScriptedProvider::new(vec![Step::EventsThenHang(vec![])]));
    let drained = provider.drained.clone();
    let policy = Arc::new(policy(
        provider,
        force_config(&history),
        ModelOptions::default(),
    ));
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let task = tokio::spawn(async move { prepare(&policy, &history, None, &cancel).await });
    timeout(LIMIT, drained.notified())
        .await
        .expect("stream never started");
    trigger.cancel();
    let answer = timeout(LIMIT, task)
        .await
        .expect("prepare hung after cancellation")
        .expect("task joined");
    assert!(matches!(answer, Err(ContextError::Cancelled)));
}

fn core_parts(
    provider: Arc<dyn Provider>,
    context: Arc<dyn ContextPolicy>,
    journal: Arc<dyn p1_contracts::CommitSink>,
    events: Arc<RecordingEvents>,
) -> AgentParts {
    AgentParts {
        provider,
        tools: vec![],
        system_prompt: "agent prompt".into(),
        options: ModelOptions::default(),
        context,
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal,
        events,
    }
}

// Must-pass (f), core half: cancellation records interruption only, never replacement.
#[tokio::test(start_paused = true)]
async fn core_cancellation_during_summary_records_interruption_without_replacement() {
    let provider = Arc::new(ScriptedProvider::new(vec![Step::EventsThenHang(vec![])]));
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
    let mut agent = Agent::new(core_parts(
        provider.clone(),
        context,
        journal.clone(),
        events,
    ))
    .unwrap();
    let cancel = CancellationToken::new();
    let trigger = cancel.clone();
    let drained = provider.drained.clone();
    let task = tokio::spawn(async move { agent.run_turn("long input".repeat(100), cancel).await });
    timeout(LIMIT, drained.notified())
        .await
        .expect("summary did not start");
    trigger.cancel();
    assert_eq!(
        timeout(LIMIT, task).await.unwrap().unwrap(),
        TurnEnd::Cancelled
    );
    let records = journal.records();
    assert!(
        records
            .iter()
            .all(|r| !matches!(r.body, RecordBody::ContextReplaced { .. }))
    );
    assert!(matches!(
        records.last().unwrap().body,
        RecordBody::AssistantInterrupted {
            reason: InterruptionReason::Cancelled,
            ..
        }
    ));
}

fn transport_failure() -> Step {
    Step::SetupError(ProviderError::new(
        ProviderErrorKind::Transport,
        "summary unavailable",
    ))
}

// Must-pass (g): below-wall failure is soft and the next prepare retries.
#[tokio::test(start_paused = true)]
async fn failure_below_wall_returns_none_and_next_prepare_retries() {
    let history = vec![assistant_text("x".repeat(700)), assistant_text("tail")];
    let provider = Arc::new(ScriptedProvider::new(vec![
        transport_failure(),
        text_response("s"),
    ]));
    let policy = policy(
        provider.clone(),
        force_config(&history),
        ModelOptions::default(),
    );
    let cancel = CancellationToken::new();
    assert!(
        prepare(&policy, &history, None, &cancel)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        prepare(&policy, &history, None, &cancel)
            .await
            .unwrap()
            .is_some()
    );
    assert_eq!(provider.requests().len(), 2);
}

// Must-pass (g): at-wall failure has the exact Failed message with both numbers.
#[tokio::test(start_paused = true)]
async fn failure_at_wall_names_next_input_window_and_reason_exactly() {
    let history = vec![assistant_text("x".repeat(700)), assistant_text("tail")];
    let next = estimate_tokens(&history);
    let provider = Arc::new(ScriptedProvider::new(vec![transport_failure()]));
    let cfg = ContextConfig {
        window_tokens: next + 10,
        output_headroom_tokens: 10,
        summarize_at_tokens: next - 1,
        keep_recent_tokens: 1,
        user_verbatim_tokens: 10,
        tool_result_excerpt_chars: 2_000,
    };
    let answer = prepare(
        &policy(provider, cfg, ModelOptions::default()),
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
            "context is full ({next} of {next} tokens) and summarizing failed: Transport: summary unavailable"
        )
    );
}

// Failure rule: an empty completed answer is summarization failure.
#[tokio::test(start_paused = true)]
async fn empty_summary_answer_is_a_soft_failure_below_wall() {
    let history = vec![assistant_text("x".repeat(700)), assistant_text("tail")];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("")]));
    let answer = prepare(
        &policy(provider, force_config(&history), ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(answer.is_none());
}

// Failure rule: a replacement which is not smaller is rejected.
#[tokio::test(start_paused = true)]
async fn non_smaller_replacement_is_a_soft_failure_below_wall() {
    let history = vec![assistant_text("old"), assistant_text("tail")];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response(
        &"larger ".repeat(100),
    )]));
    let mut cfg = config();
    cfg.summarize_at_tokens = estimate_tokens(&history);
    let answer = prepare(
        &policy(provider, cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(answer.is_none());
}

// Failure rule: an over-large tail is rebuilt by halving while retaining one whole unit.
#[tokio::test(start_paused = true)]
async fn oversized_successful_replacement_halves_tail_but_keeps_at_least_last_unit() {
    let oldest = assistant_text("old ".repeat(500));
    let recent1 = assistant_text("one ".repeat(80));
    let recent2 = assistant_text("two ".repeat(80));
    let last = assistant_text("last");
    let history = vec![oldest, recent1.clone(), recent2.clone(), last.clone()];
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let mut cfg = force_config(&history);
    cfg.summarize_at_tokens = estimate_tokens(&[
        user(format!("{SUMMARY_MARKER}\ns")),
        recent2.clone(),
        last.clone(),
    ]);
    cfg.keep_recent_tokens = estimate_tokens(&[recent1.clone(), recent2.clone(), last.clone()]);
    let prepared = prepare(
        &policy(provider, cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(prepared.items.ends_with(std::slice::from_ref(&last)));
    assert!(!prepared.items.contains(&recent1));
    assert!(estimate_tokens(&prepared.items) < estimate_tokens(&history));
}

// Must-pass (i), module half: behavior is entirely a function of supplied history.
#[tokio::test(start_paused = true)]
async fn fresh_policy_behaves_identically_on_history_containing_existing_summary() {
    let history = vec![
        user(format!("{SUMMARY_MARKER}\nprior")),
        assistant_text("x".repeat(700)),
        assistant_text("tail"),
    ];
    let first_provider = Arc::new(ScriptedProvider::new(vec![text_response("next")]));
    let second_provider = Arc::new(ScriptedProvider::new(vec![text_response("next")]));
    let cfg = force_config(&history);
    let first = prepare(
        &policy(first_provider.clone(), cfg.clone(), ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    let fresh = prepare(
        &policy(second_provider.clone(), cfg, ModelOptions::default()),
        &history,
        None,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(first.items, fresh.items);
    assert_eq!(first.usage, fresh.usage);
    assert_eq!(first_provider.requests(), second_provider.requests());
}

// Must-pass (i): replacement and usage survive JSONL resume, then replacement works again.
#[tokio::test(start_paused = true)]
async fn jsonl_resume_restores_replaced_history_and_allows_second_replacement() {
    let first_usage = Usage {
        input_uncached: Some(11),
        output: Some(2),
        ..Usage::default()
    };
    let second_usage = Usage {
        input_uncached: Some(12),
        output: Some(3),
        ..Usage::default()
    };
    let resumed_last_usage = Usage {
        input_uncached: Some(600),
        output: Some(1),
        ..Usage::default()
    };
    let provider = Arc::new(ScriptedProvider::new(vec![
        text_response(&"old ".repeat(500)),
        text_response("tiny"),
        scripted_summary("first-summary", Some(first_usage)),
        scripted_summary("followed", Some(resumed_last_usage)),
        scripted_summary("second-summary", Some(second_usage)),
        text_response("done"),
    ]));
    let initial_for_threshold = vec![
        user("task"),
        assistant_text("old ".repeat(500)),
        user("second"),
    ];
    let threshold = estimate_tokens(&initial_for_threshold) + 1;
    let cfg = ContextConfig {
        summarize_at_tokens: threshold,
        keep_recent_tokens: 10,
        ..config()
    };
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let journal = Arc::new(JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap());
    let events = Arc::new(RecordingEvents::new());
    let context = Arc::new(policy(
        provider.clone(),
        cfg.clone(),
        ModelOptions::default(),
    ));
    let mut agent = Agent::new(core_parts(
        provider.clone(),
        context,
        journal.clone(),
        events,
    ))
    .unwrap();
    for input in ["task", "second", "third"] {
        let end = timeout(
            LIMIT,
            agent.run_turn(input.into(), CancellationToken::new()),
        )
        .await
        .unwrap();
        assert_eq!(
            end,
            TurnEnd::Completed {
                stop: StopReason::EndTurn
            }
        );
    }
    let before_drop = agent.history().to_vec();
    drop(agent);
    drop(journal);

    let (resumed_journal, resumed) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    let first_replacement = resumed
        .records
        .iter()
        .find_map(|record| match &record.body {
            RecordBody::ContextReplaced { items, .. } => Some(items.clone()),
            _ => None,
        })
        .expect("first replacement was journalled");
    let mut replaced_plus_what_followed = first_replacement;
    replaced_plus_what_followed.push(assistant_text("followed"));
    assert_eq!(before_drop, replaced_plus_what_followed);
    let resumed_journal = Arc::new(resumed_journal);
    let context = Arc::new(policy(provider.clone(), cfg, ModelOptions::default()));
    let (mut agent, _) = Agent::resume(
        core_parts(
            provider,
            context,
            resumed_journal.clone(),
            Arc::new(RecordingEvents::new()),
        ),
        &resumed.records,
    )
    .unwrap();
    assert_eq!(agent.history(), before_drop);
    let end = timeout(
        LIMIT,
        agent.run_turn("fourth".into(), CancellationToken::new()),
    )
    .await
    .unwrap();
    assert_eq!(
        end,
        TurnEnd::Completed {
            stop: StopReason::EndTurn
        }
    );
    assert!(
        summary_text_if_any(&agent.history()[0])
            .unwrap()
            .contains("second-summary")
    );

    drop(agent);
    drop(resumed_journal);
    let (_, final_load) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    let usages: Vec<Option<Usage>> = final_load
        .records
        .iter()
        .filter_map(|record| match record.body {
            RecordBody::ContextReplaced { usage, .. } => Some(usage),
            _ => None,
        })
        .collect();
    assert_eq!(usages, vec![Some(first_usage), Some(second_usage)]);
}

// DEFAULT_SUMMARIZER_PROMPT contract.
#[tokio::test(start_paused = true)]
async fn default_prompt_names_ordered_sections_and_nearby_carry_forward_and_no_invention_rules() {
    let headings = [
        "## Task",
        "## Constraints and instructions",
        "## Decisions",
        "## State of the work",
        "## Verified facts",
        "## Open problems",
        "## Next step",
    ];
    let mut cursor = 0;
    for heading in headings {
        let offset = DEFAULT_SUMMARIZER_PROMPT[cursor..]
            .find(heading)
            .unwrap_or_else(|| panic!("missing {heading}"));
        cursor += offset + heading.len();
    }
    let lower = DEFAULT_SUMMARIZER_PROMPT.to_lowercase();
    let rules_start = lower.find("## constraints and instructions").unwrap();
    let rules_end = lower.find("## next step").unwrap() + "## next step".len();
    let nearby = &lower[rules_start..rules_end];
    assert!(nearby.contains("carry") || nearby.contains("copied forward"));
    assert!(nearby.contains("never"));
    assert!(nearby.contains("invent"));
}
