//! The shared provider conformance suite, run against the Claude subscription route.
//! Lead-owned acceptance test.

mod fixtures;

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, DeclarationKind, Item, ModelOptions, Provider, ProviderError, ProviderRequest,
    ToolDeclaration,
};
use p1_model_profile::ModelProfile;
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, ROUTE, build_request,
};
use p1_provider_conformance::{RouteFixtures, RouteUnderTest, run_all};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};

const MODEL: &str = "claude-sonnet-5";
const BEARER: &str = "CONFORMANCE-FAKE-BEARER";

/// The shipped route and profile of the model this suite runs: `MODEL` is what
/// `routes/anthropic-subscription.toml` binds `profiles/claude-sonnet-5.toml` to, so
/// these expectations are the adapter's own, only obtained through the files.
fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
    }
}

fn profile() -> Arc<ModelProfile> {
    let text = include_str!("../../../profiles/claude-sonnet-5.toml");
    Arc::new(ModelProfile::from_toml(MODEL, text).expect("the shipped profile is valid"))
}

struct FixedCredentials;

impl CredentialSource for FixedCredentials {
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
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

fn build(transport: ScriptedTransport) -> Arc<dyn Provider> {
    Arc::new(
        AnthropicProvider::new(
            route(),
            MODEL,
            profile(),
            Arc::new(transport),
            Arc::new(FixedCredentials),
        )
        .expect("the shipped profile is expressible on the Messages wire"),
    )
}

fn follow_up_request(request: &ProviderRequest) -> serde_json::Value {
    build_request(&route(), MODEL, &profile(), request).expect("follow-up request builds")
}

/// This route carries JSON-schema function tools only.
fn invalid_request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "conformance prompt".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![ToolDeclaration {
            name: "apply_patch".into(),
            description: "freeform".into(),
            kind: DeclarationKind::Freeform { grammar: None },
        }],
        options: ModelOptions::default(),
    }
}

#[test]
fn conformance() {
    run_all(&RouteUnderTest {
        name: "anthropic-messages/claude-subscription",
        build,
        fixtures: RouteFixtures {
            text_turn: fixtures::text_turn,
            tool_call_turn: fixtures::tool_call_turn,
            two_tool_calls: fixtures::two_tool_calls,
            truncated_tool_call: fixtures::truncated_tool_call,
            invalid_tool_json: fixtures::invalid_tool_json,
            error_event: fixtures::error_event,
            no_usage: fixtures::no_usage,
            reasoning_turn: fixtures::reasoning_turn,
            events_after_terminal: fixtures::events_after_terminal,
        },
        follow_up_request,
        fake_bearer: BEARER,
        invalid_request,
    });
}

/// Lead regression: the fixtures answer as `claude-sonnet-4-6` while the provider is
/// configured as `claude-sonnet-5` — exactly what dated aliases look like live. The
/// conformance run above only passes because origin is the configured model.
#[test]
fn fixtures_echo_a_different_model_than_the_configured_one() {
    assert!(fixtures::reasoning_turn.contains("claude-sonnet-4-6"));
    assert_ne!(MODEL, "claude-sonnet-4-6");
}
