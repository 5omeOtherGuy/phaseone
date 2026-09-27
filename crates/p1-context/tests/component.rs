//! The context-policy component `p1/context/summarizing` behind `WasmContextPolicy`, with
//! `ProviderSummary` over the scripted provider as its summary service, against the native
//! `SummarizingContext` on the same inputs: the frozen suite's core cases give equal items,
//! usage, errors and provider requests on both. Also: the summary service is never
//! re-entered and runs outside the task that owns the component's Store.
//!
//! The component is read from `modules/target/p1-modules/p1-module-context/`; a missing
//! build fails the case with how to build it, it never skips.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use p1_context::{
    ContextConfig, DEFAULT_SUMMARY_OUTPUT_TOKENS, ProviderSummary, SUMMARY_MARKER,
    SummarizingContext, estimate_tokens,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, Compaction, ContextError,
    ContextInput, ContextPolicy, Effort, Item, ModelOptions, Prepared, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, ReplayData, RouteDescription, StopReason,
    StreamEvent, ToolResultItem, ToolStatus, Usage,
};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ReleaseManifest, SummaryError, SummaryRequest,
    SummaryResponse, SummaryService, WasmContextPolicy,
};
use p1_testkit::{ScriptedProvider, Step, completed, json_call, origin, text_block, text_response};
use tokio::sync::Notify;
use tokio::time::timeout;

/// A hang guard only: every wait below ends on an explicit event.
const LIMIT: Duration = Duration::from_secs(60);

const PACKAGE: (&str, &str) = ("p1-module-context", "p1/context/summarizing");

// ------------------------------------------------------------------ the built component

fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// The built component, loaded once through a release manifest as a release lays it out.
fn module() -> &'static LoadedModule {
    static MODULE: OnceLock<LoadedModule> = OnceLock::new();
    MODULE.get_or_init(|| {
        let path = built()
            .join(PACKAGE.0)
            .join(format!("{}.manifest.json", PACKAGE.0));
        let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        });
        let manifest: Value = serde_json::from_str(&text).expect("the package manifest is JSON");
        let entry = json!({
            "name": manifest["name"],
            "digest": manifest["digest"],
            "path": format!("{0}/{0}.wasm", PACKAGE.0),
            "kind": manifest["kind"],
            "world": manifest["world"],
            "protocol": manifest["protocol"],
            "capabilities": manifest["capabilities"],
            "variant": manifest["variant"],
        });
        let release = json!({ "format": "p1-release-manifest/1", "components": [entry] });
        let release = ReleaseManifest::parse(&release.to_string()).expect("release manifest");
        Loader::new(release, built())
            .expect("loader")
            .load(PACKAGE.1)
            .expect("the built context policy loads")
    })
}

/// `configure`'s settings for `config`, the summary cap and the agent's own limit.
fn settings(config: &ContextConfig, cap: u64, options: &ModelOptions) -> String {
    let mut settings = json!({
        "window_tokens": config.window_tokens,
        "output_headroom_tokens": config.output_headroom_tokens,
        "summarize_at_tokens": config.summarize_at_tokens,
        "keep_recent_tokens": config.keep_recent_tokens,
        "user_verbatim_tokens": config.user_verbatim_tokens,
        "tool_result_excerpt_chars": config.tool_result_excerpt_chars,
        "summary_output_tokens": cap,
    });
    if let Some(limit) = options.max_output_tokens {
        settings["max_output_tokens"] = json!(limit);
    }
    settings.to_string()
}

fn component(
    config: &ContextConfig,
    cap: u64,
    options: &ModelOptions,
    service: Arc<dyn SummaryService>,
) -> WasmContextPolicy {
    WasmContextPolicy::new(
        module(),
        &settings(config, cap, options),
        service,
        ExecutionLimits::default(),
    )
    .expect("the component accepts the settings")
}

// ------------------------------------------------------------------ both policies

/// The native policy and the component, each over its own provider built by the same
/// factory, so both see the same script.
struct Both {
    native: SummarizingContext,
    native_provider: Arc<ScriptedProvider>,
    wasm: WasmContextPolicy,
    wasm_provider: Arc<ScriptedProvider>,
}

type Factory = dyn Fn() -> (Arc<dyn Provider>, Arc<ScriptedProvider>);

