//! End-to-end tests through [`p1_provider_http::drive`] plus the cross-cutting
//! invariants (no credential leaks, one terminal event, raw tool input, absent
//! usage, replay gating).

use p1_provider_conformance::fixtures::anthropic as fixtures;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use futures_util::StreamExt;
use p1_contracts::history::{AssistantBlock, Item, ToolInput};
use p1_contracts::{
    BoxFuture, CancellationToken, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, StopReason, StreamEvent,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, ROUTE, build_request,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, RetryPolicy};
use serde_json::json;
use std::collections::BTreeMap;

/// The route data the frozen expectations were recorded with (spec §7.2: the shipped
/// `routes/anthropic-subscription.toml` keeps this origin route byte for byte).
fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
    }
}

/// The profile the OLD model-name rule selected: the three adaptive prefixes took an
/// effort level, every other name took the manual budget table. The mapping documents
/// what the explicit `profiles/claude-*.toml` records replaced.
fn profile(model: &str) -> ModelProfile {
    let effort_level = ["claude-fable-5", "claude-opus-5", "claude-sonnet-5"]
        .iter()
        .any(|prefix| model.starts_with(prefix));
    let efforts = vec![
        p1_contracts::Effort::Low,
        p1_contracts::Effort::Medium,
        p1_contracts::Effort::High,
        p1_contracts::Effort::ExtraHigh,
        p1_contracts::Effort::Max,
    ];
    ModelProfile {
        id: model.to_string(),
        revision: 1,
        model_id: model.to_string(),
        family: "claude".to_string(),
        thinking: if effort_level {
            ThinkingPolicy::EffortLevel
        } else {
            ThinkingPolicy::Budget
        },
        efforts,
        default_effort: None,
        thinking_budgets: if effort_level {
            BTreeMap::new()
        } else {
            [
                (p1_contracts::Effort::Low, 4_096),
                (p1_contracts::Effort::Medium, 10_240),
                (p1_contracts::Effort::High, 20_480),
                (p1_contracts::Effort::ExtraHigh, 32_768),
                (p1_contracts::Effort::Max, 32_768),
            ]
            .into_iter()
            .collect()
        },
        context_tokens: None,
        max_output_tokens: None,
    }
}

/// `build_request` over the route and the profile `model` selects.
fn build(model: &str, request: &ProviderRequest) -> Result<serde_json::Value, ProviderError> {
    build_request(&route(), model, &profile(model), request)
}

/// The adapter composed with the frozen route/profile for `model`.
fn provider(
    model: &str,
    transport: Arc<ScriptedTransport>,
    credentials: Arc<dyn CredentialSource>,
) -> AnthropicProvider {
    AnthropicProvider::new(
        route(),
        model,
        Arc::new(profile(model)),
        transport,
        credentials,
    )
    .expect("the profile is expressible on the Messages wire")
}

struct FakeCredentials {
    bearer: String,
    refresh: Option<String>,
    refresh_calls: AtomicU64,
}

impl FakeCredentials {
    fn new(bearer: &str) -> Arc<Self> {
        Arc::new(Self {
            bearer: bearer.to_string(),
            refresh: None,
            refresh_calls: AtomicU64::new(0),
        })
    }

    fn with_refresh(bearer: &str, refresh: &str) -> Arc<Self> {
        Arc::new(Self {
            bearer: bearer.to_string(),
            refresh: Some(refresh.to_string()),
            refresh_calls: AtomicU64::new(0),
        })
    }
}

impl CredentialSource for FakeCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Ok(Credential {
                bearer: self.bearer.clone(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            self.refresh_calls.fetch_add(1, Ordering::SeqCst);
            match &self.refresh {
                Some(bearer) => Ok(Credential {
                    bearer: bearer.clone(),
                    account_id: None,
                }),
                None => Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "no refresh token scripted",
                )),
            }
        })
    }
}

struct Harness {
    provider: AnthropicProvider,
    transport: ScriptedTransport,
}

fn harness(model: &str, responses: Vec<ScriptedResponse>) -> Harness {
    harness_with(model, responses, FakeCredentials::new("SENTINEL-ACCESS"))
}

