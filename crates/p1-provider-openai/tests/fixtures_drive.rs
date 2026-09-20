//! End-to-end checks: every route fixture driven through `drive` with a
//! `ScriptedTransport`, asserting the contract-shaped events.

mod fixtures;

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::history::{AssistantBlock, Item, ToolInput};
use p1_contracts::{
    BoxFuture, CancellationToken, CompletedResponse, Effort, ModelOptions, Outcome, Provider,
    ProviderError, ProviderErrorKind, ProviderRequest, StopReason, StreamEvent, ToolDeclaration,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, RetryPolicy};
use p1_provider_openai::{OpenAiCodexProvider, ROUTE, ResponsesAccount, ResponsesRoute};

const MODEL: &str = "gpt-test";
const BEARER: &str = "SENTINEL-ACCESS";

/// The route data these fixtures were recorded with (spec §7.2).
fn route() -> ResponsesRoute {
    ResponsesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://chatgpt.com/backend-api".to_string(),
        account: ResponsesAccount::CodexSubscription,
    }
}

/// The model policy these fixtures were recorded with: any model name took an
/// effort level, and only `low`/`medium`/`high` (spec §7.1).
fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: MODEL.to_string(),
        revision: 1,
        model_id: MODEL.to_string(),
        family: "gpt".to_string(),
        thinking: ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::Low, Effort::Medium, Effort::High],
        default_effort: None,
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    })
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Ok(Credential {
                bearer: BEARER.to_string(),
                account_id: Some("acct_test".to_string()),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Ok(Credential {
                bearer: BEARER.to_string(),
                account_id: Some("acct_test".to_string()),
            })
        })
    }
}

fn provider_with(body: &'static str) -> (OpenAiCodexProvider, ScriptedTransport) {
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(body)]);
    let provider = OpenAiCodexProvider::new(
        route(),
        MODEL,
        profile(),
        Arc::new(transport.clone()),
        Arc::new(FixedCredentials),
    )
    .expect("the route and the profile compose");
    (provider, transport)
}

fn request(history: Vec<Item>, tools: Vec<ToolDeclaration>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "You are a coding assistant.".to_string(),
        history,
        tools,
        options: ModelOptions::default(),
    }
}

fn user(text: &str) -> Item {
    Item::User {
        text: text.to_string(),
    }
}

async fn drive(provider: &OpenAiCodexProvider, request: ProviderRequest) -> Vec<StreamEvent> {
    let mut stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("setup must succeed");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn finished_count(events: &[StreamEvent]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, StreamEvent::Finished(_)))
        .count()
}

fn terminal(events: &[StreamEvent]) -> &Outcome {
    assert_eq!(finished_count(events), 1, "{events:?}");
    match events.last() {
        Some(StreamEvent::Finished(outcome)) => outcome,
        other => panic!("the terminal event is not last: {other:?}"),
    }
}

fn completed(events: &[StreamEvent]) -> &CompletedResponse {
    match terminal(events) {
        Outcome::Completed(response) => response,
        other => panic!("expected completion, got {other:?}"),
    }
}

fn text_deltas(events: &[StreamEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::TextDelta { text, .. } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn text_turn_deltas_terminate_once_with_usage() {
    let (provider, transport) = provider_with(fixtures::TEXT_TURN);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;

    assert_eq!(text_deltas(&events), vec!["Hello", " world"]);
    let response = completed(&events);
    assert_eq!(response.item.text(), "Hello world");
    assert_eq!(response.stop, StopReason::EndTurn);
    assert_eq!(response.item.origin.route, ROUTE);
    assert_eq!(response.item.origin.model, MODEL);
    let usage = response.usage.expect("usage present");
    assert_eq!(usage.input_uncached, Some(60));
    assert_eq!(usage.cache_read, Some(40));
    assert_eq!(usage.cache_write, None);
    assert_eq!(usage.output, Some(20));
    assert_eq!(usage.reasoning_output, Some(5));
    assert_eq!(usage.cost_micro_usd, None);

    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn tool_call_is_complete_and_only_in_the_terminal() {
    let (provider, _) = provider_with(fixtures::TOOL_CALL_TURN);
    let events = drive(&provider, request(vec![user("read a.txt")], Vec::new())).await;

    // The call's input is streamed for display only; the call itself is only in
    // the terminal event.
    assert!(events.iter().any(|event| matches!(
        event,
        StreamEvent::ToolInputDelta { call_id, .. } if call_id == "call_1"
    )));

    let response = completed(&events);
    assert_eq!(response.stop, StopReason::ToolUse);
    assert_eq!(response.item.text(), "I will read it.");
    let calls: Vec<_> = response.item.tool_calls().collect();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].call_id, "call_1");
    assert_eq!(calls[0].name, "read");
    assert_eq!(
        calls[0].input,
        ToolInput::Json(r#"{"path":"a.txt"}"#.to_string())
    );
}

#[tokio::test]
async fn two_tool_calls_keep_their_order() {
    let (provider, _) = provider_with(fixtures::TWO_TOOL_CALLS);
    let events = drive(&provider, request(vec![user("search")], Vec::new())).await;
    let response = completed(&events);
    let ids: Vec<_> = response
        .item
        .tool_calls()
        .map(|call| call.call_id.as_str())
        .collect();
    assert_eq!(ids, vec!["call_1", "call_2"]);
}

#[tokio::test]
async fn truncated_stream_is_a_failure_and_surfaces_no_call() {
    let (provider, transport) = provider_with(fixtures::TRUNCATED_TOOL_CALL);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;

    assert_eq!(finished_count(&events), 1);
    match terminal(&events) {
        Outcome::Failed(error) => assert_eq!(error.kind, ProviderErrorKind::Transport),
        other => panic!("expected a transport failure, got {other:?}"),
    }
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::Finished(Outcome::Completed(_)))),
        "a truncated stream must not complete"
    );
    // The visible tool-input delta means the driver must not retry.
    assert_eq!(transport.requests().len(), 1);
}

