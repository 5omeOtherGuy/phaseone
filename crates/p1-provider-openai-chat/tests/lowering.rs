//! The native request is the portable lowering plus the credential (ADR-0071):
//! what `ChatProvider` hands the transport equals `lower_request`'s path, headers
//! and body with the `authorization` header at the position it has always had,
//! right after `accept`. A provider component and the native provider therefore
//! send the same request.

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Effort, Item, ModelOptions, Provider,
    ProviderError, ProviderRequest, ToolDeclaration,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, HttpRequest};
use p1_provider_openai_chat::{
    ChatDialect, ChatLimits, ChatProvider, ChatRoute, ClientIdentity, lower_request,
};

const MODEL: &str = "configured-model";
const BEARER: &str = "LOWERING-TEST-FAKE-BEARER";
/// The ids an identity generates afresh for every request that has no cache key.
const GENERATED: [&str; 2] = ["x-opencode-session", "x-opencode-request"];

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
        self.access()
    }
}

fn route(identity: Option<ClientIdentity>) -> ChatRoute {
    ChatRoute {
        origin_route: "openai-chat/opencode-go-subscription".into(),
        endpoint: "https://example.test/v1/chat/completions".into(),
        headers: vec![
            ("user-agent".into(), "p1/test".into()),
            ("x-static".into(), "yes".into()),
        ],
        session_header: Some("x-session".into()),
        dialect: ChatDialect::ThinkingWithReasoningAlias,
        client_identity: identity,
        limits: ChatLimits::default(),
    }
}

fn profile() -> ModelProfile {
    ModelProfile {
        id: MODEL.into(),
        revision: 1,
        model_id: MODEL.into(),
        family: "test".into(),
        thinking: ThinkingPolicy::Enabled,
        efforts: vec![Effort::High, Effort::Max],
        default_effort: Some(Effort::High),
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    }
}

fn request(cache_key: Option<&str>) -> ProviderRequest {
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
            cache_key: cache_key.map(str::to_string),
            ..ModelOptions::default()
        },
    }
}

/// The one request the native provider posts. The first poll reaches the post; the
/// stream is dropped before a retry could send a second one.
async fn posted(route: ChatRoute, request: ProviderRequest) -> HttpRequest {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = ChatProvider::new(
        route,
        MODEL,
        Arc::new(profile()),
        Arc::new(transport.clone()),
        Arc::new(Fixed),
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
    for identity in [None, Some(ClientIdentity::Opencode)] {
        for cache_key in [None, Some("session-key")] {
            let case = format!("{identity:?} {cache_key:?}");
            let lowered = lower_request(&route(identity), MODEL, &profile(), &request(cache_key))
                .expect("the request lowers");
            let native = posted(route(identity), request(cache_key)).await;

            assert_eq!(
                native.url,
                format!("{}{}", route(identity).endpoint, lowered.path),
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
            let accept = expected
                .iter()
                .position(|(name, _)| name == "accept")
                .expect("an accept header");
            expected.insert(
                accept + 1,
                ("authorization".to_string(), format!("Bearer {BEARER}")),
            );
            // Without a cache key an identity's ids are fresh per request, so only
            // their presence and position can match.
            let fresh_ids = identity.is_some() && cache_key.is_none();
            let comparable = |headers: &[(String, String)]| -> Vec<(String, String)> {
                headers
                    .iter()
                    .map(|(name, value)| {
                        if fresh_ids && GENERATED.contains(&name.as_str()) {
                            (name.clone(), String::new())
                        } else {
                            (name.clone(), value.clone())
                        }
                    })
                    .collect()
            };
            assert!(
                comparable(&native.headers) == comparable(&expected),
                "{case}: the headers differ"
            );
        }
    }
}

#[test]
fn the_lowering_refuses_what_validate_refuses() {
    let mut refused = request(None);
    refused.options.reasoning_effort = Some(Effort::Low);
    let error = lower_request(&route(None), MODEL, &profile(), &refused)
        .err()
        .expect("an effort the profile does not list is refused");
    assert_eq!(error.kind, p1_contracts::ProviderErrorKind::InvalidRequest);
}