fn harness_with(
    model: &str,
    responses: Vec<ScriptedResponse>,
    credentials: Arc<FakeCredentials>,
) -> Harness {
    let transport = ScriptedTransport::new(responses);
    let provider = provider(model, Arc::new(transport.clone()), credentials);
    Harness {
        provider,
        transport,
    }
}

fn request(history: Vec<Item>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "SYS".to_string(),
        history,
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_string(),
    }
}

async fn collect(mut stream: ProviderStream) -> Vec<StreamEvent> {
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

async fn run(harness: &Harness, request: ProviderRequest) -> Vec<StreamEvent> {
    let stream = harness
        .provider
        .stream(request, CancellationToken::new())
        .await
        .expect("a valid request must start a stream");
    collect(stream).await
}

fn finished_count(events: &[StreamEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Finished(_)))
        .count()
}

fn terminal(events: &[StreamEvent]) -> Outcome {
    match events.last() {
        Some(StreamEvent::Finished(outcome)) => outcome.clone(),
        other => panic!("the stream must end with exactly one Finished: {other:?}"),
    }
}

fn completed(events: &[StreamEvent]) -> p1_contracts::CompletedResponse {
    match terminal(events) {
        Outcome::Completed(completed) => completed,
        other => panic!("expected a completed turn, got {other:?}"),
    }
}

fn failed(events: &[StreamEvent]) -> ProviderError {
    match terminal(events) {
        Outcome::Failed(error) => error,
        other => panic!("expected a failed turn, got {other:?}"),
    }
}

fn ok(body: &str) -> ScriptedResponse {
    ScriptedResponse::ok_sse(body)
}

fn status(status: u16) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }
}

fn json_response(status: u16, body: &str) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: vec![body.as_bytes().to_vec()],
        end: BodyEnd::Eof,
    }
}

fn all_fixtures() -> Vec<(&'static str, &'static str)> {
    vec![
        ("text_turn", fixtures::text_turn),
        ("tool_call_turn", fixtures::tool_call_turn),
        ("two_tool_calls", fixtures::two_tool_calls),
        ("truncated_tool_call", fixtures::truncated_tool_call),
        ("invalid_tool_json", fixtures::invalid_tool_json),
        ("error_event", fixtures::error_event),
        ("no_usage", fixtures::no_usage),
        ("reasoning_turn", fixtures::reasoning_turn),
        ("events_after_terminal", fixtures::events_after_terminal),
    ]
}

#[tokio::test]
async fn stop_reasons_map_from_the_wire() {
    for (wire, expected) in [
        ("end_turn", StopReason::EndTurn),
        ("tool_use", StopReason::ToolUse),
        ("max_tokens", StopReason::MaxOutputTokens),
        (
            "model_context_window_exceeded",
            StopReason::ContextWindowExceeded,
        ),
        ("refusal", StopReason::Refusal),
        ("pause_turn", StopReason::Paused),
        ("stop_sequence", StopReason::Other),
        ("something_new", StopReason::Other),
    ] {
        let body = format!(
            "event: message_start\ndata: {{\"type\":\"message_start\",\"message\":{{\"id\":\"m\",\"model\":\"claude-sonnet-4-6\",\"usage\":{{\"input_tokens\":1,\"output_tokens\":1}}}}}}\n\nevent: message_delta\ndata: {{\"type\":\"message_delta\",\"delta\":{{\"stop_reason\":\"{wire}\"}}}}\n\nevent: message_stop\ndata: {{\"type\":\"message_stop\"}}\n\n"
        );
        let harness = harness("claude-sonnet-4-6", vec![ok(&body)]);
        let events = run(&harness, request(vec![user("hi")])).await;
        assert_eq!(completed(&events).stop, expected, "{wire}");
    }
}