fn both(make: &Factory, config: ContextConfig, cap: u64, options: ModelOptions) -> Both {
    let (native_route, native_provider) = make();
    let native = SummarizingContext::new(
        native_route,
        options.clone(),
        config.clone(),
        "summary prompt".into(),
    )
    .unwrap()
    .with_summary_output_tokens(cap)
    .unwrap();
    let (wasm_route, wasm_provider) = make();
    let service = ProviderSummary::new(wasm_route, options.clone(), "summary prompt".into());
    let wasm = component(&config, cap, &options, Arc::new(service));
    Both {
        native,
        native_provider,
        wasm,
        wasm_provider,
    }
}

fn scripted(script: Vec<Step>) -> impl Fn() -> (Arc<dyn Provider>, Arc<ScriptedProvider>) {
    move || {
        let provider = Arc::new(ScriptedProvider::new(script.clone()));
        (provider.clone() as Arc<dyn Provider>, provider)
    }
}

/// What a preparation gave, comparable across the two policies.
type Outcome = Result<Option<(Vec<Item>, Option<Usage>)>, ContextError>;

fn outcome(result: Result<Option<Prepared>, ContextError>) -> Outcome {
    result.map(|prepared| prepared.map(|prepared| (prepared.items, prepared.usage)))
}

async fn prepare_on(
    policy: &dyn ContextPolicy,
    history: &[Item],
    usage: Option<&Usage>,
    cancel: &CancellationToken,
) -> Outcome {
    let result = timeout(
        LIMIT,
        policy.prepare(ContextInput {
            history,
            last_usage: usage,
            cancel,
        }),
    )
    .await
    .expect("prepare hung");
    outcome(result)
}

/// What a compaction gave: the history after it and the estimates before and after.
type Compacted = Result<(Vec<Item>, u64, u64), ContextError>;

async fn compact_on(
    policy: &dyn ContextPolicy,
    history: &[Item],
    cancel: &CancellationToken,
) -> Compacted {
    let result = timeout(
        LIMIT,
        policy.compact_now(ContextInput {
            history,
            last_usage: None,
            cancel,
        }),
    )
    .await
    .expect("compact-now hung");
    result.map(|compaction| match compaction {
        Compaction::Replaced {
            prepared,
            tokens_before,
            tokens_after,
        } => (prepared.items, tokens_before, tokens_after),
        Compaction::Unchanged { tokens } => (history.to_vec(), tokens, tokens),
    })
}

impl Both {
    /// Prepares on both, checks they agree on the answer and on every provider request,
    /// and returns the answer.
    async fn prepare(&self, history: &[Item], usage: Option<&Usage>) -> Outcome {
        let cancel = CancellationToken::new();
        let native = prepare_on(&self.native, history, usage, &cancel).await;
        let wasm = prepare_on(&self.wasm, history, usage, &cancel).await;
        assert_eq!(wasm, native, "the component and the native policy differ");
        self.same_requests();
        wasm
    }

    async fn compact_now(&self, history: &[Item]) -> Compacted {
        let cancel = CancellationToken::new();
        let native = compact_on(&self.native, history, &cancel).await;
        let wasm = compact_on(&self.wasm, history, &cancel).await;
        assert_eq!(wasm, native, "the component and the native policy differ");
        self.same_requests();
        wasm
    }

    fn same_requests(&self) {
        assert_eq!(
            self.wasm_provider.requests(),
            self.native_provider.requests(),
            "the component's summary requests differ from the native ones"
        );
        assert_eq!(
            self.wasm_provider.validated(),
            self.native_provider.validated()
        );
    }

    fn caps(&self) -> Vec<Option<u32>> {
        self.wasm_provider
            .requests()
            .iter()
            .map(|request| request.options.max_output_tokens)
            .collect()
    }
}

// ------------------------------------------------------------------ builders

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

fn call(id: &str) -> Item {
    assistant(vec![AssistantBlock::ToolCall(json_call(
        id,
        "read",
        r#"{"path":"src/lib.rs"}"#,
    ))])
}

fn result(id: &str, content: impl Into<String>) -> Item {
    Item::ToolResult(ToolResultItem {
        call_id: id.into(),
        name: "read".into(),
        status: ToolStatus::Ok,
        content: content.into(),
    })
}

fn summary(text: &str, stop: StopReason, usage: Option<Usage>) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(completed(vec![text_block(text)], stop, usage)),
    ])
}

