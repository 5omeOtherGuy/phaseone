//! Issue #134: a route whose credential an egress proxy injects sends NO
//! authentication header. This is the Messages adapter's half of the claim — the
//! source says `proxy_injected()`, and the request that reaches the transport carries
//! no `authorization` header. Everything else the account requires (the version, the
//! betas, the long-context setting, the CLI identity) is unchanged.
//!
//! The contrast case pins the other half: a route that resolves its own credential
//! still sends it.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Provider, ProviderError,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_anthropic::{AnthropicProvider, MessagesAccount, MessagesRoute};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};

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

fn route() -> MessagesRoute {
    MessagesRoute {
        origin_route: p1_provider_anthropic::ROUTE.to_string(),
        endpoint: "https://api.anthropic.com".to_string(),
        account: MessagesAccount::ClaudeCodeSubscription,
        long_context: true,
    }
}

fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: "claude-example".into(),
        revision: 1,
        model_id: "claude-example".into(),
        family: "claude".into(),
        thinking: ThinkingPolicy::Budget,
        efforts: vec![Effort::High],
        default_effort: None,
        thinking_budgets: BTreeMap::from([(Effort::High, 20_480)]),
        context_tokens: None,
        max_output_tokens: None,
    })
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![],
        options: ModelOptions::default(),
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
    let provider = AnthropicProvider::new(
        route(),
        "claude-example",
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
    for name in [
        "authorization",
        "x-api-key",
        "api-key",
        "proxy-authorization",
    ] {
        assert!(
            headers
                .iter()
                .all(|(sent, _)| !sent.eq_ignore_ascii_case(name)),
            "{name} was sent: {headers:?}"
        );
    }
    // Everything the account requires is still there, betas included.
    assert!(headers.iter().any(|(name, _)| name == "anthropic-version"));
    assert!(
        headers
            .iter()
            .any(|(name, value)| name == "anthropic-beta" && value.contains("context-1m"))
    );
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