#[tokio::test]
async fn text_turn_through_drive() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::text_turn)]);
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(finished_count(&events), 1);
    assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
    assert!(
        events
            .iter()
            .any(|event| matches!(event, StreamEvent::Activity)),
        "ping must surface as Activity: {events:?}"
    );

    let deltas: Vec<(usize, &str)> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { block, text } => Some((*block, text.as_str())),
            _ => None,
        })
        .collect();
    assert_eq!(deltas, vec![(0, "Hello"), (0, " world")]);

    let completed = completed(&events);
    assert_eq!(completed.item.origin.route, ROUTE);
    assert_eq!(completed.item.origin.model, "claude-sonnet-4-6");
    assert_eq!(completed.item.text(), "Hello world");
    assert_eq!(completed.stop, StopReason::EndTurn);

    let usage = completed.usage.expect("usage present in the fixture");
    assert_eq!(usage.input_uncached, Some(10));
    assert_eq!(usage.cache_read, Some(0));
    assert_eq!(usage.cache_write, Some(0));
    assert_eq!(usage.output, Some(7));
    assert_eq!(usage.reasoning_output, None);
    assert_eq!(usage.cost_micro_usd, None);

    let requests = harness.transport.requests();
    assert_eq!(requests.len(), 1);
    assert!(
        requests[0].url.ends_with("/v1/messages"),
        "{}",
        requests[0].url
    );
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer SENTINEL-ACCESS")
    );
    assert!(
        !requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("x-api-key"))
    );
}

#[tokio::test]
async fn tool_call_turn_through_drive() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::tool_call_turn)]);
    let events = run(&harness, request(vec![user("hi")])).await;

    let completed = completed(&events);
    assert_eq!(completed.stop, StopReason::ToolUse);
    let calls: Vec<&p1_contracts::ToolCall> = completed.item.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call_1");
    assert_eq!(calls[0].name, "read");
    assert_eq!(
        calls[0].input,
        ToolInput::Json("{\"path\":\"a.txt\"}".to_string())
    );

    // Display deltas carry the call id but no call is complete before Finished.
    let deltas: Vec<(String, String)> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ToolInputDelta { call_id, text } => Some((call_id.clone(), text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(
        deltas,
        vec![
            ("call_1".to_string(), "{\"path\":".to_string()),
            ("call_1".to_string(), "\"a.txt\"}".to_string()),
        ]
    );
}

#[tokio::test]
async fn tool_call_order_is_preserved() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::two_tool_calls)]);
    let events = run(&harness, request(vec![user("hi")])).await;
    let completed = completed(&events);
    let calls: Vec<&p1_contracts::ToolCall> = completed.item.tool_calls().collect();
    assert_eq!(
        calls
            .iter()
            .map(|call| (call.call_id.as_str(), call.name.as_str()))
            .collect::<Vec<_>>(),
        vec![("call_1", "read"), ("call_2", "grep")]
    );
}

#[tokio::test]
async fn truncated_stream_is_a_failure_and_surfaces_no_call() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::truncated_tool_call)]);
    let events = run(&harness, request(vec![user("hi")])).await;

    let error = failed(&events);
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert_eq!(harness.transport.requests().len(), 1);
    assert!(
        !events.iter().any(|event| matches!(
            event,
            StreamEvent::Finished(Outcome::Completed(completed))
                if completed.item.tool_calls().next().is_some()
        )),
        "a truncated call must never be surfaced: {events:?}"
    );
}

#[tokio::test]
async fn invalid_tool_json_is_preserved_raw() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::invalid_tool_json)]);
    let events = run(&harness, request(vec![user("hi")])).await;
    let completed = completed(&events);
    let calls: Vec<&p1_contracts::ToolCall> = completed.item.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].input, ToolInput::Json("{\"path\": ".to_string()));
    assert_ne!(calls[0].input, ToolInput::Json("{}".to_string()));
}

#[tokio::test]
async fn error_event_is_a_single_failed_terminal_without_body_text() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::error_event)]);
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(finished_count(&events), 1);
    let error = failed(&events);
    assert_eq!(error.kind, ProviderErrorKind::Transport);
    assert!(
        error.message.contains("overloaded_error"),
        "{}",
        error.message
    );
    let rendered = format!("{events:?} {error:?} {}", error);
    assert!(!rendered.contains("SENTINEL-BODY-TEXT"), "{rendered}");
}

#[tokio::test]
async fn absent_usage_is_none_not_zero() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::no_usage)]);
    let events = run(&harness, request(vec![user("hi")])).await;
    let completed = completed(&events);
    assert_eq!(completed.usage, None);
    assert_eq!(completed.stop, StopReason::EndTurn);
}

