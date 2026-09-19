//! Credential-file tests: fake tokens in temp files only, a scripted transport
//! for the refresh endpoint, and an injected clock. No real credential file is
//! ever opened.

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use p1_contracts::ProviderErrorKind;
use p1_provider_anthropic::ClaudeCodeCredentials;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{Credential, CredentialSource};
use serde_json::{Value, json};

const NOW: u64 = 1_700_000_000_000;
const HOUR_MS: u64 = 3_600_000;
const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
const DEFAULT_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

// One clock per test: tests run in parallel, so a shared clock would race.
static CLOCK_FRESH: AtomicU64 = AtomicU64::new(0);
static CLOCK_REFRESH: AtomicU64 = AtomicU64::new(0);
static CLOCK_ROTATED: AtomicU64 = AtomicU64::new(0);
static CLOCK_REJECTED: AtomicU64 = AtomicU64::new(0);
static CLOCK_FAILURE: AtomicU64 = AtomicU64::new(0);
static CLOCK_NOEXPIRY: AtomicU64 = AtomicU64::new(0);
static CLOCK_FLAT: AtomicU64 = AtomicU64::new(0);

fn clock_fresh() -> u64 {
    CLOCK_FRESH.load(Ordering::SeqCst)
}
fn clock_refresh() -> u64 {
    CLOCK_REFRESH.load(Ordering::SeqCst)
}
fn clock_rotated() -> u64 {
    CLOCK_ROTATED.load(Ordering::SeqCst)
}
fn clock_rejected() -> u64 {
    CLOCK_REJECTED.load(Ordering::SeqCst)
}
fn clock_failure() -> u64 {
    CLOCK_FAILURE.load(Ordering::SeqCst)
}
fn clock_noexpiry() -> u64 {
    CLOCK_NOEXPIRY.load(Ordering::SeqCst)
}
fn clock_flat() -> u64 {
    CLOCK_FLAT.load(Ordering::SeqCst)
}

fn nested(access: &str, refresh: &str, expires_at: u64) -> Value {
    json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
            "subscriptionType": "max",
        }
    })
}

fn write_private(path: &Path, contents: &str) {
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    use std::io::Write;
    let mut file = options.open(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

fn write_json_private(path: &Path, value: &Value) {
    write_private(path, &serde_json::to_string_pretty(value).unwrap());
}

fn read_json(path: &Path) -> Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

#[cfg(unix)]
fn mode(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

fn refresh_response(body: Value) -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![body.to_string().into_bytes()],
        end: BodyEnd::Eof,
    }
}

fn status_response(status: u16) -> ScriptedResponse {
    ScriptedResponse {
        status,
        headers: Vec::new(),
        chunks: Vec::new(),
        end: BodyEnd::Eof,
    }
}

fn rejected() -> Credential {
    Credential {
        bearer: "OLD-REJECTED".to_string(),
        account_id: None,
    }
}

#[tokio::test]
async fn a_fresh_token_is_returned_without_refreshing() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_FRESH.store(NOW, Ordering::SeqCst);
    write_json_private(&path, &nested("FRESH-ACCESS", "R", NOW + HOUR_MS));

    let transport = ScriptedTransport::new(Vec::new());
    let credentials =
        ClaudeCodeCredentials::at(path, Arc::new(transport.clone())).with_clock(clock_fresh);
    let credential = credentials.access().await.unwrap();

    assert_eq!(credential.bearer, "FRESH-ACCESS");
    assert!(
        transport.requests().is_empty(),
        "a fresh token needs no refresh"
    );
}

#[tokio::test]
async fn an_expired_token_refreshes_writes_back_and_preserves_unknown_fields() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_REFRESH.store(NOW, Ordering::SeqCst);
    let mut document = nested("OLD-ACCESS", "OLD-REFRESH", NOW - 1);
    document["claudeAiOauth"]["scopes"] = json!(["user:inference"]);
    document["rootExtra"] = json!({ "kept": true });
    write_json_private(&path, &document);

    let transport = ScriptedTransport::new(vec![refresh_response(json!({
        "access_token": "NEW-ACCESS",
        "expires_in": 3600,
        "refresh_token": "NEW-REFRESH",
        "scope": "user:inference user:profile",
    }))]);
    let credentials = ClaudeCodeCredentials::at(path.clone(), Arc::new(transport.clone()))
        .with_clock(clock_refresh);
    let credential = credentials.access().await.unwrap();

    assert_eq!(credential.bearer, "NEW-ACCESS");
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://platform.claude.com/v1/oauth/token"
    );
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, value)| name == "anthropic-beta" && value == "oauth-2025-04-20")
    );
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();
    assert_eq!(body["grant_type"], json!("refresh_token"));
    assert_eq!(body["refresh_token"], json!("OLD-REFRESH"));
    assert_eq!(body["client_id"], json!(CLIENT_ID));
    assert_eq!(body["scope"], json!("user:inference"));

    let written = read_json(&path);
    assert_eq!(written["claudeAiOauth"]["accessToken"], json!("NEW-ACCESS"));
    assert_eq!(
        written["claudeAiOauth"]["refreshToken"],
        json!("NEW-REFRESH")
    );
    assert_eq!(
        written["claudeAiOauth"]["expiresAt"],
        json!(NOW + 3600 * 1000)
    );
    assert_eq!(written["claudeAiOauth"]["subscriptionType"], json!("max"));
    assert_eq!(
        written["claudeAiOauth"]["scopes"],
        json!(["user:inference", "user:profile"])
    );
    assert_eq!(written["rootExtra"]["kept"], json!(true));
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
}

