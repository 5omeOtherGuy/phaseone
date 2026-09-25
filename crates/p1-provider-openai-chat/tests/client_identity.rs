//! The `client_identity` adapter setting: the OpenCode Zen free-tier request shape.
//! Probes and the minimal accepted set are recorded in
//! `docs/design/zen-client-identity-evidence.md`; these tests pin the wire shape
//! that setting produces, and that it changes nothing when it is absent.

use p1_contracts::{
    BoxFuture, DeclarationKind, Effort, ModelOptions, Provider, ProviderError, ProviderRequest,
    ToolDeclaration,
};
use p1_model_profile::{ModelProfile, ThinkingPolicy};
use p1_provider_conformance::fixtures::chat as fixtures;
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use p1_provider_openai_chat::{
    ChatAdapterSettings, ChatDialect, ChatLimits, ChatProvider, ChatRoute, ClientIdentity,
};
use std::sync::Arc;

const MODEL: &str = "mimo-v2.6-flash-free";
const BEARER: &str = "CLIENT-IDENTITY-FAKE-BEARER";

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
        Box::pin(async {
            Ok(Credential {
                bearer: format!("{BEARER}-refreshed"),
                account_id: None,
            })
        })
    }
}

fn route(identity: Option<ClientIdentity>) -> ChatRoute {
    ChatRoute {
        origin_route: "openai-chat/opencode-zen-1".into(),
        endpoint: "https://opencode.ai/zen/v1/chat/completions".into(),
        headers: vec![("user-agent".into(), "p1/test".into())],
        session_header: Some("x-opencode-session".into()),
        dialect: ChatDialect::ThinkingWithReasoningAlias,
        client_identity: identity,
        limits: ChatLimits::default(),
    }
}

fn profile() -> Arc<ModelProfile> {
    Arc::new(ModelProfile {
        id: MODEL.into(),
        revision: 1,
        model_id: MODEL.into(),
        family: "mimo".into(),
        thinking: ThinkingPolicy::Enabled,
        efforts: vec![Effort::High],
        default_effort: Some(Effort::High),
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: Some(200_000),
        max_output_tokens: Some(32_000),
    })
}

fn declaration(name: &str) -> ToolDeclaration {
    ToolDeclaration {
        name: name.into(),
        description: format!("{name} tool"),
        kind: DeclarationKind::Function {
            input_schema: serde_json::json!({
                "type": "object",
                "properties": { "path": { "type": "string" } }
            }),
        },
    }
}

fn request(tools: Vec<ToolDeclaration>, cache_key: Option<&str>) -> ProviderRequest {
    ProviderRequest {
        system_prompt: "system".into(),
        history: vec![],
        tools,
        options: ModelOptions {
            cache_key: cache_key.map(str::to_string),
            ..ModelOptions::default()
        },
    }
}

/// One round trip through the provider, returning the request it put on the wire.
async fn sent(
    identity: Option<ClientIdentity>,
    tools: Vec<ToolDeclaration>,
    cache_key: Option<&str>,
) -> p1_provider_http::HttpRequest {
    use futures_util::StreamExt;
    let transport = ScriptedTransport::new(vec![ScriptedResponse::ok_sse(fixtures::NO_USAGE)]);
    let provider = ChatProvider::new(
        route(identity),
        MODEL,
        profile(),
        Arc::new(transport.clone()),
        Arc::new(Fixed),
    )
    .unwrap();
    let mut stream = provider
        .stream(
            request(tools, cache_key),
            p1_contracts::CancellationToken::new(),
        )
        .await
        .unwrap();
    while stream.next().await.is_some() {}
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    requests.into_iter().next().unwrap()
}

fn header<'a>(request: &'a p1_provider_http::HttpRequest, name: &str) -> Option<&'a str> {
    request
        .headers
        .iter()
        .find(|(header, _)| header.as_str() == name)
        .map(|(_, value)| value.as_str())
}

/// `ses_`/`msg_` + 12 lowercase hex + 14 base62, exactly the shape the gateway's
/// free-tier gate accepts (probe `lower_hex_prefix`; uppercase or shorter is 403).
fn assert_opencode_id(id: &str, prefix: &str) {
    assert!(id.starts_with(prefix), "{id}");
    assert_eq!(id.len(), prefix.len() + 26, "{id}");
    let rest = &id[prefix.len()..];
    assert!(
        rest[..12]
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)),
        "{id}: first 12 must be lowercase hex"
    );
    assert!(
        rest.bytes().all(|b| b.is_ascii_alphanumeric()),
        "{id}: suffix must be alphanumeric"
    );
}