#[tokio::test]
async fn invalid_tool_json_is_preserved_raw() {
    let (provider, _) = provider_with(fixtures::INVALID_TOOL_JSON);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;
    let response = completed(&events);
    let call = response.item.tool_calls().next().expect("call");
    assert_eq!(call.input, ToolInput::Json(r#"{"path": "#.to_string()));
    assert_ne!(call.input, ToolInput::Json("{}".to_string()));
}

#[tokio::test]
async fn error_event_is_a_single_failed_terminal_without_body_text() {
    let (provider, _) = provider_with(fixtures::ERROR_EVENT);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;

    assert_eq!(finished_count(&events), 1);
    match terminal(&events) {
        Outcome::Failed(error) => {
            assert_eq!(error.kind, ProviderErrorKind::InvalidRequest);
            assert!(!error.message.contains("SENTINEL-BODY"), "{error:?}");
        }
        other => panic!("expected a failure, got {other:?}"),
    }
    // Prior visible deltas are preserved.
    assert_eq!(text_deltas(&events), vec!["partial"]);
}

#[tokio::test]
async fn absent_usage_stays_none() {
    let (provider, _) = provider_with(fixtures::NO_USAGE);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;
    let response = completed(&events);
    assert_eq!(response.item.text(), "ok");
    assert!(
        response.usage.is_none(),
        "absent usage must be None, not zero"
    );
}

#[tokio::test]
async fn reasoning_turn_round_trips_replay() {
    let (provider, _) = provider_with(fixtures::REASONING_TURN);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;
    let response = completed(&events);

    match &response.item.blocks[0] {
        AssistantBlock::Reasoning { text, replay } => {
            assert_eq!(text, "first part\n\nsecond part");
            let replay = replay.as_ref().expect("replay data");
            assert_eq!(replay.origin.route, ROUTE);
            assert_eq!(replay.origin.model, MODEL);
            assert_eq!(replay.version, 1);
            assert_eq!(replay.payload["encrypted_content"], "enc-1");
        }
        other => panic!("expected reasoning, got {other:?}"),
    }
    assert_eq!(response.item.text(), "answer");

    // Round trip: the replayed item appears byte-exact in the follow-up request.
    let history = vec![Item::Assistant(response.item.clone()), user("continue")];
    let (provider, transport) = provider_with(fixtures::NO_USAGE);
    let _ = drive(&provider, request(history, Vec::new())).await;
    let body: serde_json::Value = serde_json::from_slice(&transport.requests()[0].body).unwrap();
    assert_eq!(
        body["input"][0],
        serde_json::json!({
            "type": "reasoning",
            "encrypted_content": "enc-1",
            "summary": [],
        })
    );

    // A foreign origin is dropped, not downgraded to text.
    let mut foreign = response.item.clone();
    if let AssistantBlock::Reasoning { replay, .. } = &mut foreign.blocks[0] {
        let replay = replay.as_mut().unwrap();
        replay.origin.route = "other-route".to_string();
    }
    let (provider, transport) = provider_with(fixtures::NO_USAGE);
    let _ = drive(
        &provider,
        request(vec![Item::Assistant(foreign), user("continue")], Vec::new()),
    )
    .await;
    let body: serde_json::Value = serde_json::from_slice(&transport.requests()[0].body).unwrap();
    assert!(
        body["input"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["type"] != "reasoning"),
        "foreign replay must be absent: {body}"
    );
}

#[tokio::test]
async fn nothing_is_surfaced_after_the_terminal_event() {
    let (provider, _) = provider_with(fixtures::EVENTS_AFTER_TERMINAL);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;
    assert_eq!(finished_count(&events), 1);
    assert_eq!(completed(&events).item.text(), "done");
    assert!(
        !events
            .iter()
            .any(|event| matches!(event, StreamEvent::TextDelta { text, .. } if text == "stray")),
        "stray events after the terminal must not be surfaced"
    );
}

#[tokio::test]
async fn every_fixture_has_exactly_one_terminal_event() {
    for body in [
        fixtures::TEXT_TURN,
        fixtures::TOOL_CALL_TURN,
        fixtures::TWO_TOOL_CALLS,
        fixtures::TRUNCATED_TOOL_CALL,
        fixtures::INVALID_TOOL_JSON,
        fixtures::ERROR_EVENT,
        fixtures::NO_USAGE,
        fixtures::REASONING_TURN,
        fixtures::EVENTS_AFTER_TERMINAL,
    ] {
        let (provider, _) = provider_with(body);
        let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;
        assert_eq!(finished_count(&events), 1, "fixture: {body}");
        assert!(
            matches!(events.last(), Some(StreamEvent::Finished(_))),
            "the terminal must be last: {body}"
        );
    }
}

#[tokio::test]
async fn headers_and_request_body_are_sent_exactly() {
    let (provider, transport) = provider_with(fixtures::NO_USAGE);
    let provider = provider.with_base_url("https://example.test/backend");
    let _ = drive(&provider, request(vec![user("hello")], Vec::new())).await;

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://example.test/backend/codex/responses"
    );
    let headers = &requests[0].headers;
    let value = |name: &str| {
        headers
            .iter()
            .find(|(candidate, _)| candidate.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.clone())
    };
    assert_eq!(
        value("Authorization").as_deref(),
        Some("Bearer SENTINEL-ACCESS")
    );
    assert_eq!(value("chatgpt-account-id").as_deref(), Some("acct_test"));
    assert_eq!(value("originator").as_deref(), Some("p1"));
    assert_eq!(
        value("User-Agent").as_deref(),
        Some(concat!("p1/", env!("CARGO_PKG_VERSION")))
    );
    assert_eq!(
        value("OpenAI-Beta").as_deref(),
        Some("responses=experimental")
    );
    assert_eq!(value("Content-Type").as_deref(), Some("application/json"));
    assert_eq!(value("Accept").as_deref(), Some("text/event-stream"));

    let body: serde_json::Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(
        body,
        serde_json::json!({
            "model": MODEL,
            "store": false,
            "stream": true,
            "instructions": "You are a coding assistant.",
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": "hello" }],
            }],
            "text": { "verbosity": "low" },
        })
    );
}

