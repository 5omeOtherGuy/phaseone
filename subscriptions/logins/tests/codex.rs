//! The Codex CLI login through p1's credential chain: fake tokens in temp files only, a
//! scripted transport for the refresh endpoint and an injected clock.

mod support;

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use p1_contracts::ProviderErrorKind;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use serde_json::{Value, json};

fn now_seconds() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
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

fn jwt_with_payload(payload: &Value) -> String {
    format!(
        "{}.{}.signature",
        base64url(br#"{"alg":"none"}"#),
        base64url(payload.to_string().as_bytes())
    )
}

fn jwt_exp(exp: i64) -> String {
    jwt_with_payload(&json!({ "exp": exp }))
}

fn jwt_expiring_in(seconds: i64) -> String {
    jwt_exp(now_seconds() + seconds)
}

fn auth_json(access: &str, refresh: &str) -> Value {
    json!({
        "OPENAI_API_KEY": null,
        "tokens": {
            "id_token": "old-id",
            "access_token": access,
            "refresh_token": refresh,
            "account_id": "acct_1",
        },
        "last_refresh": "2000-01-01T00:00:00Z",
        "unknown_field": { "keep": true },
    })
}

fn write_auth_file(path: &Path, value: &Value) {
    fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
}

fn refresh_response(value: &Value) -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        chunks: vec![serde_json::to_vec(value).unwrap()],
        end: BodyEnd::Eof,
    }
}

fn credentials(path: &Path, transport: &ScriptedTransport, now_ms: u64) -> p1_auth::LoginFile {
    support::codex_at(path.to_path_buf(), Arc::new(transport.clone())).with_clock(move || now_ms)
}

fn system_now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

#[tokio::test]
async fn missing_tokens_tell_the_user_to_log_in() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    write_auth_file(&path, &json!({ "OPENAI_API_KEY": "sk-test" }));
    let credentials = support::codex_at(path, Arc::new(ScriptedTransport::new(Vec::new())));
    let error = credentials.access().await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(error.message.contains("codex login"), "{}", error.message);
}

#[tokio::test]
async fn account_id_falls_back_to_the_jwt_claim() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let access = jwt_with_payload(&json!({
        "exp": now_seconds() + 3600,
        "https://api.openai.com/auth": { "chatgpt_account_id": "acc_jwt" }
    }));
    let mut value = auth_json(&access, "refresh-1");
    value["tokens"]["account_id"] = Value::Null;
    write_auth_file(&path, &value);
    let credentials = support::codex_at(path, Arc::new(ScriptedTransport::new(Vec::new())));
    let credential = credentials.access().await.unwrap();
    assert_eq!(credential.account_id.as_deref(), Some("acc_jwt"));
}

#[tokio::test]
async fn valid_token_is_used_without_a_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    let access = jwt_expiring_in(3600);
    write_auth_file(&path, &auth_json(&access, "refresh-1"));
    let transport = ScriptedTransport::new(Vec::new());
    let credential = credentials(&path, &transport, system_now_ms())
        .access()
        .await
        .unwrap();
    assert_eq!(credential.bearer, access);
    assert_eq!(credential.account_id.as_deref(), Some("acct_1"));
    assert!(transport.requests().is_empty(), "no refresh was needed");
}