#[tokio::test]
async fn cache_usage_fields_map_from_the_wire() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::tool_call_turn)]);
    let events = run(&harness, request(vec![user("hi")])).await;
    let usage = completed(&events).usage.expect("usage");
    assert_eq!(usage.input_uncached, Some(25));
    assert_eq!(usage.cache_read, Some(3));
    assert_eq!(usage.cache_write, Some(4));
    assert_eq!(usage.output, Some(9));
}

#[tokio::test]
async fn reasoning_turn_round_trips_replay_data_byte_exact() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::reasoning_turn)]);
    let events = run(&harness, request(vec![user("hi")])).await;

    let reasoning_deltas: Vec<(usize, String)> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::ReasoningDelta { block, text } => Some((*block, text.clone())),
            _ => None,
        })
        .collect();
    assert_eq!(reasoning_deltas, vec![(0, "Let me think".to_string())]);

    let completed = completed(&events);
    assert_eq!(completed.item.blocks.len(), 2);
    let AssistantBlock::Reasoning { text, replay } = &completed.item.blocks[0] else {
        panic!("expected a reasoning block: {:?}", completed.item.blocks);
    };
    assert_eq!(text, "Let me think");
    let replay = replay.as_ref().expect("replay data");
    assert_eq!(replay.origin.route, ROUTE);
    assert_eq!(replay.origin.model, "claude-sonnet-4-6");
    assert_eq!(replay.version, 1);
    assert_eq!(replay.payload["type"], json!("thinking"));
    assert_eq!(replay.payload["signature"], json!("sig-abc"));
    assert_eq!(completed.item.text(), "Answer");

    // The replay data goes back byte-exact in a follow-up request.
    let follow_up = build(
        "claude-sonnet-4-6",
        &request(vec![user("next"), Item::Assistant(completed.item.clone())]),
    )
    .unwrap();
    let blocks = follow_up["messages"][1]["content"].as_array().unwrap();
    assert!(blocks.contains(&json!({
        "type": "thinking",
        "thinking": "Let me think",
        "signature": "sig-abc",
    })));
}

#[tokio::test]
async fn nothing_after_terminal_is_surfaced() {
    let harness = harness(
        "claude-sonnet-4-6",
        vec![ok(fixtures::events_after_terminal)],
    );
    let mut stream = harness
        .provider
        .stream(request(vec![user("hi")]), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }

    assert_eq!(finished_count(&events), 1);
    assert!(matches!(events.last(), Some(StreamEvent::Finished(_))));
    assert_eq!(completed(&events).item.text(), "done");
    assert!(
        !events.iter().any(|event| matches!(
            event,
            StreamEvent::TextDelta { text, .. } if text == "STRAY"
        )),
        "stray events after the terminal must be dropped: {events:?}"
    );
    assert!(stream.next().await.is_none());
}

/// Invariant: exactly one terminal event, and it is last, for every fixture.
#[tokio::test]
async fn exactly_one_terminal_event_per_fixture() {
    for (name, body) in all_fixtures() {
        let harness = harness("claude-sonnet-4-6", vec![ok(body)]);
        let events = run(&harness, request(vec![user("hi")])).await;
        assert_eq!(finished_count(&events), 1, "{name}: {events:?}");
        assert!(
            matches!(events.last(), Some(StreamEvent::Finished(_))),
            "{name}: {events:?}"
        );
    }
}

#[tokio::test]
async fn chunking_at_every_byte_offset_is_irrelevant() {
    for (name, body) in all_fixtures() {
        let expected = run(
            &harness("claude-sonnet-4-6", vec![ok(body)]),
            request(vec![user("hi")]),
        )
        .await;
        for at in 0..=body.len() {
            let harness = harness(
                "claude-sonnet-4-6",
                vec![ScriptedResponse::ok_sse_split(body, at)],
            );
            let events = run(&harness, request(vec![user("hi")])).await;
            assert_eq!(events, expected, "{name}: split at byte offset {at}");
        }
    }
}

