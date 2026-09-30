//! Issue #484: what a credential source accepts. A token a header cannot carry, an
//! expiry that is not a timestamp, a changed variable, a `login_dir` that walks back
//! to the home, and a store that cannot be read are all errors that name the source —
//! never a silently fresh token, a transport failure later, or "nothing there".

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_auth::store::remove;
use p1_auth::{CredentialSpec, Locations, Presence, describe, resolve};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use serde_json::json;
use support::{NEVER_EXPIRES_MS, Scratch, claude_login, token_response};

const ROUTE: &str = "test-route";
const STORE: &str = ".config/p1/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";
const CODEX: &str = ".codex/auth.json";

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).unwrap()
}

fn chain(spec: &CredentialSpec, locations: &Locations) -> Arc<dyn CredentialSource> {
    resolve(
        ROUTE,
        spec,
        Arc::new(ScriptedTransport::new(Vec::new())),
        locations,
    )
}

fn unusable(presence: &Presence) -> String {
    match presence {
        Presence::Unusable(reason) => reason.clone(),
        other => panic!("expected an unusable source, got {other}"),
    }
}

fn store_oauth(fields: serde_json::Value) -> String {
    let mut entry = json!({ "type": "oauth", "access": "FAKE-ACCESS", "refresh": "FAKE-REFRESH" });
    for (key, value) in fields.as_object().unwrap() {
        entry[key] = value.clone();
    }
    json!({ ROUTE: entry }).to_string()
}

// ---------------------------------------------------------------- finding 35

/// The store issued the rejected token and its entry is gone since; the Claude
/// Code login now answers the chain and holds a copy of the same token. Only the
/// source that issued the credential may be rotated: the login is not refreshed.
#[tokio::test]
async fn a_refresh_never_rotates_a_source_that_did_not_issue_the_credential() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_oauth(json!({ "access": "FAKE-SHARED", "expires": NEVER_EXPIRES_MS })),
    );
    let login = claude_login("FAKE-SHARED", "FAKE-LOGIN-REFRESH", NEVER_EXPIRES_MS);
    scratch.write(CLAUDE, &login);
    let transport = ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-ROTATED", "refresh_token": "FAKE-R2", "expires_in": 3600,
    }))]);
    let source = resolve(
        ROUTE,
        &spec(r#"{"kind":"claude-code-oauth"}"#),
        Arc::new(transport.clone()),
        &scratch.locations(),
    );
    let issued = source.access().await.unwrap();
    assert_eq!(issued.bearer, "FAKE-SHARED");

    assert!(remove(ROUTE, &scratch.locations()).await.unwrap());
    assert!(source.refresh(&issued).await.is_err());
    assert!(transport.requests().is_empty(), "the login was not rotated");
    assert_eq!(scratch.read(CLAUDE), login);
}

/// The same bearer is handed out by the store and then, once the store entry is gone,
/// by the Claude Code login: which copy a rejection refers to is unknown, so neither
/// source is rotated.
#[tokio::test]
async fn a_bearer_two_sources_handed_out_is_never_rotated() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_oauth(json!({ "access": "FAKE-SHARED", "expires": NEVER_EXPIRES_MS })),
    );
    let login = claude_login("FAKE-SHARED", "FAKE-LOGIN-REFRESH", NEVER_EXPIRES_MS);
    scratch.write(CLAUDE, &login);
    let transport = ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-ROTATED", "refresh_token": "FAKE-R2", "expires_in": 3600,
    }))]);
    let source = resolve(
        ROUTE,
        &spec(r#"{"kind":"claude-code-oauth"}"#),
        Arc::new(transport.clone()),
        &scratch.locations(),
    );
    let from_store = source.access().await.unwrap();
    assert!(remove(ROUTE, &scratch.locations()).await.unwrap());
    let from_login = source.access().await.unwrap();
    assert_eq!(from_store.bearer, from_login.bearer);

    let error = source.refresh(&from_store).await.unwrap_err();
    assert!(
        error.message.contains("more than one source"),
        "{}",
        error.message
    );
    assert!(transport.requests().is_empty(), "nothing was rotated");
    assert_eq!(scratch.read(CLAUDE), login);
}