#[tokio::test]
async fn an_opaque_token_without_an_expiry_is_used_as_it_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    write_auth_file(&path, &auth_json("not-a-jwt", "refresh-1"));
    let transport = ScriptedTransport::new(Vec::new());
    assert!(
        credentials(&path, &transport, system_now_ms())
            .access()
            .await
            .is_ok()
    );
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn expired_token_refreshes_and_preserves_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    // The fixed clock below is 1_700_000_000 s, so the token is expired by it.
    write_auth_file(
        &path,
        &auth_json(&jwt_exp(1_700_000_000 - 10), "refresh-old"),
    );
    // A world-readable original is tightened by the atomic 0600 write-back.
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();
    let transport = ScriptedTransport::new(vec![refresh_response(&json!({
        "access_token": "new-access",
        "refresh_token": "refresh-new",
        "id_token": "new-id",
    }))]);
    let credential = credentials(&path, &transport, 1_700_000_000_000)
        .access()
        .await
        .unwrap();
    assert_eq!(credential.bearer, "new-access");
    assert_eq!(credential.account_id.as_deref(), Some("acct_1"));

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");
    assert!(requests[0].body.starts_with(b"grant_type=refresh_token"));
    assert!(requests[0].headers.iter().any(|(name, value)| {
        name == "Content-Type" && value == "application/x-www-form-urlencoded"
    }));

    let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    assert_eq!(written["tokens"]["access_token"], "new-access");
    assert_eq!(written["tokens"]["refresh_token"], "refresh-new");
    assert_eq!(written["tokens"]["id_token"], "new-id");
    assert_eq!(written["unknown_field"]["keep"], true);
    assert_eq!(written["last_refresh"], "2023-11-14T22:13:20Z");
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert!(
        dir.path().join("auth.lock").exists(),
        "the lock older p1 processes take is the one taken"
    );
}

#[tokio::test]
async fn refresh_failure_leaves_the_file_byte_identical() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    write_auth_file(&path, &auth_json(&jwt_expiring_in(-10), "refresh-old"));
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
    let before = fs::read(&path).unwrap();
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 500,
        headers: Vec::new(),
        chunks: vec![b"upstream failure".to_vec()],
        end: BodyEnd::Eof,
    }]);
    let error = credentials(&path, &transport, system_now_ms())
        .access()
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(fs::read(&path).unwrap(), before);
    assert_eq!(
        fs::metadata(&path).unwrap().permissions().mode() & 0o777,
        0o600
    );
}

#[tokio::test]
async fn refresh_does_not_return_the_rejected_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    write_auth_file(&path, &auth_json("rejected-token", "refresh-old"));
    // The server rotates the refresh token but mints no new access token: the source must
    // not fall back to the rejected bearer.
    let transport = ScriptedTransport::new(vec![refresh_response(&json!({
        "access_token": "rejected-token",
        "refresh_token": "refresh-new",
    }))]);
    let rejected = Credential {
        bearer: "rejected-token".to_string(),
        account_id: Some("acct_1".to_string()),
    };
    let error = credentials(&path, &transport, system_now_ms())
        .refresh(&rejected)
        .await
        .unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(!error.message.contains("rejected-token"));
}

#[tokio::test]
async fn refresh_reuses_a_peers_rotated_token_without_a_request() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("auth.json");
    write_auth_file(&path, &auth_json(&jwt_expiring_in(3600), "refresh-new"));
    let transport = ScriptedTransport::new(Vec::new());
    let rejected = Credential {
        bearer: "old-rejected".to_string(),
        account_id: Some("acct_1".to_string()),
    };
    let credential = credentials(&path, &transport, system_now_ms())
        .refresh(&rejected)
        .await
        .unwrap();
    assert_ne!(credential.bearer, "old-rejected");
    assert!(transport.requests().is_empty());
}

/// The token origin each credential component's manifest declares is the origin its refresh
/// request goes to (ADR-0212): a module whose request goes elsewhere is refused at refresh.
#[test]
fn each_component_declares_the_origin_its_refresh_goes_to() {
    use p1_subscription_logins::{ClaudeCode, Codex, Login};
    let modules = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../modules");
    for (crate_dir, login) in [
        ("p1-module-claude-code-oauth", &ClaudeCode as &dyn Login),
        ("p1-module-codex-oauth", &Codex),
    ] {
        let manifest = fs::read_to_string(modules.join(crate_dir).join("Cargo.toml")).unwrap();
        let request: Value =
            serde_json::from_str(&login.refresh_request("FAKE-REFRESH", None).unwrap()).unwrap();
        let url = request["url"].as_str().unwrap();
        let origin = &url[..url["https://".len()..].find('/').unwrap() + "https://".len()];
        assert!(
            manifest.contains(&format!("token_origins = [\"{origin}\"]")),
            "{crate_dir} must declare {origin}"
        );
    }
}
