//! Both borrowed OAuth logins driving a provider request END TO END, through the
//! host: an expired token is refreshed through the transport, the provider request
//! then carries the new bearer, and the refresh is written back to the file it came
//! from.
//!
//! These two tests moved here with the credential code: the sources live in
//! `p1-auth`, which no adapter may depend on, and only the host composes. No real
//! credential file is ever read — every path is a scratch directory and every value
//! is obviously fake.

// The adapter's own fixtures; only the text turn is used here.
#[allow(dead_code)]
#[path = "../../p1-provider-anthropic/tests/fixtures/mod.rs"]
mod messages_fixtures;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use p1_contracts::{
    CancellationToken, ModelOptions, Outcome, Provider, ProviderRequest, StreamEvent,
};
use p1_host::auth::credential_source_at;
use p1_host::catalog::route_provider;
use p1_host::routes::{RouteFile, load_route_by_id};
use p1_model_profile::ModelProfile;
use p1_provider_http::Transport;
use p1_provider_http::testing::{
    BodyEnd, RefusingWsConnector, ScriptedResponse, ScriptedTransport,
};

const SSE_TEXT: &str = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1"}}

"#;

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

fn environment_dirs() -> Vec<PathBuf> {
    vec![repo("environments")]
}

/// The shipped profile `id`, read from `profiles/<id>.toml`.
fn profile(id: &str) -> Arc<ModelProfile> {
    let text = std::fs::read_to_string(repo(&format!("profiles/{id}.toml")))
        .unwrap_or_else(|error| panic!("profiles/{id}.toml: {error}"));
    Arc::new(ModelProfile::from_toml(id, &text).unwrap_or_else(|error| panic!("{id}: {error}")))
}

/// The provider the host's own catalog factory builds for this route: the shipped
/// route file, its binding and the REAL credential source over a scratch home.
///
/// The connector is injected like the transport (ADR-0047 §1). The shipped Codex
/// route asks for WebSocket, so this test must hand it one that REFUSES every
/// upgrade: §5 then falls back to SSE at once and the scripted HTTP transport serves
/// the request, exactly as these expectations were recorded. The real connector —
/// what the host composes — would reach the internet, which no test may do.
fn provider(
    route_id: &str,
    profile_id: &str,
    home: &Path,
    transport: Arc<dyn Transport>,
) -> Arc<dyn Provider> {
    let route: RouteFile = load_route_by_id(&environment_dirs(), route_id).expect("the route file");
    let binding = route
        .binding(profile_id)
        .expect("the route serves it")
        .clone();
    let locations = p1_auth::Locations::none().with_home(Some(home.to_path_buf()));
    let credentials = credential_source_at(&route, transport.clone(), &locations);
    route_provider(
        &route,
        &binding,
        profile(profile_id),
        transport,
        Arc::new(RefusingWsConnector::default()),
        credentials,
    )
    .expect("the shipped route and profile compose")
}

fn request() -> ProviderRequest {
    ProviderRequest {
        system_prompt: "sys".to_string(),
        history: Vec::new(),
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

async fn run(provider: &dyn Provider) -> Vec<StreamEvent> {
    let mut stream = provider
        .stream(request(), CancellationToken::new())
        .await
        .expect("setup succeeds");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
    events
}

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        }
    }
    out
}

/// A fake unsigned token that expired ten seconds ago.
fn expired_jwt() -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let payload = serde_json::json!({ "exp": now - 10 }).to_string();
    format!(
        "{}.{}.signature",
        base64url_encode(br#"{"alg":"none"}"#),
        base64url_encode(payload.as_bytes())
    )
}

fn refresh_response() -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        chunks: vec![
            serde_json::to_vec(&serde_json::json!({
                "access_token": "new-access",
                "refresh_token": "refresh-new",
                "id_token": "new-id",
            }))
            .unwrap(),
        ],
        end: BodyEnd::Eof,
    }
}

/// A scratch home with `<home>/.codex/auth.json` holding an expired fake token.
fn codex_home() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".codex")).unwrap();
    std::fs::write(
        dir.path().join(".codex/auth.json"),
        serde_json::to_vec_pretty(&serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "old-id",
                "access_token": expired_jwt(),
                "refresh_token": "refresh-old",
                "account_id": "acct_1",
            },
            "last_refresh": "2000-01-01T00:00:00Z",
            "unknown_field": "keep",
        }))
        .unwrap(),
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn an_expired_codex_token_is_refreshed_and_the_request_uses_the_new_bearer() {
    let home = codex_home();
    let path = home.path().join(".codex/auth.json");
    let transport = Arc::new(ScriptedTransport::new(vec![
        refresh_response(),
        ScriptedResponse::ok_sse(SSE_TEXT),
    ]));
    let provider = provider(
        "openai-codex-subscription",
        "gpt-5.6-sol",
        home.path(),
        transport.clone(),
    );

    let events = run(&*provider).await;
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::Finished(Outcome::Completed(_)))
        ),
        "{events:?}"
    );

    let requests = transport.requests();
    assert_eq!(requests.len(), 2);
    assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");
    assert_eq!(
        requests[1].url,
        "https://chatgpt.com/backend-api/codex/responses"
    );
    assert!(
        requests[1].headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer new-access"
        }),
        "{:?}",
        requests[1].headers
    );
    assert!(requests[1].headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("chatgpt-account-id") && value == "acct_1"
    }));

    let written: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["tokens"]["access_token"], "new-access");
    assert_eq!(written["tokens"]["refresh_token"], "refresh-new");
    assert_eq!(written["unknown_field"], "keep");
    assert_eq!(
        std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600,
        "the auth file must be written back owner-only"
    );
}

#[tokio::test]
async fn the_claude_code_login_token_reaches_the_wire_without_a_refresh() {
    let home = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(home.path().join(".claude")).unwrap();
    std::fs::write(
        home.path().join(".claude/.credentials.json"),
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": "FAKE-FILE-TOKEN",
                "refreshToken": "FAKE-REFRESH",
                "expiresAt": 4_102_444_800_000u64,
            }
        })
        .to_string(),
    )
    .unwrap();

    let transport = Arc::new(ScriptedTransport::new(vec![ScriptedResponse::ok_sse(
        messages_fixtures::text_turn,
    )]));
    let provider = provider(
        "anthropic-subscription",
        "claude-sonnet-4-6",
        home.path(),
        transport.clone(),
    );

    let events = run(&*provider).await;
    assert!(
        matches!(
            events.last(),
            Some(StreamEvent::Finished(Outcome::Completed(_)))
        ),
        "{events:?}"
    );

    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "a fresh token needs no refresh");
    assert_eq!(requests[0].url, "https://api.anthropic.com/v1/messages");
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "authorization" && value == "Bearer FAKE-FILE-TOKEN"),
        "the file token must be used verbatim: {:?}",
        requests[0].headers
    );
}
