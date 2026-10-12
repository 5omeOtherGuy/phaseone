//! Must-pass (a) and (b) of the spec: the order of the chain (env → p1 store →
//! borrowed login), and the rule that access is re-evaluated on every call.

mod support;

use std::sync::Arc;

use p1_auth::{CredentialSpec, Locations, resolve};
use p1_provider_http::CredentialSource;
use p1_provider_http::testing::ScriptedTransport;
use support::{FakeEnv, NEVER_EXPIRES_MS, Scratch, claude_login, login};

const ROUTE: &str = "test-route";
const STORE: &str = ".config/p1/auth.json";
const OPENCODE: &str = ".local/share/opencode/auth.json";
const PI: &str = ".pi/agent/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).expect("the `[credential]` table parses")
}

/// An API-key route: the variable, then p1's store, then both borrowed logins.
fn api_key() -> CredentialSpec {
    spec(
        r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY",
            "borrow":["opencode:opencode-go","pi:opencode-go"]}"#,
    )
}

/// A store document with one api_key entry for the route under test.
fn store_api_key(key: &str) -> String {
    serde_json::json!({ ROUTE: { "type": "api_key", "key": key } }).to_string()
}

/// A store document with one fresh oauth entry for the route under test.
fn store_oauth(access: &str, refresh: &str) -> String {
    serde_json::json!({
        ROUTE: {
            "type": "oauth",
            "access": access,
            "refresh": refresh,
            "expires": NEVER_EXPIRES_MS,
            "account_id": "FAKE-ACCOUNT",
        }
    })
    .to_string()
}

/// The chain over `locations`, with a scripted transport that has no responses: a
/// source that tries to refresh fails loudly instead of silently passing.
fn source(spec: &CredentialSpec, locations: &Locations) -> Arc<dyn CredentialSource> {
    resolve(
        ROUTE,
        spec,
        Arc::new(ScriptedTransport::new(Vec::new())),
        locations,
    )
}

async fn bearer(spec: &CredentialSpec, locations: &Locations) -> String {
    source(spec, locations)
        .access()
        .await
        .expect("the chain resolves")
        .bearer
}

#[tokio::test]
async fn env_beats_the_store_and_the_borrowed_logins() {
    let scratch = Scratch::new();
    scratch.write(STORE, &store_api_key("FAKE-STORE"));
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));
    scratch.write(PI, &login("pi", "opencode-go", "FAKE-PI"));
    let env = FakeEnv::new();
    env.set("P1_AUTH_TEST_KEY", "FAKE-ENV");

    assert_eq!(
        bearer(&api_key(), &env.locations(&scratch)).await,
        "FAKE-ENV"
    );
}

#[tokio::test]
async fn the_store_beats_a_borrowed_login() {
    let scratch = Scratch::new();
    scratch.write(STORE, &store_api_key("FAKE-STORE"));
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));

    let env = FakeEnv::new();
    assert_eq!(
        bearer(&api_key(), &env.locations(&scratch)).await,
        "FAKE-STORE"
    );
}

#[tokio::test]
async fn a_borrowed_login_is_used_when_nothing_else_has_an_entry() {
    let scratch = Scratch::new();
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));
    scratch.write(PI, &login("pi", "opencode-go", "FAKE-PI"));

    let env = FakeEnv::new();
    assert_eq!(
        bearer(&api_key(), &env.locations(&scratch)).await,
        "FAKE-OPENCODE",
        "the first borrowed login in the file's order wins"
    );
}

#[tokio::test]
async fn absent_sources_are_skipped() {
    let scratch = Scratch::new();
    // The store has an entry for ANOTHER route, and the OpenCode login has no entry
    // for this key: both are absent for this route, not errors.
    scratch.write(
        STORE,
        &serde_json::json!({ "other-route": { "type": "api_key", "key": "FAKE-OTHER" } })
            .to_string(),
    );
    scratch.write(
        OPENCODE,
        &login("opencode", "some-other-key", "FAKE-OTHER-KEY"),
    );
    scratch.write(PI, &login("pi", "opencode-go", "FAKE-PI"));

    let env = FakeEnv::new();
    assert_eq!(
        bearer(&api_key(), &env.locations(&scratch)).await,
        "FAKE-PI"
    );
}

