//! ADR-0074: a `claude-code-oauth` route may name the Claude Code config directory
//! whose login it borrows (`login_dir`), and a Claude Code login can be imported into
//! p1's store as the route's `oauth` entry. Every directory is a scratch one; every
//! token is obviously fake.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use p1_auth::store::{ImportError, import_claude_code_login};
use p1_auth::{CredentialSpec, SourceName, describe, resolve};
use p1_provider_http::testing::ScriptedTransport;
use support::{NEVER_EXPIRES_MS, Scratch, claude_login};

const ROUTE: &str = "test-route-2";
const SENTINEL_ACCESS: &str = "FAKE-SENTINEL-ACCESS";
const SENTINEL_REFRESH: &str = "FAKE-SENTINEL-REFRESH";

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).unwrap()
}

async fn bearer(spec: &CredentialSpec, scratch: &Scratch) -> String {
    resolve(
        ROUTE,
        spec,
        Arc::new(ScriptedTransport::new(Vec::new())),
        &scratch.locations(),
    )
    .access()
    .await
    .unwrap()
    .bearer
}

#[tokio::test]
async fn a_login_dir_is_parsed_expanded_and_borrowed_and_absent_keeps_the_default() {
    let scratch = Scratch::new();
    scratch.write(
        ".claude/.credentials.json",
        &claude_login("FAKE-FIRST", "R1", NEVER_EXPIRES_MS),
    );
    scratch.write(
        ".claude-2/.credentials.json",
        &claude_login("FAKE-SECOND", "R2", NEVER_EXPIRES_MS),
    );

    let second = spec(r#"{"kind":"claude-code-oauth","login_dir":"~/.claude-2"}"#);
    assert_eq!(second.login_dir.as_deref(), Some("~/.claude-2"));
    second.validate().unwrap();
    assert_eq!(
        describe(ROUTE, &second, &scratch.locations()).chosen,
        Some(SourceName::ClaudeCodeLogin)
    );
    assert_eq!(bearer(&second, &scratch).await, "FAKE-SECOND");

    // Absent: the default directory, exactly as before.
    let first = spec(r#"{"kind":"claude-code-oauth"}"#);
    assert_eq!(first.login_dir, None);
    assert_eq!(bearer(&first, &scratch).await, "FAKE-FIRST");

    // An absolute directory is taken as written.
    let absolute = scratch.path("elsewhere");
    scratch.write(
        "elsewhere/.credentials.json",
        &claude_login("FAKE-ABSOLUTE", "R3", NEVER_EXPIRES_MS),
    );
    let absolute = spec(&format!(
        r#"{{"kind":"claude-code-oauth","login_dir":"{}"}}"#,
        absolute.display()
    ));
    absolute.validate().unwrap();
    assert_eq!(bearer(&absolute, &scratch).await, "FAKE-ABSOLUTE");

    // A named directory with no login is absent: it never falls back to the default one.
    let empty = spec(r#"{"kind":"claude-code-oauth","login_dir":"~/.claude-3"}"#);
    assert_eq!(describe(ROUTE, &empty, &scratch.locations()).chosen, None);
}

#[test]
fn a_login_dir_on_any_other_kind_or_a_relative_one_is_a_load_error() {
    for json in [
        r#"{"kind":"api-key","env":"A_KEY","login_dir":"~/.claude-2"}"#,
        r#"{"kind":"codex-oauth","login_dir":"~/.claude-2"}"#,
        r#"{"kind":"none","login_dir":"~/.claude-2"}"#,
    ] {
        let error = spec(json).validate().unwrap_err();
        assert!(
            error.contains("login_dir") && error.contains("claude-code-oauth"),
            "{json}: {error}"
        );
    }
    let error = spec(r#"{"kind":"claude-code-oauth","login_dir":"claude-2"}"#)
        .validate()
        .unwrap_err();
    assert!(error.contains("absolute"), "{error}");
}

#[tokio::test]
async fn an_import_writes_exactly_the_store_oauth_shape_private_and_never_says_a_token() {
    let scratch = Scratch::new();
    scratch.write(
        ".claude-2/.credentials.json",
        &claude_login(SENTINEL_ACCESS, SENTINEL_REFRESH, NEVER_EXPIRES_MS),
    );
    scratch.write(
        ".claude-2/.claude.json",
        r#"{"oauthAccount":{"accountUuid":"fake-account"}}"#,
    );
    // Another route's entry survives the import untouched.
    scratch.write(
        ".config/p1/auth.json",
        r#"{"other-route":{"type":"api_key","key":"FAKE-OTHER"}}"#,
    );
    let locations = scratch.locations();

    import_claude_code_login(ROUTE, &scratch.path(".claude-2"), &locations)
        .await
        .unwrap();

    let store: serde_json::Value =
        serde_json::from_str(&scratch.read(".config/p1/auth.json")).unwrap();
    assert_eq!(
        store,
        serde_json::json!({
            "other-route": { "type": "api_key", "key": "FAKE-OTHER" },
            ROUTE: {
                "type": "oauth",
                "access": SENTINEL_ACCESS,
                "refresh": SENTINEL_REFRESH,
                "expires": NEVER_EXPIRES_MS,
                "account_id": "fake-account",
            }
        })
    );
    let mode = |relative: &str| {
        std::fs::metadata(scratch.path(relative))
            .unwrap()
            .permissions()
            .mode()
            & 0o777
    };
    assert_eq!(mode(".config/p1/auth.json"), 0o600);
    assert_eq!(mode(".config/p1"), 0o700);

    // The imported entry is what a store-only route now reads.
    let store_only = spec(r#"{"kind":"claude-code-oauth","store_only":true}"#);
    assert_eq!(
        describe(ROUTE, &store_only, &locations).chosen,
        Some(SourceName::P1Store)
    );
    assert_eq!(bearer(&store_only, &scratch).await, SENTINEL_ACCESS);

    // A login without an account file still imports, with `null` for what it lacks.
    let bare = Scratch::new();
    bare.write(
        "login/.credentials.json",
        r#"{"claudeAiOauth":{"accessToken":"FAKE-BARE"}}"#,
    );
    import_claude_code_login(ROUTE, &bare.path("login"), &bare.locations())
        .await
        .unwrap();
    let store: serde_json::Value =
        serde_json::from_str(&bare.read(".config/p1/auth.json")).unwrap();
    assert_eq!(
        store[ROUTE],
        serde_json::json!({
            "type": "oauth",
            "access": "FAKE-BARE",
            "refresh": null,
            "expires": null,
            "account_id": null,
        })
    );

    // Every error the import can produce names paths and routes only.
    let mut seen = String::new();
    let missing = import_claude_code_login(ROUTE, &scratch.path("nowhere"), &locations)
        .await
        .unwrap_err();
    assert!(matches!(missing, ImportError::NoLogin(_)), "{missing:?}");
    assert!(missing.to_string().contains("claude"), "{missing}");
    seen.push_str(&format!("{missing} {missing:?}"));
    scratch.set_mode(".config/p1", 0o755);
    let wide = import_claude_code_login(ROUTE, &scratch.path(".claude-2"), &locations)
        .await
        .unwrap_err();
    assert!(wide.to_string().contains("chmod 700"), "{wide}");
    seen.push_str(&format!("{wide} {wide:?}"));
    scratch.set_mode(".config/p1", 0o700);
    scratch.write(
        "broken/.credentials.json",
        &format!(r#"{{"claudeAiOauth":{{"refreshToken":"{SENTINEL_REFRESH}"}}}}"#),
    );
    let broken = import_claude_code_login(ROUTE, &scratch.path("broken"), &locations)
        .await
        .unwrap_err();
    seen.push_str(&format!("{broken} {broken:?}"));
    assert!(
        !seen.contains(SENTINEL_ACCESS) && !seen.contains(SENTINEL_REFRESH),
        "a token leaked: {seen}"
    );
}
