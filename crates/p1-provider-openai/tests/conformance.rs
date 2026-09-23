//! The shared provider conformance suite, run against the Codex subscription route.
//! Lead-owned acceptance test.

use std::sync::Arc;

use p1_contracts::{BoxFuture, Item, ModelOptions, Provider, ProviderError, ProviderRequest};
use p1_model_profile::ModelProfile;
use p1_provider_conformance::{
    RouteFixtures, RouteUnderTest, fixtures::responses as fixtures, run_all,
};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{
    OpenAiCodexProvider, ROUTE, ResponsesAccount, ResponsesRoute, ResponsesTransport, build_request,
};

const MODEL: &str = "gpt-5.6-sol";
const BEARER: &str = "CONFORMANCE-FAKE-BEARER";

/// The shipped route and profile of the model this suite runs: `MODEL` is what
/// `routes/openai-codex-subscription.toml` binds `profiles/gpt-5.6-sol.toml` to,
/// so these expectations are the adapter's own, only obtained through the files.
fn route() -> ResponsesRoute {
    ResponsesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://chatgpt.com/backend-api".to_string(),
        account: ResponsesAccount::CodexSubscription,
        transport: ResponsesTransport::Sse,
    }
}

fn profile() -> Arc<ModelProfile> {
    let text = include_str!("../../../profiles/gpt-5.6-sol.toml");
    Arc::new(ModelProfile::from_toml(MODEL, text).expect("the shipped profile is valid"))
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: Some("acct-conformance".into()),
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: Some("acct-conformance".into()),
            })
        })
    }
}

fn build(transport: ScriptedTransport) -> Arc<dyn Provider> {
    Arc::new(
        OpenAiCodexProvider::new(
            route(),
            MODEL,
            profile(),
            Arc::new(transport),
            Arc::new(FixedCredentials),
        )
        .expect("shipped route and profile compose"),
    )
}

fn follow_up_request(request: &ProviderRequest) -> serde_json::Value {
    build_request(&route(), MODEL, &profile(), request).expect("follow-up request builds")
}

/// This route rejects an explicit output cap (`max_output_tokens` is a 400 on the wire).
fn invalid_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "conformance prompt".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: Vec::new(),
        options: ModelOptions {
            max_output_tokens: Some(1000),
            ..ModelOptions::default()
        },
    }
}

#[test]
fn conformance() {
    run_all(&RouteUnderTest {
        name: "openai-responses/codex-subscription",
        build,
        fixtures: RouteFixtures {
            text_turn: fixtures::TEXT_TURN,
            tool_call_turn: fixtures::TOOL_CALL_TURN,
            two_tool_calls: fixtures::TWO_TOOL_CALLS,
            truncated_tool_call: fixtures::TRUNCATED_TOOL_CALL,
            invalid_tool_json: fixtures::INVALID_TOOL_JSON,
            error_event: fixtures::ERROR_EVENT,
            no_usage: fixtures::NO_USAGE,
            reasoning_turn: fixtures::REASONING_TURN,
            events_after_terminal: fixtures::EVENTS_AFTER_TERMINAL,
        },
        follow_up_request,
        fake_bearer: BEARER,
        invalid_request,
    });
}