#[tokio::test]
async fn an_unusable_source_is_an_error_not_a_skip() {
    let scratch = Scratch::new();
    // OpenCode's entry exists but is command-backed: unusable. The Pi login holds a
    // perfectly good key, and must NOT be used instead.
    scratch.write(
        OPENCODE,
        r#"{"opencode-go":{"type":"api","key":"!FAKE-COMMAND"}}"#,
    );
    scratch.write(PI, &login("pi", "opencode-go", "FAKE-PI"));

    let env = FakeEnv::new();
    let error = source(&api_key(), &env.locations(&scratch))
        .access()
        .await
        .unwrap_err();

    assert_eq!(error.kind, p1_contracts::ProviderErrorKind::Authentication);
    assert!(
        error.message.contains("opencode login") && error.message.contains("unusable"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("FAKE-PI"),
        "a broken explicit choice must not fall through: {}",
        error.message
    );
}

#[tokio::test]
async fn the_claude_oauth_chain_prefers_env_then_the_store_then_the_login() {
    let scratch = Scratch::new();
    scratch.write(
        CLAUDE,
        &claude_login("FAKE-CLAUDE-ACCESS", "FAKE-R", NEVER_EXPIRES_MS),
    );
    let env = FakeEnv::new();

    env.set("CLAUDE_CODE_OAUTH_TOKEN", "FAKE-ENV-TOKEN");
    let spec = spec(r#"{"kind":"claude-code-oauth","env":"CLAUDE_CODE_OAUTH_TOKEN"}"#);
    assert_eq!(
        bearer(&spec, &env.locations(&scratch)).await,
        "FAKE-ENV-TOKEN"
    );

    env.clear("CLAUDE_CODE_OAUTH_TOKEN");
    scratch.write(
        STORE,
        &store_oauth("FAKE-STORE-ACCESS", "FAKE-STORE-REFRESH"),
    );
    assert_eq!(
        bearer(&spec, &env.locations(&scratch)).await,
        "FAKE-STORE-ACCESS"
    );

    scratch.write(STORE, "{}");
    assert_eq!(
        bearer(&spec, &env.locations(&scratch)).await,
        "FAKE-CLAUDE-ACCESS",
        "with no env and no store entry the borrowed login answers"
    );
}

#[tokio::test]
async fn a_changed_file_value_is_seen_by_the_next_access() {
    let scratch = Scratch::new();
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-FIRST"));
    let env = FakeEnv::new();
    let source = source(&api_key(), &env.locations(&scratch));

    assert_eq!(source.access().await.unwrap().bearer, "FAKE-FIRST");
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-SECOND"));
    assert_eq!(
        source.access().await.unwrap().bearer,
        "FAKE-SECOND",
        "a rotated key needs no restart"
    );
}

#[tokio::test]
async fn an_environment_credential_is_never_refreshed() {
    let scratch = Scratch::new();
    let env = FakeEnv::new();
    env.set("P1_AUTH_TEST_KEY", "FAKE-ENV");
    let source = source(&api_key(), &env.locations(&scratch));
    let rejected = source.access().await.unwrap();

    // The same value: an error that names the variable and says what to do.
    let error = source.refresh(&rejected).await.unwrap_err();
    assert_eq!(error.kind, p1_contracts::ProviderErrorKind::Authentication);
    assert!(
        error.message.contains("P1_AUTH_TEST_KEY") && error.message.contains("never refreshed"),
        "{}",
        error.message
    );
    assert!(!error.message.contains("FAKE-ENV"), "{}", error.message);

    // A new value is the only way to replace it.
    env.set("P1_AUTH_TEST_KEY", "FAKE-ENV-NEW");
    assert_eq!(
        source.refresh(&rejected).await.unwrap().bearer,
        "FAKE-ENV-NEW"
    );
}

#[tokio::test]
async fn a_newly_set_variable_is_seen_by_the_next_access() {
    let scratch = Scratch::new();
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));
    let env = FakeEnv::new();
    let source = source(&api_key(), &env.locations(&scratch));

    assert_eq!(source.access().await.unwrap().bearer, "FAKE-OPENCODE");
    env.set("P1_AUTH_TEST_KEY", "FAKE-LATE-ENV");
    assert_eq!(
        source.access().await.unwrap().bearer,
        "FAKE-LATE-ENV",
        "a variable that appears is picked up without a restart"
    );
}
