//! Issue #134: a route whose credential an egress proxy injects sends NO
//! authentication header. This is the chat adapter's half of the claim — the source
//! says `proxy_injected()`, and the request that reaches the transport carries no
//! `authorization` header at all. The contrast case pins the other half: a route that
//! resolves its own credential still sends it.

use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, ModelOptions, Provider, ProviderError, ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{ChatDialect, ChatLimits, ChatProvider, ChatRoute};

const BEARER: &str = "NONE-TEST-FAKE-BEARER";

/// A source that resolves a credential of its own.
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

/// The source of a route whose credential an egress proxy injects: the placeholder
/// `p1_auth::resolve` hands every adapter for `kind = "none"`.
struct ProxyInjected;

impl CredentialSource for ProxyInjected {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Ok(Credential {
                bearer: String::new(),
                account_id: None,
            })
        })
    }

    fn refresh<'a>(
        &'a self,
        _: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::new(
                p1_contracts::ProviderErrorKind::Authentication,
                "a route that sends no credential has nothing to refresh",
            ))
        })
    }

    fn proxy_injected(&self) -> bool {
        true
    }
}

fn route() -> ChatRoute {
    ChatRoute {
        origin_route: "openai-chat/proxy-route".into(),
        endpoint: "https://example.test/v1/chat/completions".into(),
        headers: vec![],
        session_header: Some("x-session".into()),
        client_identity: None,
        dialect: ChatDialect::ThinkingWithReasoningAlias,
        limits: ChatLimits::default(),
    }
}

fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: "canonical-model".into(),
        revision: 1,
        model_id: "canonical-model".into(),
        family: "test".into(),
        thinking: ThinkingPolicy::Enabled,
        efforts: vec![Effort::High],
        default_effort: Some(Effort::High),
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    })
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![],
        tools: vec![],
        options: ModelOptions {
            cache_key: Some("proxy-key".into()),
            ..ModelOptions::default()
        },
    }
}

/// One request's headers, as the transport recorded them. The scripted body ends
/// right after, so the first poll reaches the post and nothing is retried.
async fn posted_headers(credentials: Arc<dyn CredentialSource>) -> Vec<(String, String)> {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = ChatProvider::new(
        route(),
        "canonical-model",
        profile(),
        Arc::new(transport.clone()),
        credentials,
    )
    .expect("the route and profile compose");
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("the request is buildable");
    let _ = stream.next().await;

    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    requests.into_iter().next().unwrap().headers
}

#[tokio::test]
async fn a_proxy_injected_route_sends_no_authentication_header() {
    let headers = posted_headers(Arc::new(ProxyInjected)).await;
    assert!(
        headers
            .iter()
            .all(|(name, _)| !name.eq_ignore_ascii_case("authorization")),
        "{headers:?}"
    );
    for name in ["x-api-key", "api-key", "proxy-authorization"] {
        assert!(
            headers
                .iter()
                .all(|(sent, _)| !sent.eq_ignore_ascii_case(name)),
            "{name} was sent: {headers:?}"
        );
    }
    // The rest of the request is unchanged: the session header, content type and
    // accept are still there.
    assert_eq!(
        headers
            .iter()
            .find(|(name, _)| name == "x-session")
            .map(|(_, value)| value.as_str()),
        Some("proxy-key")
    );
    assert!(headers.iter().any(|(name, _)| name == "content-type"));
}

#[tokio::test]
async fn a_route_that_resolves_its_own_credential_still_sends_it() {
    let headers = posted_headers(Arc::new(Resolved)).await;
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == &format!("Bearer {BEARER}")),
        "{headers:?}"
    );
}
