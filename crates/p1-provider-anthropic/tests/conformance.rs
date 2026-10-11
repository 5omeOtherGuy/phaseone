//! The shared provider conformance suite, run against the Claude subscription route.
//! Lead-owned acceptance test.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, DeclarationKind, Item, ModelOptions, Provider, ProviderError, ProviderRequest,
    ToolDeclaration,
};
use p1_model_profile::ModelProfile;
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, ROUTE, build_request,
};
use p1_provider_conformance::{
    RouteFixtures, RouteUnderTest, fixtures::anthropic as fixtures, run_all,
};
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
        long_context: false,
    }
}

/// Copied from `profiles/claude-sonnet-5.toml` on 2026-10-11; independent of it now,
/// so a moved or edited file cannot break this target at compile time (#844).
const SHIPPED_PROFILE: &str = r#"# What the model is, independent of the route that reaches it (ADR-0039).
id       = "claude-sonnet-5"
revision = 2
model_id = "claude-sonnet-5"
family   = "claude"
thinking = "effort-level"

efforts = ["low", "medium", "high", "extra_high", "max"]
# The Messages API takes PNG, JPEG, GIF and WebP images (ADR-0180, #694).
input_modalities = ["text", "image"]

# default_effort: absent — an effort-less request carries no thinking fields.
# context_tokens and max_output_tokens: unknown for this model, so stated nowhere.
"#;

fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile::from_toml(MODEL, SHIPPED_PROFILE).expect("the shipped profile is valid"))
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
        max_retries: 3,
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