#[tokio::test(start_paused = true)]
async fn http_401_refreshes_once_then_fails_authentication() {
    let credentials = FakeCredentials::with_refresh("SENTINEL-ACCESS", "SENTINEL-REFRESH");
    let harness = harness_with(
        "claude-sonnet-4-6",
        vec![status(401), status(401)],
        credentials.clone(),
    );
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(harness.transport.requests().len(), 2);
    assert_eq!(credentials.refresh_calls.load(Ordering::SeqCst), 1);
    let error = failed(&events);
    assert_eq!(error.kind, ProviderErrorKind::Authentication);

    // The refreshed credential was actually used on the second request.
    let requests = harness.transport.requests();
    assert!(
        requests[1]
            .headers
            .iter()
            .any(|(name, value)| { name == "authorization" && value == "Bearer SENTINEL-REFRESH" })
    );
}

#[tokio::test(start_paused = true)]
async fn http_429_retries_then_succeeds() {
    let harness = harness(
        "claude-sonnet-4-6",
        vec![status(429), ok(fixtures::text_turn)],
    );
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(harness.transport.requests().len(), 2);
    assert_eq!(completed(&events).item.text(), "Hello world");
}

#[tokio::test(start_paused = true)]
async fn http_500_exhausts_the_budget_then_fails_transport() {
    let harness = harness(
        "claude-sonnet-4-6",
        vec![status(500), status(500), status(500), status(500)],
    );
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(harness.transport.requests().len(), 4);
    assert_eq!(failed(&events).kind, ProviderErrorKind::Transport);
}

#[tokio::test(start_paused = true)]
async fn http_400_is_invalid_request_without_retry() {
    let harness = harness(
        "claude-sonnet-4-6",
        vec![json_response(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"bad"}}"#,
        )],
    );
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(harness.transport.requests().len(), 1);
    assert_eq!(failed(&events).kind, ProviderErrorKind::InvalidRequest);
}

#[tokio::test(start_paused = true)]
async fn http_400_context_window_is_classified_by_the_message() {
    let harness = harness(
        "claude-sonnet-4-6",
        vec![json_response(
            400,
            r#"{"error":{"type":"invalid_request_error","message":"prompt is too long: 300000 tokens"}}"#,
        )],
    );
    let events = run(&harness, request(vec![user("hi")])).await;
    assert_eq!(
        failed(&events).kind,
        ProviderErrorKind::ContextWindowExceeded
    );
}

#[tokio::test(start_paused = true)]
async fn http_error_message_names_status_type_and_request_id_but_no_body() {
    let mut response = json_response(
        400,
        r#"{"error":{"type":"invalid_request_error","message":"SENTINEL-BODY-TEXT"}}"#,
    );
    response
        .headers
        .push(("request-id".to_string(), "req_test_123".to_string()));
    let harness = harness("claude-sonnet-4-6", vec![response]);
    let events = run(&harness, request(vec![user("hi")])).await;

    let error = failed(&events);
    assert!(error.message.contains("400"), "{}", error.message);
    assert!(
        error.message.contains("invalid_request_error"),
        "{}",
        error.message
    );
    assert!(error.message.contains("req_test_123"), "{}", error.message);
    assert!(
        !error.message.contains("SENTINEL-BODY-TEXT"),
        "{}",
        error.message
    );
}

#[tokio::test(start_paused = true)]
async fn no_retry_after_visible_output() {
    let broken = ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            b"data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"text_delta\",\"text\":\"partial\"}}\n\n"
                .to_vec(),
        ],
        end: BodyEnd::Error("connection reset".to_string()),
    };
    let harness = harness("claude-sonnet-4-6", vec![broken, ok(fixtures::text_turn)]);
    let events = run(&harness, request(vec![user("hi")])).await;

    assert_eq!(harness.transport.requests().len(), 1);
    assert_eq!(failed(&events).kind, ProviderErrorKind::Transport);
}

#[tokio::test(start_paused = true)]
async fn cancel_before_the_first_byte_is_cancelled_once() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::text_turn)]);
    let cancel = CancellationToken::new();
    cancel.cancel();
    let stream = harness
        .provider
        .stream(request(vec![user("hi")]), cancel)
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(finished_count(&events), 1);
    assert!(matches!(terminal(&events), Outcome::Cancelled));
    assert_eq!(harness.transport.requests().len(), 0);
}

