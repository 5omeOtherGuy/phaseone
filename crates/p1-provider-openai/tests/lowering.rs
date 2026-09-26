//! The native request is the portable lowering plus the credential (ADR-0071):
//! what `OpenAiCodexProvider` hands the transport on its SSE path equals
//! `lower_request`'s path, headers and body with `Authorization` and the account-id
//! header in front, where they have always been. A provider component and the
//! native provider therefore send the same request.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Provider, ProviderError,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, HttpRequest};
use p1_provider_openai::{
    OpenAiCodexProvider, ROUTE, ResponsesAccount, ResponsesRoute, ResponsesTransport, lower_request,
};

const BEARER: &str = "LOWERING-TEST-FAKE-BEARER";
const ACCOUNT_ID: &str = "LOWERING-TEST-FAKE-ACCOUNT";

struct Resolved;

impl CredentialSource for Resolved {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: BEARER.into(),
                account_id: Some(ACCOUNT_ID.into()),
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

fn route(endpoint: &str) -> ResponsesRoute {
    ResponsesRoute {
        origin_route: ROUTE.to_string(),
        endpoint: endpoint.to_string(),
        account: ResponsesAccount::CodexSubscription,
        transport: ResponsesTransport::Sse,
    }
}

fn profile() -> ModelProfile {
    ModelProfile {
        id: "gpt-test".into(),
        revision: 1,
        model_id: "gpt-test".into(),
        family: "gpt".into(),
        thinking: ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::Low, Effort::Medium, Effort::High],
        default_effort: None,
        thinking_budgets: BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    }
}

fn request(effort: Option<Effort>, cache_key: Option<String>) -> ProviderRequest {
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
            cache_key,
            ..ModelOptions::default()
        },
    }
}

/// The one request the native provider posts. The first poll reaches the post; the
/// stream is dropped before a retry could send a second one.
async fn posted(route: ResponsesRoute, request: ProviderRequest) -> HttpRequest {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = OpenAiCodexProvider::new(
        route,
        "gpt-test",
        Arc::new(profile()),
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
    for endpoint in [
        "https://chatgpt.com/backend-api",
        "https://example.test/codex/",
        "https://example.test/codex/responses",
    ] {
        for effort in [None, Some(Effort::High)] {
            for cache_key in [None, Some("k".repeat(80))] {
                let case = format!(
                    "{endpoint} {effort:?} {:?}",
                    cache_key.as_ref().map(String::len)
                );
                let request = request(effort, cache_key);
                let lowered = lower_request(&route(endpoint), "gpt-test", &profile(), &request)
                    .expect("the request lowers");
                let native = posted(route(endpoint), request).await;

                assert_eq!(
                    native.url,
                    format!("{}{}", endpoint.trim_end_matches('/'), lowered.path),
                    "{case}"
                );
                assert!(native.url.ends_with("/codex/responses"), "{case}");
                assert!(native.body == lowered.body, "{case}: the bodies differ");
                assert!(
                    lowered.headers.iter().all(|(name, _)| {
                        !name.eq_ignore_ascii_case("authorization")
                            && !name.eq_ignore_ascii_case("chatgpt-account-id")
                    }),
                    "{case}: the lowering carries a credential header"
                );
                let mut expected = vec![
                    ("Authorization".to_string(), format!("Bearer {BEARER}")),
                    ("chatgpt-account-id".to_string(), ACCOUNT_ID.to_string()),
                ];
                expected.extend(lowered.headers.clone());
                assert!(native.headers == expected, "{case}: the headers differ");
            }
        }
    }
}

#[test]
fn the_lowering_refuses_what_validate_refuses() {
    let error = lower_request(
        &route("https://chatgpt.com/backend-api"),
        "gpt-test",
        &profile(),
        &request(None, Some(String::new())),
    )
    .err()
    .expect("an empty cache key is refused");
    assert!(error.message.contains("cache_key"), "{error}");
}

#[test]
fn the_account_names_the_header_its_credential_id_travels_in() {
    assert_eq!(
        ResponsesAccount::CodexSubscription.account_id_header(),
        Some("chatgpt-account-id")
    );
}