// ---------------------------------------------------------------- finding 36

/// The variable is usable when the chain picks it and changes to something a header
/// cannot carry before the refresh: an authentication error, not a credential.
#[tokio::test]
async fn a_changed_variable_is_held_to_the_same_rule_on_refresh() {
    for invalid in ["", "  ", "FAKE\r\nX", "FAKE KEY", "FAKE\u{e9}"] {
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = calls.clone();
        let locations = Scratch::new().locations().with_env_lookup(move |name| {
            (name == "P1_AUTH_TEST_KEY").then(|| {
                if counted.fetch_add(1, Ordering::SeqCst) < 2 {
                    "FAKE-FIRST".to_string()
                } else {
                    invalid.to_string()
                }
            })
        });
        let source = chain(
            &spec(r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY"}"#),
            &locations,
        );
        let first = source.access().await.unwrap();
        let error = source.refresh(&first).await.unwrap_err();
        assert!(
            error.message.contains("no usable token"),
            "{invalid:?}: {}",
            error.message
        );
    }
}

// ---------------------------------------------------------------- findings 37 and 38

/// Tokens and account ids a header cannot carry, and expiries that are not
/// timestamps, make the entry unusable — for the store, Claude Code and Codex.
#[test]
fn unusable_tokens_and_expiries_are_refused_by_every_source() {
    let claude = spec(r#"{"kind":"claude-code-oauth"}"#);
    for fields in [
        json!({ "access": "FAKE\r\nINJECTED" }),
        json!({ "access": "FAKE ACCESS" }),
        json!({ "access": "FAKE-\u{e9}" }),
        json!({ "account_id": "FAKE\nACCOUNT" }),
        json!({ "expires": "soon" }),
        json!({ "expires": -5 }),
        json!({ "expires": { "at": 1 } }),
        json!({ "refresh": 42 }),
    ] {
        let scratch = Scratch::new();
        scratch.write(STORE, &store_oauth(fields.clone()));
        let report = describe(ROUTE, &claude, &scratch.locations());
        let reason = unusable(&report.tried[0].1);
        assert!(!reason.contains("FAKE"), "{fields}: {reason}");
    }
    // A missing or null expiry is still "no expiry".
    for fields in [json!({}), json!({ "expires": null })] {
        let scratch = Scratch::new();
        scratch.write(STORE, &store_oauth(fields));
        assert_eq!(
            describe(ROUTE, &claude, &scratch.locations()).tried[0].1,
            Presence::Present
        );
    }

    for login in [
        json!({ "claudeAiOauth": { "accessToken": "FAKE ACCESS", "expiresAt": 1 } }),
        json!({ "claudeAiOauth": { "accessToken": "FAKE-ACCESS", "expiresAt": "tomorrow" } }),
    ] {
        let scratch = Scratch::new();
        scratch.write(CLAUDE, &login.to_string());
        let report = describe(ROUTE, &claude, &scratch.locations());
        unusable(&report.tried[1].1);
    }

    let codex = spec(r#"{"kind":"codex-oauth"}"#);
    for tokens in [
        json!({ "access_token": "FAKE ACCESS" }),
        json!({ "access_token": "   " }),
        json!({ "access_token": "FAKE-ACCESS", "account_id": "FAKE\tACCOUNT" }),
        json!({ "access_token": jwt_with_payload(&json!({ "exp": "later" })) }),
    ] {
        let scratch = Scratch::new();
        scratch.write(CODEX, &json!({ "tokens": tokens }).to_string());
        let report = describe(ROUTE, &codex, &scratch.locations());
        let presence = &report.tried[1].1;
        assert!(
            matches!(presence, Presence::Unusable(_)),
            "{tokens}: {presence}"
        );
    }
}

/// A token-refresh response whose access token a header cannot carry is refused;
/// nothing unusable is handed to the transport.
#[tokio::test]
async fn a_refreshed_token_a_header_cannot_carry_is_refused() {
    let scratch = Scratch::new();
    scratch.write(CLAUDE, &claude_login("FAKE-OLD", "FAKE-OLD-REFRESH", 1));
    let transport = ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE NEW\r\nX-Injected: 1", "expires_in": 3600,
    }))]);
    let source = resolve(
        ROUTE,
        &spec(r#"{"kind":"claude-code-oauth"}"#),
        Arc::new(transport),
        &scratch.locations(),
    );
    let error = source.access().await.unwrap_err();
    assert!(error.message.contains("header-safe"), "{}", error.message);
    assert!(!error.message.contains("Injected"), "{}", error.message);
}

