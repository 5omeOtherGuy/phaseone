//! Hand-written fixtures only; each subscription passes THE shared suite unchanged.
use p1_contracts::{BoxFuture, Effort, ModelOptions, Provider, ProviderError, ProviderRequest};
use p1_provider_conformance::{RouteFixtures, RouteUnderTest, run_all};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{ChatProvider, SubscriptionRoute, build_request};
use std::sync::Arc;
const MODEL: &str = "configured-model";
const BEARER: &str = "CONFORMANCE-FAKE-BEARER";
struct Fixed;
impl CredentialSource for Fixed {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: None,
            })
        })
    }
    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}
fn go(t: ScriptedTransport) -> Arc<dyn Provider> {
    Arc::new(ChatProvider::new(
        SubscriptionRoute::OpenCodeGo,
        MODEL,
        Arc::new(t),
        Arc::new(Fixed),
    ))
}
fn glm(t: ScriptedTransport) -> Arc<dyn Provider> {
    Arc::new(ChatProvider::new(
        SubscriptionRoute::Glm,
        MODEL,
        Arc::new(t),
        Arc::new(Fixed),
    ))
}
fn go_request(r: &ProviderRequest) -> serde_json::Value {
    build_request(SubscriptionRoute::OpenCodeGo, MODEL, r).unwrap()
}
fn glm_request(r: &ProviderRequest) -> serde_json::Value {
    build_request(SubscriptionRoute::Glm, MODEL, r).unwrap()
}
fn invalid() -> ProviderRequest {
    ProviderRequest {
        system_prompt: String::new(),
        history: vec![],
        tools: vec![],
        options: ModelOptions {
            reasoning_effort: Some(Effort::Medium),
            ..ModelOptions::default()
        },
    }
}
fn fixtures() -> RouteFixtures {
    RouteFixtures {
        text_turn: include_str!("fixtures/text.sse"),
        tool_call_turn: include_str!("fixtures/tool.sse"),
        two_tool_calls: include_str!("fixtures/two_tools.sse"),
        truncated_tool_call: include_str!("fixtures/truncated.sse"),
        invalid_tool_json: include_str!("fixtures/invalid_json.sse"),
        error_event: include_str!("fixtures/error.sse"),
        no_usage: include_str!("fixtures/no_usage.sse"),
        reasoning_turn: include_str!("fixtures/reasoning.sse"),
        events_after_terminal: include_str!("fixtures/after_terminal.sse"),
    }
}
#[test]
fn opencode_go_conformance() {
    run_all(&RouteUnderTest {
        name: "opencode-go",
        build: go,
        fixtures: fixtures(),
        follow_up_request: go_request,
        fake_bearer: BEARER,
        invalid_request: invalid,
    });
}
#[test]
fn glm_conformance() {
    run_all(&RouteUnderTest {
        name: "glm",
        build: glm,
        fixtures: fixtures(),
        follow_up_request: glm_request,
        fake_bearer: BEARER,
        invalid_request: invalid,
    });
}