#[tokio::test]
async fn opencode_identity_sets_the_headers_and_injects_no_tools() {
    let request = sent(Some(ClientIdentity::Opencode), vec![], Some("p1-abc")).await;
    let user_agent = header(&request, "user-agent").expect("user-agent");
    assert!(
        user_agent.starts_with("opencode/"),
        "the identity replaces p1's user-agent: {user_agent}"
    );
    assert_eq!(header(&request, "x-opencode-client"), Some("cli"));
    assert_eq!(header(&request, "x-opencode-project"), Some("global"));
    assert_opencode_id(
        header(&request, "x-opencode-session").expect("session"),
        "ses_",
    );
    assert_opencode_id(
        header(&request, "x-opencode-request").expect("request id"),
        "msg_",
    );
    // The adapter declares no tool of its own. The Zen free-tier gate wants `bash` and
    // `read` among the declared tools, but that is an environment's business
    // (`[[tools]] name = "bash"` / `"read"`); a tool-less request therefore carries no
    // `tools` and no `tool_choice`.
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
    assert_eq!(body["stream"], true);
}

#[tokio::test]
async fn the_identity_leaves_p1_tools_untouched() {
    let request = sent(
        Some(ClientIdentity::Opencode),
        vec![declaration("read"), declaration("shell")],
        Some("p1-abc"),
    )
    .await;
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    let names: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    // The identity changes only headers: it neither adds nor renames a declaration.
    assert_eq!(names, ["read", "shell"], "{names:?}");
}

#[tokio::test]
async fn the_identity_ids_are_a_pure_function_of_the_cache_key() {
    let first = sent(Some(ClientIdentity::Opencode), vec![], Some("p1-abc")).await;
    let again = sent(Some(ClientIdentity::Opencode), vec![], Some("p1-abc")).await;
    let other = sent(Some(ClientIdentity::Opencode), vec![], Some("p1-def")).await;
    assert_eq!(
        header(&first, "x-opencode-session"),
        header(&again, "x-opencode-session")
    );
    assert_ne!(
        header(&first, "x-opencode-session"),
        header(&other, "x-opencode-session")
    );
}

#[tokio::test]
async fn without_the_setting_the_request_is_p1s_own() {
    let request = sent(None, vec![declaration("read")], Some("synthetic-session")).await;
    assert_eq!(header(&request, "user-agent"), Some("p1/test"));
    assert_eq!(
        header(&request, "x-opencode-session"),
        Some("synthetic-session")
    );
    assert_eq!(header(&request, "x-opencode-client"), None);
    assert_eq!(header(&request, "x-opencode-project"), None);
    assert_eq!(header(&request, "x-opencode-request"), None);
    let body: serde_json::Value = serde_json::from_slice(&request.body).unwrap();
    let names: Vec<&str> = body["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["function"]["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["read"], "{names:?}");
    assert!(body.get("tool_choice").is_none(), "{body}");
}

#[test]
fn the_setting_parses_and_an_unknown_identity_is_rejected() {
    let settings: ChatAdapterSettings = serde_json::from_value(serde_json::json!({
        "dialect": "thinking-with-reasoning-alias",
        "session_header": "x-opencode-session",
        "client_identity": "opencode"
    }))
    .expect("the setting parses");
    assert_eq!(settings.client_identity, Some(ClientIdentity::Opencode));
    let absent: ChatAdapterSettings = serde_json::from_value(serde_json::json!({
        "dialect": "thinking-with-reasoning-alias"
    }))
    .expect("the setting is optional");
    assert_eq!(absent.client_identity, None);
    let error = serde_json::from_value::<ChatAdapterSettings>(serde_json::json!({
        "dialect": "thinking-with-reasoning-alias",
        "client_identity": "gemini"
    }))
    .unwrap_err();
    assert!(
        error.to_string().contains("unknown variant"),
        "{error}: an unknown identity is refused, never ignored"
    );
}