fn usage(input: u64, output: u64) -> Usage {
    Usage {
        input_uncached: Some(input),
        cache_read: Some(1),
        cache_write: None,
        output: Some(output),
        reasoning_output: None,
        cost_micro_usd: Some(3),
    }
}

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

/// Summarizes as soon as the history is bigger than one token, with a wall far above.
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

/// Summarizes at once and sits exactly at the wall: a failure is fatal.
fn wall_config(history: &[Item]) -> ContextConfig {
    let total = estimate_tokens(history);
    ContextConfig {
        window_tokens: total + 1_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: total - 1,
        keep_recent_tokens: 20,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

/// A history of whole tool-call/result units, bigger than the tail keeps.
fn unit_history() -> Vec<Item> {
    vec![
        user("task: fix the parser"),
        call("c1"),
        result("c1", "old ".repeat(300)),
        call("c2"),
        result("c2", "older ".repeat(300)),
        call("c3"),
        result("c3", "recent"),
        assistant_text("tail"),
    ]
}

/// Every result follows its call, and every call but a final one has its result.
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
                    "call {} has no result",
                    call.call_id
                );
            }
        }
    }
}

// ------------------------------------------------------------------ the core cases

#[tokio::test]
async fn below_the_threshold_no_request_is_made() {
    let policy = both(
        &scripted(vec![]),
        config(),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let history = vec![user("task"), assistant_text("short")];
    assert_eq!(policy.prepare(&history, None).await, Ok(None));
    // Known usage below the threshold, too.
    assert_eq!(
        policy.prepare(&history, Some(&usage(100, 10))).await,
        Ok(None)
    );
    assert!(policy.wasm_provider.requests().is_empty());
}

#[tokio::test]
async fn at_the_threshold_one_summary_replaces_the_prefix_with_whole_units() {
    let history = unit_history();
    let policy = both(
        &scripted(vec![summary(
            "the summary",
            StopReason::EndTurn,
            Some(usage(40, 7)),
        )]),
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let (items, used) = policy
        .prepare(&history, None)
        .await
        .unwrap()
        .expect("a replacement");
    assert_eq!(policy.wasm_provider.requests().len(), 1);
    assert_eq!(items[0], user(format!("{SUMMARY_MARKER}\nthe summary")));
    assert_eq!(items[1], user("task: fix the parser"));
    assert_eq!(items.last(), history.last());
    assert!(items.len() < history.len());
    assert_pairing(&items);
    assert_eq!(used, Some(usage(40, 7)));
}

#[tokio::test]
async fn a_truncated_summary_gets_the_one_cap_doubling_retry_with_both_usages_summed() {
    let history = unit_history();
    let first = usage(10, 4_000);
    let second = Usage {
        cache_read: None,
        ..usage(20, 8)
    };
    let policy = both(
        &scripted(vec![
            summary("cut off", StopReason::MaxOutputTokens, Some(first)),
            summary("whole", StopReason::EndTurn, Some(second)),
        ]),
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let (items, used) = policy.prepare(&history, None).await.unwrap().unwrap();
    assert_eq!(policy.caps(), vec![Some(4_000), Some(8_000)]);
    assert_eq!(items[0], user(format!("{SUMMARY_MARKER}\nwhole")));
    assert_eq!(
        used,
        Some(Usage {
            input_uncached: Some(30),
            cache_read: None,
            cache_write: None,
            output: Some(4_008),
            reasoning_output: None,
            cost_micro_usd: Some(6),
        })
    );
}

#[tokio::test]
async fn the_agents_smaller_limit_is_sent_first_and_doubled() {
    let history = unit_history();
    let options = ModelOptions {
        max_output_tokens: Some(300),
        ..ModelOptions::default()
    };
    let policy = both(
        &scripted(vec![
            summary("cut", StopReason::MaxOutputTokens, None),
            summary("cut", StopReason::MaxOutputTokens, None),
        ]),
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        options,
    );
    // Truncated twice below the wall: soft.
    assert_eq!(policy.prepare(&history, None).await, Ok(None));
    assert_eq!(policy.caps(), vec![Some(300), Some(600)]);
}

/// A route that refuses `max_output_tokens` (the Codex one).
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

#[tokio::test]
async fn a_route_refusing_the_cap_is_retried_once_without_it() {
    let history = unit_history();
    let make = || {
        let inner = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
        let route: Arc<dyn Provider> = Arc::new(RejectExplicitMax {
            inner: inner.clone(),
        });
        (route, inner)
    };
    let policy = both(
        &make,
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let prepared = policy.prepare(&history, None).await.unwrap();
    assert!(prepared.is_some());
    assert_eq!(policy.caps(), vec![None]);
}

#[tokio::test]
async fn a_failure_below_the_wall_is_soft() {
    let history = unit_history();
    let policy = both(
        &scripted(vec![Step::SetupError(ProviderError::new(
            ProviderErrorKind::Transport,
            "summary unavailable",
        ))]),
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    assert_eq!(policy.prepare(&history, None).await, Ok(None));
    assert_eq!(policy.wasm_provider.requests().len(), 1);
}

#[tokio::test]
async fn a_failure_at_the_wall_fails_naming_both_numbers() {
    let history = unit_history();
    let config = wall_config(&history);
    let next = estimate_tokens(&history);
    let policy = both(
        &scripted(vec![Step::SetupError(ProviderError::new(
            ProviderErrorKind::Transport,
            "summary unavailable",
        ))]),
        config,
        100,
        ModelOptions::default(),
    );
    match policy.prepare(&history, None).await {
        Err(ContextError::Failed(reason)) => assert_eq!(
            reason,
            format!(
                "context is full ({next} of {next} tokens) and summarizing failed: Transport: summary unavailable"
            )
        ),
        other => panic!("expected a failure at the wall, got {other:?}"),
    }
}

#[tokio::test]
async fn an_empty_answer_is_refused() {
    let history = unit_history();
    let policy = both(
        &scripted(vec![summary("", StopReason::EndTurn, None)]),
        wall_config(&history),
        100,
        ModelOptions::default(),
    );
    match policy.prepare(&history, None).await {
        Err(ContextError::Failed(reason)) => {
            assert!(
                reason.ends_with("the summarization produced an empty answer"),
                "{reason}"
            )
        }
        other => panic!("expected a failure at the wall, got {other:?}"),
    }
}

#[tokio::test]
async fn nothing_to_summarize_makes_no_request() {
    let tail = assistant_text("x".repeat(400));
    let history = vec![user(format!("{SUMMARY_MARKER}\nold summary")), tail.clone()];
    let mut config = force_config(&history);
    config.keep_recent_tokens = estimate_tokens(std::slice::from_ref(&tail));
    let policy = both(
        &scripted(vec![]),
        config,
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    assert_eq!(policy.prepare(&history, None).await, Ok(None));
    assert!(policy.wasm_provider.requests().is_empty());
}

#[tokio::test]
async fn compact_now_is_a_no_op_on_a_short_history_and_replaces_a_long_one() {
    let short = vec![user("task"), assistant_text("hi")];
    let policy = both(
        &scripted(vec![text_response("compacted")]),
        config(),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let (items, before, after) = policy.compact_now(&short).await.unwrap();
    assert_eq!(items, short);
    assert_eq!(before, estimate_tokens(&short));
    assert_eq!(after, before);
    assert!(policy.wasm_provider.requests().is_empty());

    // Whatever the threshold says, one summary is made.
    let long = unit_history();
    let (items, before, after) = policy.compact_now(&long).await.unwrap();
    assert_eq!(policy.wasm_provider.requests().len(), 1);
    assert_eq!(items[0], user(format!("{SUMMARY_MARKER}\ncompacted")));
    assert_eq!(before, estimate_tokens(&long));
    assert_eq!(after, estimate_tokens(&items));
    assert_pairing(&items);
}

#[tokio::test]
async fn cancellation_while_the_summary_is_in_flight_is_cancelled() {
    let history = unit_history();
    let policy = both(
        &scripted(vec![Step::EventsThenHang(vec![])]),
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    for (name, provider, policy) in [
        (
            "native",
            &policy.native_provider,
            &policy.native as &dyn ContextPolicy,
        ),
        ("component", &policy.wasm_provider, &policy.wasm),
    ] {
        let cancel = CancellationToken::new();
        let drained = provider.drained.clone();
        let (answer, ()) = tokio::join!(prepare_on(policy, &history, None, &cancel), async {
            timeout(LIMIT, drained.notified())
                .await
                .expect("the summary stream never started");
            cancel.cancel();
        });
        assert_eq!(answer, Err(ContextError::Cancelled), "{name}");
    }
}

#[tokio::test]
async fn a_credential_shaped_summary_is_masked_in_the_replacement() {
    let history = unit_history();
    let secret = format!("sk-proj-{}", "Q".repeat(32));
    let policy = both(
        &scripted(vec![text_response(&format!(
            "## Decisions\nkeep {secret}\n"
        ))]),
        force_config(&history),
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let (items, _) = policy.prepare(&history, None).await.unwrap().unwrap();
    let Item::User { text } = &items[0] else {
        panic!("the summary item comes first");
    };
    assert!(text.starts_with(SUMMARY_MARKER));
    assert!(text.contains("## Decisions"));
    assert!(
        !text.contains(&secret),
        "the summary leaked the value: {text}"
    );
}

#[tokio::test]
async fn replay_data_of_kept_items_is_retained() {
    let replay = ReplayData {
        origin: origin(),
        version: 9,
        payload: json!({"signature":[0,255,"opaque"]}),
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
    let history = vec![
        user("task\0with\r\nexact bytes"),
        assistant_text("old ".repeat(500)),
        user("constraint: preserve λ"),
        tail.clone(),
    ];
    let mut config = force_config(&history);
    config.keep_recent_tokens = estimate_tokens(std::slice::from_ref(&tail));
    let policy = both(
        &scripted(vec![text_response("SCRIPTED ANSWER")]),
        config,
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        ModelOptions::default(),
    );
    let (items, _) = policy.prepare(&history, None).await.unwrap().unwrap();
    assert_eq!(
        items,
        vec![
            user(format!("{SUMMARY_MARKER}\nSCRIPTED ANSWER")),
            user("task\0with\r\nexact bytes"),
            user("constraint: preserve λ"),
            tail,
        ]
    );
}

// ------------------------------------------------------------------ the summary service

#[tokio::test]
async fn provider_summary_sends_the_cap_as_given_at_the_summary_effort() {
    let provider = Arc::new(ScriptedProvider::new(vec![summary(
        "done",
        StopReason::EndTurn,
        None,
    )]));
    let options = ModelOptions {
        max_output_tokens: Some(100),
        reasoning_effort: Some(Effort::High),
        ..ModelOptions::default()
    };
    let service = ProviderSummary::new(provider.clone(), options, "summary prompt".into())
        .with_summary_effort(Some(Effort::Low));
    let answer = SummaryService::summarize(
        &service,
        SummaryRequest {
            transcript: "the transcript".into(),
            max_output_tokens: Some(8_000),
        },
        CancellationToken::new(),
    )
    .await;
    assert_eq!(
        answer,
        Ok(SummaryResponse {
            text: "done".into(),
            stop: StopReason::EndTurn,
            usage: None,
        })
    );
    let request = &provider.requests()[0];
    // Never lowered to the agent's own limit: the component chose it.
    assert_eq!(request.options.max_output_tokens, Some(8_000));
    assert_eq!(request.options.reasoning_effort, Some(Effort::Low));
    assert_eq!(request.system_prompt, "summary prompt");
    assert_eq!(request.history, vec![user("the transcript")]);
    assert!(request.tools.is_empty());
}

#[tokio::test]
async fn provider_summary_reports_a_validation_refusal_as_refused() {
    let error = ProviderError::new(ProviderErrorKind::InvalidRequest, "no cap here");
    let provider = Arc::new(ScriptedProvider::new(vec![]).rejecting_validation(error.clone()));
    let service = ProviderSummary::new(provider.clone(), ModelOptions::default(), "p".into());
    let answer = SummaryService::summarize(
        &service,
        SummaryRequest {
            transcript: "t".into(),
            max_output_tokens: Some(1),
        },
        CancellationToken::new(),
    )
    .await;
    assert_eq!(answer, Err(SummaryError::Refused(error)));
    assert!(provider.requests().is_empty(), "nothing was sent");
}

/// Records what runs, in order, through one shared log.
type Log = Arc<Mutex<Vec<&'static str>>>;

struct LoggingService {
    inner: ProviderSummary,
    log: Log,
    depth: Arc<Mutex<u32>>,
}

impl SummaryService for LoggingService {
    fn summarize(
        &self,
        request: SummaryRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>> {
        Box::pin(async move {
            {
                let mut depth = self.depth.lock().unwrap();
                *depth += 1;
                assert_eq!(*depth, 1, "a summary started inside another");
            }
            self.log.lock().unwrap().push("summary begins");
            let answer = SummaryService::summarize(&self.inner, request, cancel).await;
            self.log.lock().unwrap().push("summary ends");
            *self.depth.lock().unwrap() -= 1;
            answer
        })
    }
}

/// The policy with every entry and exit of its exports logged.
struct LoggingPolicy {
    inner: WasmContextPolicy,
    log: Log,
}

impl ContextPolicy for LoggingPolicy {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push("prepare begins");
            let answer = self.inner.prepare(input).await;
            self.log.lock().unwrap().push("prepare ends");
            answer
        })
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(async move {
            self.log.lock().unwrap().push("compact-now begins");
            let answer = self.inner.compact_now(input).await;
            self.log.lock().unwrap().push("compact-now ends");
            answer
        })
    }
}

#[tokio::test]
async fn the_summary_service_is_never_re_entered_and_runs_no_prepare() {
    let history = unit_history();
    let config = force_config(&history);
    let provider = Arc::new(ScriptedProvider::new(vec![
        summary("cut", StopReason::MaxOutputTokens, None),
        text_response("whole"),
        text_response("compacted"),
    ]));
    let log: Log = Arc::default();
    let service = LoggingService {
        inner: ProviderSummary::new(provider.clone(), ModelOptions::default(), "p".into()),
        log: log.clone(),
        depth: Arc::default(),
    };
    let policy = LoggingPolicy {
        inner: component(
            &config,
            DEFAULT_SUMMARY_OUTPUT_TOKENS,
            &ModelOptions::default(),
            Arc::new(service),
        ),
        log: log.clone(),
    };
    let cancel = CancellationToken::new();
    let prepared = prepare_on(&policy, &history, None, &cancel).await;
    assert!(matches!(prepared, Ok(Some(_))), "{prepared:?}");
    let compacted = timeout(
        LIMIT,
        policy.compact_now(ContextInput {
            history: &history,
            last_usage: None,
            cancel: &cancel,
        }),
    )
    .await
    .expect("compact-now hung");
    assert!(matches!(compacted, Ok(Compaction::Replaced { .. })));
    assert_eq!(
        *log.lock().unwrap(),
        vec![
            "prepare begins",
            "summary begins",
            "summary ends",
            "summary begins",
            "summary ends",
            "prepare ends",
            "compact-now begins",
            "summary begins",
            "summary ends",
            "compact-now ends",
        ],
        "a summary in flight ran no export of the policy"
    );
}

/// Holds each summary until the test releases it.
struct GatedService {
    inner: ProviderSummary,
    entered: Arc<Notify>,
    release: Arc<Notify>,
}

impl SummaryService for GatedService {
    fn summarize(
        &self,
        request: SummaryRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            SummaryService::summarize(&self.inner, request, cancel).await
        })
    }
}

#[tokio::test]
async fn the_store_owner_keeps_serving_while_a_summary_is_in_flight() {
    let history = unit_history();
    let config = force_config(&history);
    let provider = Arc::new(ScriptedProvider::new(vec![text_response("s")]));
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Notify::new());
    let policy = component(
        &config,
        DEFAULT_SUMMARY_OUTPUT_TOKENS,
        &ModelOptions::default(),
        Arc::new(GatedService {
            inner: ProviderSummary::new(provider.clone(), ModelOptions::default(), "p".into()),
            entered: entered.clone(),
            release: release.clone(),
        }),
    );
    let cancel = CancellationToken::new();
    let short = vec![user("hi")];
    let (first, second) = tokio::join!(prepare_on(&policy, &history, None, &cancel), async {
        timeout(LIMIT, entered.notified())
            .await
            .expect("the summary never started");
        // The first call waits in `summary.summarize`; the policy still answers another.
        let second = prepare_on(&policy, &short, None, &cancel).await;
        release.notify_one();
        second
    });
    assert_eq!(second, Ok(None));
    assert!(matches!(first, Ok(Some(_))), "{first:?}");
    assert_eq!(provider.requests().len(), 1);
}