#[tokio::test]
async fn a_token_another_process_already_rotated_is_used_without_a_network_call() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_ROTATED.store(NOW, Ordering::SeqCst);
    write_json_private(&path, &nested("FRESH-OTHER", "R2", NOW + HOUR_MS));

    let transport = ScriptedTransport::new(Vec::new());
    let credentials =
        ClaudeCodeCredentials::at(path, Arc::new(transport.clone())).with_clock(clock_rotated);
    let credential = credentials.refresh(&rejected()).await.unwrap();

    assert_eq!(credential.bearer, "FRESH-OTHER");
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn refresh_never_returns_the_rejected_token() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_REJECTED.store(NOW, Ordering::SeqCst);
    write_json_private(&path, &nested("SENTINEL-ACCESS", "R", NOW - 1));
    let before = std::fs::read(&path).unwrap();

    let transport = ScriptedTransport::new(vec![refresh_response(json!({
        "access_token": "SENTINEL-ACCESS",
        "expires_in": 3600,
    }))]);
    let credentials = ClaudeCodeCredentials::at(path.clone(), Arc::new(transport.clone()))
        .with_clock(clock_rejected);
    let rejected = Credential {
        bearer: "SENTINEL-ACCESS".to_string(),
        account_id: None,
    };
    let error = credentials.refresh(&rejected).await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(
        std::fs::read(&path).unwrap(),
        before,
        "a rejected refresh must not rewrite the file"
    );
}

#[tokio::test]
async fn a_missing_file_is_an_authentication_error_with_a_login_hint() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    let transport = ScriptedTransport::new(Vec::new());
    let credentials = ClaudeCodeCredentials::at(path.clone(), Arc::new(transport));

    let error = credentials.access().await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(
        error.message.contains("Claude Code login"),
        "{}",
        error.message
    );
    assert!(
        error.message.contains(".credentials.json"),
        "{}",
        error.message
    );
    assert!(
        !path.exists(),
        "reading must never create the credentials file"
    );
}

#[tokio::test]
async fn a_failed_refresh_leaves_the_file_byte_identical_and_private() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_FAILURE.store(NOW, Ordering::SeqCst);
    let mut document = nested("OLD-ACCESS", "OLD-REFRESH", NOW - 1);
    document["unknownRoot"] = json!({ "preserved": "verbatim" });
    write_json_private(&path, &document);
    let before = std::fs::read(&path).unwrap();

    let transport = ScriptedTransport::new(vec![status_response(500)]);
    let credentials =
        ClaudeCodeCredentials::at(path.clone(), Arc::new(transport)).with_clock(clock_failure);
    let error = credentials.access().await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(std::fs::read(&path).unwrap(), before);
    #[cfg(unix)]
    assert_eq!(mode(&path), 0o600);
}

#[tokio::test]
async fn a_malformed_file_is_authentication_without_leaking_its_contents() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    write_private(&path, "SENTINEL-BODY { this is not json");

    let transport = ScriptedTransport::new(Vec::new());
    let credentials = ClaudeCodeCredentials::at(path, Arc::new(transport));
    let error = credentials.access().await.unwrap_err();

    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(
        !error.message.contains("SENTINEL-BODY"),
        "{}",
        error.message
    );
}

#[tokio::test]
async fn a_token_without_an_expiry_is_used_as_is() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_NOEXPIRY.store(NOW, Ordering::SeqCst);
    write_json_private(&path, &json!({ "accessToken": "FLAT-ACCESS" }));

    let transport = ScriptedTransport::new(Vec::new());
    let credentials =
        ClaudeCodeCredentials::at(path, Arc::new(transport.clone())).with_clock(clock_noexpiry);
    let credential = credentials.access().await.unwrap();

    assert_eq!(credential.bearer, "FLAT-ACCESS");
    assert!(transport.requests().is_empty());
}

#[tokio::test]
async fn a_flat_file_stays_flat_and_defaults_scopes_on_refresh() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    CLOCK_FLAT.store(NOW, Ordering::SeqCst);
    write_json_private(
        &path,
        &json!({
            "accessToken": "OLD-FLAT",
            "refreshToken": "R",
            "expiresAt": NOW - 1,
            "custom": "keep",
        }),
    );

    let transport = ScriptedTransport::new(vec![refresh_response(json!({
        "access_token": "NEW-FLAT",
        "expires_in": 3600,
    }))]);
    let credentials =
        ClaudeCodeCredentials::at(path.clone(), Arc::new(transport.clone())).with_clock(clock_flat);
    let credential = credentials.access().await.unwrap();
    assert_eq!(credential.bearer, "NEW-FLAT");

    let request_body: Value = serde_json::from_slice(&transport.requests()[0].body).unwrap();
    assert_eq!(request_body["scope"], json!(DEFAULT_SCOPES));
    assert_eq!(request_body["refresh_token"], json!("R"));

    let written = read_json(&path);
    assert!(
        written.get("claudeAiOauth").is_none(),
        "flat shape preserved"
    );
    assert_eq!(written["accessToken"], json!("NEW-FLAT"));
    assert_eq!(written["custom"], json!("keep"));
    // The response rotated no refresh token, so the existing one is kept.
    assert_eq!(written["refreshToken"], json!("R"));
}

#[tokio::test]
async fn from_default_location_constructs_without_reading_the_file() {
    // Construction must never open the real credential file; `access` is
    // deliberately not called here.
    let credentials = ClaudeCodeCredentials::from_default_location();
    assert!(credentials.is_ok(), "HOME is set, so a path resolves");
}