fn jwt_with_payload(payload: &serde_json::Value) -> String {
    fn encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let triple = (u32::from(chunk[0]) << 16)
                | (u32::from(*chunk.get(1).unwrap_or(&0)) << 8)
                | u32::from(*chunk.get(2).unwrap_or(&0));
            for index in 0..=chunk.len() {
                out.push(ALPHABET[((triple >> (18 - 6 * index)) & 0x3F) as usize] as char);
            }
        }
        out
    }
    format!(
        "{}.{}.sig",
        encode(br#"{"alg":"none"}"#),
        encode(payload.to_string().as_bytes())
    )
}

// ---------------------------------------------------------------- finding 43

/// `logout` reports "nothing to remove" only for a store that does not exist.
#[tokio::test]
async fn logout_of_an_unreadable_store_is_an_error_not_nothing() {
    let scratch = Scratch::new();
    std::fs::create_dir_all(scratch.path(STORE)).unwrap();
    support::set_mode(&scratch.path(".config/p1"), 0o700);
    let error = remove(ROUTE, &scratch.locations()).await.unwrap_err();
    assert!(error.contains("not a regular file"), "{error}");

    let scratch = Scratch::new();
    scratch.write(STORE, "{}");
    scratch.set_mode(".config/p1", 0o755);
    let error = remove(ROUTE, &scratch.locations()).await.unwrap_err();
    assert!(error.contains("chmod 700"), "{error}");

    let missing = Scratch::new();
    assert!(!remove(ROUTE, &missing.locations()).await.unwrap());
}

// ---------------------------------------------------------------- finding 44

/// A `login_dir` that walks back to the home, out of it or to the root is a load
/// error; a plain one is not.
#[test]
fn a_login_dir_that_resolves_to_the_home_or_escapes_it_is_refused() {
    for dir in [
        "~/.",
        "~/sub/..",
        "~/../other",
        "~//",
        "~/./.claude-2",
        "/",
        "//",
        "/home/../etc",
    ] {
        let table = format!(r#"{{"kind":"claude-code-oauth","login_dir":"{dir}"}}"#);
        assert!(spec(&table).validate().is_err(), "{dir} must be refused");
    }
    for dir in ["~/.claude-2", "~/a/b", "/srv/claude"] {
        let table = format!(r#"{{"kind":"claude-code-oauth","login_dir":"{dir}"}}"#);
        assert!(spec(&table).validate().is_ok(), "{dir} is fine");
    }
    // An absolute path that IS the home names no Claude Code directory.
    let scratch = Scratch::new();
    let home = scratch.home();
    assert_eq!(
        scratch
            .locations()
            .claude_code_dir(Some(home.to_str().unwrap())),
        None
    );
}

// ---------------------------------------------------------------- finding 45

/// A fresh token that was rejected, with no refresh token recorded: the message says
/// it cannot be replaced, and does not claim it expired.
#[tokio::test]
async fn a_rejected_fresh_token_without_a_refresh_token_is_not_called_expired() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &json!({ ROUTE: { "type": "oauth", "access": "FAKE-ACCESS", "expires": NEVER_EXPIRES_MS } })
            .to_string(),
    );
    let source = chain(
        &spec(r#"{"kind":"claude-code-oauth"}"#),
        &scratch.locations(),
    );
    let rejected = Credential {
        bearer: "FAKE-ACCESS".to_string(),
        account_id: None,
    };
    let error = source.refresh(&rejected).await.unwrap_err();
    assert!(error.message.contains("rejected"), "{}", error.message);
    assert!(!error.message.contains("expired"), "{}", error.message);
}
