//! The native request is the portable lowering plus the credential (ADR-0071):
//! what `AnthropicProvider` hands the transport equals `lower_request`'s path,
//! headers and body with the `authorization` header at the position it has always
//! had, right after `user-agent`. A provider component and the native provider
//! therefore send the same request.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Provider, ProviderError,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{
    AnthropicProvider, MessagesAccount, MessagesRoute, ROUTE, lower_request,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, HttpRequest};

const BEARER: &str = "LOWERING-TEST-FAKE-BEARER";

struct Resolved;

impl CredentialSource for Resolved {
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
        self.access()
    }
}

fn route(long_context: bool) -> MessagesRoute {
    MessagesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
        long_context,
    }
}

fn profile(thinking: ThinkingPolicy) -> ModelProfile {
    ModelProfile {
        id: "claude-example".into(),
        revision: 1,
        model_id: "claude-example".into(),
        family: "claude".into(),
        thinking,
        efforts: vec![Effort::High],
        default_effort: None,
        thinking_budgets: match thinking {
            ThinkingPolicy::Budget => BTreeMap::from([(Effort::High, 20_480)]),
            _ => BTreeMap::new(),
        },
        context_tokens: None,
        max_output_tokens: None,
    }
}

fn request(effort: Option<Effort>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![ToolDeclaration {
            name: "read".into(),
            description: "Read a file".into(),
            kind: DeclarationKind::Function {
                input_schema: serde_json::json!({ "type": "object" }),
            },
        }],
        options: ModelOptions {
            reasoning_effort: effort,
            ..ModelOptions::default()
        },
    }
}

/// The one request the native provider posts. The first poll reaches the post; the
/// stream is dropped before a retry could send a second one.
async fn posted(
    route: MessagesRoute,
    profile: ModelProfile,
    request: ProviderRequest,
) -> HttpRequest {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = AnthropicProvider::new(
        route,
        "claude-example",
        Arc::new(profile),
        Arc::new(transport.clone()),
        Arc::new(Resolved),
    )
    .expect("the route and profile compose");
    let mut stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("the request is buildable");
    let _ = stream.next().await;
    let mut requests = transport.requests();
    assert_eq!(requests.len(), 1);
    requests.remove(0)
}

#[tokio::test]
async fn the_native_request_is_the_portable_lowering_plus_the_credential() {
    for long_context in [false, true] {
        for thinking in [ThinkingPolicy::Budget, ThinkingPolicy::EffortLevel] {
            for effort in [None, Some(Effort::High)] {
                let case = format!("{long_context} {thinking:?} {effort:?}");
                let lowered = lower_request(
                    &route(long_context),
                    "claude-example",
                    &profile(thinking),
                    &request(effort),
                )
                .expect("the request lowers");
                let native = posted(route(long_context), profile(thinking), request(effort)).await;

                assert_eq!(
                    native.url, "https://api.anthropic.com/v1/messages",
                    "{case}"
                );
                assert_eq!(
                    native.url,
                    format!("{}{}", route(long_context).endpoint, lowered.path),
                    "{case}"
                );
                assert!(native.body == lowered.body, "{case}: the bodies differ");
                assert!(
                    lowered
                        .headers
                        .iter()
                        .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")),
                    "{case}: the lowering carries a credential header"
                );
                let mut expected = lowered.headers.clone();
                let user_agent = expected
                    .iter()
                    .position(|(name, _)| name == "user-agent")
                    .expect("a user-agent header");
                assert_eq!(user_agent, 3, "{case}");
                expected.insert(
                    user_agent + 1,
                    ("authorization".to_string(), format!("Bearer {BEARER}")),
                );
                assert!(native.headers == expected, "{case}: the headers differ");
            }
        }
    }
}

#[test]
fn the_lowering_refuses_what_validate_refuses() {
    let mut refused = request(None);
    refused.options.cache_key = Some("key".into());
    let error = lower_request(
        &route(false),
        "claude-example",
        &profile(ThinkingPolicy::Budget),
        &refused,
    )
    .err()
    .expect("a cache key is refused");
    assert!(error.message.contains("takes no cache key"), "{error}");
}