#[tokio::test]
async fn with_retry_bounds_transient_failures() {
    let transport = ScriptedTransport::new(vec![
        ScriptedResponse {
            status: 500,
            headers: Vec::new(),
            chunks: Vec::new(),
            end: p1_provider_http::testing::BodyEnd::Eof,
        },
        ScriptedResponse::ok_sse(fixtures::NO_USAGE),
    ]);
    let provider = OpenAiCodexProvider::new(
        route(),
        MODEL,
        profile(),
        Arc::new(transport.clone()),
        Arc::new(FixedCredentials),
    )
    .expect("the route and the profile compose")
    .with_retry(RetryPolicy {
        max_retries: 0,
        ..RetryPolicy::default()
    });
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;
    assert_eq!(
        transport.requests().len(),
        1,
        "max_retries 0 forbids a retry"
    );
    assert!(matches!(
        terminal(&events),
        Outcome::Failed(error) if error.kind == ProviderErrorKind::Transport
    ));
}

#[tokio::test]
async fn credentials_never_leak_through_debug_or_events() {
    let (provider, _) = provider_with(fixtures::TEXT_TURN);
    let events = drive(&provider, request(vec![user("hi")], Vec::new())).await;

    let descriptions = [
        format!("{provider:?}"),
        provider.describe().origin.route.clone(),
        format!("{events:?}"),
    ];
    for text in descriptions {
        assert!(!text.contains(BEARER), "leaked bearer: {text}");
    }

    let error = ProviderError::new(ProviderErrorKind::Transport, "SENTINEL-BODY");
    assert!(!format!("{error:?}").contains(BEARER));
}

#[test]
fn describe_reports_the_expected_route() {
    let (provider, _) = provider_with(fixtures::TEXT_TURN);
    let description = provider.describe();
    assert_eq!(description.origin.route, ROUTE);
    assert_eq!(description.origin.model, MODEL);
    assert!(description.supports_freeform_tools);
    assert!(description.mandatory_prompt_prefix.is_none());
    assert!(!description.reports_cost);
}