/// Invariant 9a: no credential value or response-body text reaches an error, a
/// `Debug` output or a request `Debug`.
#[tokio::test(start_paused = true)]
async fn credentials_and_bodies_never_leak() {
    let credentials = FakeCredentials::with_refresh("SENTINEL-ACCESS", "SENTINEL-REFRESH");

    // A hostile 400 body containing a sentinel that must never be copied.
    let mut hostile = json_response(
        400,
        r#"{"error":{"type":"invalid_request_error","message":"SENTINEL-BODY-TEXT"}}"#,
    );
    hostile
        .headers
        .push(("x-secret".to_string(), "SENTINEL-HEADER".to_string()));
    let harness = harness_with("claude-sonnet-4-6", vec![hostile], credentials.clone());
    let events = run(&harness, request(vec![user("hi")])).await;
    let error = failed(&events);
    let request_debug = format!("{:?}", harness.transport.requests()[0]);
    for rendered in [
        format!("{events:?}"),
        format!("{error:?}"),
        error.to_string(),
        request_debug,
    ] {
        for sentinel in ["SENTINEL-ACCESS", "SENTINEL-BODY-TEXT", "SENTINEL-HEADER"] {
            assert!(
                !rendered.contains(sentinel),
                "leaked {sentinel}: {rendered}"
            );
        }
    }

    // The 401 path exercises the refreshed (rotating) token too.
    let harness = harness_with(
        "claude-sonnet-4-6",
        vec![status(401), status(401)],
        credentials,
    );
    let events = run(&harness, request(vec![user("hi")])).await;
    let error = failed(&events);
    for rendered in [format!("{events:?}"), error.to_string()] {
        assert!(!rendered.contains("SENTINEL-REFRESH"), "leaked: {rendered}");
        assert!(!rendered.contains("SENTINEL-ACCESS"), "leaked: {rendered}");
    }
}

// A fixed clock for the file-credential end-to-end test.
static FILE_CLOCK: AtomicU64 = AtomicU64::new(1_700_000_000_000);
#[allow(dead_code)]
fn file_clock() -> u64 {
    FILE_CLOCK.load(Ordering::SeqCst)
}

/// `with_base_url`, `with_retry` and the custom base URL all the way through
/// `drive`.
#[tokio::test]
async fn base_url_and_retry_policy_are_applied_through_drive() {
    let harness = harness("claude-sonnet-4-6", vec![ok(fixtures::text_turn)]);
    let provider = provider(
        "claude-sonnet-4-6",
        Arc::new(harness.transport.clone()),
        FakeCredentials::new("SENTINEL-ACCESS"),
    )
    .with_base_url("https://alt.example.test/")
    .with_retry(RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    });
    let stream = provider
        .stream(request(vec![user("hi")]), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(completed(&events).item.text(), "Hello world");
    assert_eq!(
        harness.transport.requests()[0].url,
        "https://alt.example.test/v1/messages"
    );
}

/// The real [`ClaudeCodeCredentials`] source feeding `drive` end to end lives in
/// `p1-auth`, which this adapter must not depend on: `tests/credentials_end_to_end.rs`
/// in `p1-host` proves that composition instead (the host is the one place that
/// composes). The `#[tokio::test]` below covers the same wire path with a fake
/// source, so the adapter's own behaviour stays pinned here.
#[tokio::test]
async fn a_fake_file_shaped_source_drives_a_request_end_to_end() {
    let transport = ScriptedTransport::new(vec![ok(fixtures::text_turn)]);
    let credentials = FakeCredentials::new("FILE-TOKEN");
    let provider = provider(
        "claude-sonnet-4-6",
        Arc::new(transport.clone()),
        credentials,
    );
    let stream = provider
        .stream(request(vec![user("hi")]), CancellationToken::new())
        .await
        .unwrap();
    let events = collect(stream).await;

    assert_eq!(completed(&events).item.text(), "Hello world");
    assert!(
        transport.requests()[0]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer FILE-TOKEN"),
        "the source's token must be used verbatim"
    );
}
