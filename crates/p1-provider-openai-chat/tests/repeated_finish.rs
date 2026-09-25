//! OpenRouter-style repeated finish choices in the final usage chunk.
use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Outcome, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, StreamEvent,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{ChatDialect, ChatLimits, ChatProvider, ChatRoute};
use serde_json::json;
use std::sync::Arc;

struct Fixed;
impl CredentialSource for Fixed {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: "TEST-FAKE-BEARER".into(),
                account_id: None,
            })
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        unreachable!()
    }
}

fn provider(response: ScriptedResponse) -> Arc<dyn Provider> {
    let route = ChatRoute {
        origin_route: "openai-chat/repeated-finish".into(),
        endpoint: "https://example.test/v1/chat/completions".into(),
        headers: vec![],
        session_header: None,
        dialect: ChatDialect::ThinkingWithReasoningAlias,
        limits: ChatLimits::default(),
        client_identity: None,
    };
    let profile = ModelProfile {
        id: "test-model".into(),
        revision: 1,
        model_id: "test-model".into(),
        family: "test".into(),
        thinking: ThinkingPolicy::Enabled,
        efforts: vec![Effort::High],
        default_effort: Some(Effort::High),
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    };
    Arc::new(
        ChatProvider::new(
            route,
            "test-model",
            Arc::new(profile),
            Arc::new(ScriptedTransport::new(vec![response])),
            Arc::new(Fixed),
        )
        .unwrap(),
    )
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: String::new(),
        history: vec![],
        tools: vec![],
        options: ModelOptions::default(),
    }
}

async fn events(transcript: &str) -> Vec<StreamEvent> {
    let provider = provider(ScriptedResponse::ok_sse(transcript));
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .unwrap();
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn first_finish(reason: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{\"content\":\"OK\"}},\"finish_reason\":\"{reason}\"}}],\"usage\":{{\"completion_tokens\":7}}}}\n\n"
    )
}

fn repeated(
    transcript: String,
    delta: serde_json::Value,
    reason: Option<&str>,
    usage: bool,
) -> String {
    let mut choice = json!({"index": 0, "delta": delta});
    if let Some(reason) = reason {
        choice["finish_reason"] = json!(reason);
    }
    let mut chunk = json!({"choices": [choice]});
    if usage {
        chunk["usage"] = json!({"completion_tokens": 11});
    }
    transcript + &format!("data: {chunk}\n\n")
}

#[tokio::test]
async fn repeated_empty_finish_with_usage_keeps_one_finish_and_last_usage() {
    let transcript = format!(
        "{}data: [DONE]\n\n",
        repeated(
            first_finish("stop"),
            json!({"content": "", "role": "assistant", "reasoning": null}),
            Some("stop"),
            true,
        )
    );
    let events = events(&transcript).await;
    let finished: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Finished(Outcome::Completed(response)) => Some(response),
            _ => None,
        })
        .collect();
    assert_eq!(finished.len(), 1, "{events:?}");
    assert_eq!(finished[0].item.text(), "OK");
    assert_eq!(finished[0].usage.unwrap().output, Some(11));
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, StreamEvent::Finished(_)))
            .count(),
        1
    );
}

async fn assert_repeated_is_protocol_error(delta: serde_json::Value, reason: Option<&str>) {
    let events = events(&repeated(first_finish("stop"), delta, reason, false)).await;
    let failures: Vec<_> = events
        .iter()
        .filter_map(|event| match event {
            StreamEvent::Finished(Outcome::Failed(error)) => Some(error),
            _ => None,
        })
        .collect();
    assert_eq!(failures.len(), 1, "{events:?}");
    assert_eq!(failures[0].kind, ProviderErrorKind::Protocol, "{events:?}");
}

#[tokio::test]
async fn repeated_finish_carrying_text_is_a_protocol_error() {
    assert_repeated_is_protocol_error(json!({"content": "more"}), Some("stop")).await;
}

#[tokio::test]
async fn repeated_finish_carrying_a_tool_call_delta_is_a_protocol_error() {
    assert_repeated_is_protocol_error(
        json!({"tool_calls": [{"index": 0, "id": "call_1", "function": {"name": "read", "arguments": "{}"}}]}),
        Some("stop"),
    )
    .await;
}

#[tokio::test]
async fn repeated_finish_carrying_a_legacy_function_call_is_a_protocol_error() {
    assert_repeated_is_protocol_error(
        json!({"function_call": {"name": "read", "arguments": "{}"}}),
        Some("stop"),
    )
    .await;
}

#[tokio::test]
async fn repeated_finish_carrying_an_unknown_non_empty_delta_is_a_protocol_error() {
    assert_repeated_is_protocol_error(json!({"refusal": "no"}), Some("stop")).await;
}

#[tokio::test]
async fn repeated_finish_with_a_different_reason_is_a_protocol_error() {
    assert_repeated_is_protocol_error(json!({"content": ""}), Some("length")).await;
}

#[tokio::test]
async fn repeated_finish_without_a_reason_is_a_protocol_error() {
    assert_repeated_is_protocol_error(json!({"content": ""}), None).await;
}
