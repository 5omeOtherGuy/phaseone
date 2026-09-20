//! The file-based credential source end-to-end: an expired Codex CLI token is
//! refreshed through the transport and the provider request then carries the new
//! bearer. No real credential file is ever read: the test writes a temp file.

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use futures_util::StreamExt;
use p1_contracts::{
    CancellationToken, ModelOptions, Outcome, Provider, ProviderRequest, StreamEvent,
};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_openai::{CodexCliCredentials, OpenAiCodexProvider};

const SSE_TEXT: &str = r#"event: response.output_item.done
data: {"type":"response.output_item.done","item":{"type":"message","content":[{"type":"output_text","text":"ok"}]}}

event: response.completed
data: {"type":"response.completed","response":{"id":"resp_1"}}

"#;

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

/// The model policy the driver was recorded with: any model name took an effort
/// level, and only `low`/`medium`/`high` (spec §7.1).
fn test_profile() -> p1_model_profile::ModelProfile {
    use p1_contracts::Effort;
    p1_model_profile::ModelProfile {
        id: "gpt-test".to_string(),
        revision: 1,
        model_id: "gpt-test".to_string(),
        family: "gpt".to_string(),
        thinking: p1_model_profile::ThinkingPolicy::EffortLevel,
        efforts: vec![Effort::Low, Effort::Medium, Effort::High],
        default_effort: None,
        thinking_budgets: std::collections::BTreeMap::new(),
        context_tokens: None,
        max_output_tokens: None,
    }
}

#[tokio::test]
async fn expired_token_is_refreshed_and_the_request_uses_the_new_bearer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&serde_json::json!({
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

    let transport =
        ScriptedTransport::new(vec![refresh_response(), ScriptedResponse::ok_sse(SSE_TEXT)]);
    let credentials = Arc::new(CodexCliCredentials::at(
        path.clone(),
        Arc::new(transport.clone()),
    ));
    let provider = OpenAiCodexProvider::new(
        p1_provider_openai::ResponsesRoute {
            origin_route: p1_provider_openai::ROUTE.to_string(),
            endpoint: "https://chatgpt.com/backend-api".to_string(),
            account: p1_provider_openai::ResponsesAccount::CodexSubscription,
        },
        "gpt-test",
        Arc::new(test_profile()),
        Arc::new(transport.clone()),
        credentials,
    )
    .expect("the route and the profile compose");

    let request = ProviderRequest {
        system_prompt: "sys".to_string(),
        history: Vec::new(),
        tools: Vec::new(),
        options: ModelOptions::default(),
    };
    let mut stream = provider
        .stream(request, CancellationToken::new())
        .await
        .expect("setup succeeds");
    let mut events = Vec::new();
    while let Some(event) = stream.next().await {
        events.push(event);
    }
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
