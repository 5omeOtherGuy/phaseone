//! The `api-key` dialect (ADR-0197): the public Responses API reached with an OpenAI API
//! key. `POST /v1/responses` with a bearer key and p1's own user agent; no ChatGPT
//! account id, no `originator`, no Codex beta or session headers, and no `store: false`.

use std::collections::BTreeMap;
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::tool::{DeclarationKind, ToolDeclaration};
use p1_contracts::{
    BoxFuture, CancellationToken, Effort, Item, ModelOptions, Provider, ProviderError,
    ProviderRequest,
};
use p1_model_profile::ModelProfile;
use p1_provider_conformance::{
    RouteFixtures, RouteUnderTest, fixtures::responses as fixtures, run_all,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource, HttpRequest};
use p1_provider_openai::{
    OpenAiCodexProvider, ResponsesAccount, ResponsesRoute, ResponsesTransport, build_request,
    lower_request,
};
use serde_json::{Value, json};

const MODEL: &str = "gpt-6-sol";
const BEARER: &str = "API-KEY-TEST-FAKE-BEARER";

/// `routes/openai.toml` as the adapter sees it.
fn route(endpoint: &str) -> ResponsesRoute {
    ResponsesRoute {
        origin_route: "openai-responses/openai".into(),
        endpoint: endpoint.into(),
        account: ResponsesAccount::ApiKey,
        transport: ResponsesTransport::Sse,
    }
}

fn profile() -> Arc<ModelProfile> {
    Arc::new(
        ModelProfile::from_toml(MODEL, include_str!("../../../profiles/gpt-6-sol.toml"))
            .expect("the shipped profile is valid"),
    )
}

/// An API key carries no account id; the provider must not ask for one.
struct ApiKey;

impl CredentialSource for ApiKey {
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

fn provider(endpoint: &str, transport: ScriptedTransport) -> Arc<dyn Provider> {
    Arc::new(
        OpenAiCodexProvider::new(
            route(endpoint),
            MODEL,
            profile(),
            Arc::new(transport),
            Arc::new(ApiKey),
        )
        .expect("the API-key route composes"),
    )
}

fn request(effort: Option<Effort>, cache_key: Option<&str>, cap: Option<u32>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".into(),
        history: vec![Item::User { text: "hi".into() }],
        tools: vec![ToolDeclaration {
            name: "read".into(),
            description: "Read a file".into(),
            kind: DeclarationKind::Function {
                input_schema: json!({ "type": "object" }),
            },
        }],
        options: ModelOptions {
            reasoning_effort: effort,
            cache_key: cache_key.map(str::to_string),
            max_output_tokens: cap,
            ..ModelOptions::default()
        },
    }
}

/// The one request the native provider posts for `request`.
async fn posted(endpoint: &str, request: ProviderRequest) -> HttpRequest {
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }]);
    let provider = provider(endpoint, transport.clone());
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
async fn the_api_key_request_is_the_public_wire_with_a_bearer_and_no_codex_identity() {
    for endpoint in [
        "https://api.openai.com",
        "https://api.openai.com/v1",
        "https://api.openai.com/v1/responses/",
    ] {
        let request = request(Some(Effort::High), Some("session-key"), Some(2048));
        let lowered = lower_request(&route(endpoint), MODEL, &profile(), &request)
            .expect("the request lowers");
        let native = posted(endpoint, request).await;
        assert_eq!(
            native.url, "https://api.openai.com/v1/responses",
            "{endpoint}"
        );
        assert!(native.body == lowered.body, "{endpoint}: the bodies differ");

        // The native request is the lowering behind the bearer, and nothing else.
        let mut expected = vec![("Authorization".to_string(), format!("Bearer {BEARER}"))];
        expected.extend(lowered.headers.clone());
        assert_eq!(native.headers, expected, "{endpoint}");
        assert_eq!(
            lowered.headers,
            vec![
                (
                    "User-Agent".to_string(),
                    format!("p1/{}", env!("CARGO_PKG_VERSION"))
                ),
                ("Content-Type".to_string(), "application/json".to_string()),
                ("Accept".to_string(), "text/event-stream".to_string()),
            ],
            "{endpoint}"
        );

        let body: Value = serde_json::from_slice(&native.body).unwrap();
        assert!(body.get("store").is_none(), "{endpoint}: {body}");
        assert_eq!(body["max_output_tokens"], json!(2048));
        assert_eq!(body["prompt_cache_key"], json!("session-key"));
        assert_eq!(body["model"], json!(MODEL));
        assert_eq!(
            body["reasoning"],
            json!({"effort": "high", "summary": "auto"})
        );
    }
}

#[tokio::test]
async fn the_api_key_request_names_no_account_and_no_session() {
    let native = posted(
        "https://api.openai.com",
        request(None, Some("session-key"), None),
    )
    .await;
    let names: BTreeMap<String, &str> = native
        .headers
        .iter()
        .map(|(name, value)| (name.to_ascii_lowercase(), value.as_str()))
        .collect();
    for forbidden in [
        "chatgpt-account-id",
        "originator",
        "openai-beta",
        "session_id",
        "conversation_id",
        "x-api-key",
    ] {
        assert!(!names.contains_key(forbidden), "{forbidden}: {names:?}");
    }
    let body: Value = serde_json::from_slice(&native.body).unwrap();
    assert!(body.get("max_output_tokens").is_none(), "{body}");
}

#[test]
fn the_api_key_dialect_speaks_sse_only() {
    let route = ResponsesRoute {
        transport: ResponsesTransport::Websocket,
        ..route("https://api.openai.com")
    };
    let error = OpenAiCodexProvider::new(
        route,
        MODEL,
        profile(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        Arc::new(ApiKey),
    )
    .err()
    .expect("the WebSocket transport is refused");
    assert!(error.message.contains("sse"), "{error}");
}

#[test]
fn the_api_key_dialect_has_no_account_id_header() {
    assert_eq!(ResponsesAccount::ApiKey.account_id_header(), None);
}

fn build(transport: ScriptedTransport) -> Arc<dyn Provider> {
    provider("https://api.openai.com", transport)
}

fn follow_up_request(request: &ProviderRequest) -> Value {
    build_request(&route("https://api.openai.com"), MODEL, &profile(), request)
        .expect("the follow-up request builds")
}

/// The output cap this route takes; reasoning effort off it does not (ADR-0170).
fn invalid_request() -> ProviderRequest {
    ProviderRequest {
        options: ModelOptions {
            reasoning_effort: Some(Effort::Off),
            ..ModelOptions::default()
        },
        ..request(None, None, None)
    }
}

#[test]
fn conformance() {
    run_all(&RouteUnderTest {
        name: "openai-responses/openai",
        max_retries: 3,
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
