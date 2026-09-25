//! Issue #134: a route whose credential an egress proxy injects sends NO
//! authentication header. This is the Responses adapter's half of the claim — the
//! source says `proxy_injected()`, and the request that reaches the transport carries
//! no `Authorization` header (and, on this account, no account-id header either,
//! because the proxy supplies the whole credential). The contrast case pins the other
//! half: a route that resolves its own credential still sends both.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Provider, ProviderError,
    ProviderRequest,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai::{
    OpenAiCodexProvider, ResponsesAccount, ResponsesRoute, ResponsesTransport,
};

const BEARER: &str = "NONE-TEST-FAKE-BEARER";
const ACCOUNT_ID: &str = "NONE-TEST-FAKE-ACCOUNT";

/// A source that resolves a credential of its own, account id included.
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

/// The source of a route whose credential an egress proxy injects: the placeholder
/// `p1_auth::resolve` hands every adapter for `kind = "none"`. It has no account id,
/// and the account-id guard must let it through unchanged.
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

fn route() -> ResponsesRoute {
    ResponsesRoute {
        origin_route: p1_provider_openai::ROUTE.to_string(),
        endpoint: "https://chatgpt.com/backend-api".to_string(),
        account: ResponsesAccount::CodexSubscription,
        transport: ResponsesTransport::Sse,
    }
}

fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: "gpt-example".into(),
        revision: 1,
        model_id: "gpt-example".into(),
        family: "gpt".into(),
        thinking: ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::High],
        default_effort: Some(Effort::High),
        thinking_budgets: BTreeMap::new(),
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
    let provider = OpenAiCodexProvider::new(
        route(),
        "gpt-example",
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
        "chatgpt-account-id",
    ] {
        assert!(
            headers
                .iter()
                .all(|(sent, _)| !sent.eq_ignore_ascii_case(name)),
            "{name} was sent: {headers:?}"
        );
    }
    // The client identity both transports send is unchanged.
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("originator") && value == "p1")
    );
}

#[tokio::test]
async fn a_route_that_resolves_its_own_credential_still_sends_it() {
    let headers = posted_headers(Arc::new(Resolved)).await;
    assert!(
        headers
            .iter()
            .any(|(name, value)| name.eq_ignore_ascii_case("authorization")
                && value == &format!("Bearer {BEARER}")),
        "{headers:?}"
    );
    assert!(
        headers.iter().any(
            |(name, value)| name.eq_ignore_ascii_case("chatgpt-account-id") && value == ACCOUNT_ID
        ),
        "{headers:?}"
    );
}
