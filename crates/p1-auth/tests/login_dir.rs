//! ADR-0074: a `claude-code-oauth` route may name the Claude Code config directory
//! whose login it borrows (`login_dir`), and a Claude Code login can be imported into
//! p1's store as the route's `oauth` entry. Every directory is a scratch one; every
//! token is obviously fake.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use p1_auth::store::{ImportError, import_claude_code_login};
use p1_auth::{CredentialSpec, SourceName, describe, resolve};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use support::{LONG_EXPIRED_MS, NEVER_EXPIRES_MS, Scratch, claude_login};

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
    // A relative directory, and the home directory itself (`~`, `~/`), are refused: a
    // Claude Code login directory is absolute or a directory BELOW the home.
    for dir in ["claude-2", "~", "~/"] {
        let error = spec(&format!(
            r#"{{"kind":"claude-code-oauth","login_dir":"{dir}"}}"#
        ))
        .validate()
        .unwrap_err();
        assert!(error.contains("absolute"), "{dir}: {error}");
    }
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

/// Review item 8: a REFRESH through a named `login_dir`. The expired token in
/// `<login_dir>/.credentials.json` is rotated against the scripted token endpoint, and the
/// rotation is written back to THAT file (0600, replaced atomically — no temp file left
/// beside it), never to the default `~/.claude`.
#[tokio::test]
async fn a_refresh_through_a_login_dir_writes_back_to_that_directory_only() {
    let scratch = Scratch::new();
    let default_login = claude_login(
        "FAKE-DEFAULT-ACCESS",
        "FAKE-DEFAULT-REFRESH",
        NEVER_EXPIRES_MS,
    );
    scratch.write(".claude/.credentials.json", &default_login);
    scratch.write(
        ".claude-2/.credentials.json",
        &claude_login("FAKE-OLD-ACCESS", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            serde_json::json!({
                "access_token": "FAKE-NEW-ACCESS",
                "refresh_token": "FAKE-NEW-REFRESH",
                "expires_in": 3600,
            })
            .to_string()
            .into_bytes(),
        ],
        end: BodyEnd::Eof,
    }]);
    let second = spec(r#"{"kind":"claude-code-oauth","login_dir":"~/.claude-2"}"#);

    let credential = resolve(
        ROUTE,
        &second,
        Arc::new(transport.clone()),
        &scratch.locations(),
    )
    .access()
    .await
    .unwrap();

    assert_eq!(credential.bearer, "FAKE-NEW-ACCESS");
    let requests = transport.requests();
    assert_eq!(requests.len(), 1, "exactly one rotation");
    let body = String::from_utf8(requests[0].body.clone()).unwrap();
    assert!(
        body.contains("FAKE-OLD-REFRESH"),
        "the named login's refresh token is used"
    );

    let written: serde_json::Value =
        serde_json::from_str(&scratch.read(".claude-2/.credentials.json")).unwrap();
    assert_eq!(written["claudeAiOauth"]["accessToken"], "FAKE-NEW-ACCESS");
    assert_eq!(written["claudeAiOauth"]["refreshToken"], "FAKE-NEW-REFRESH");
    let mode = std::fs::metadata(scratch.path(".claude-2/.credentials.json"))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
    let leftovers: Vec<String> = std::fs::read_dir(scratch.path(".claude-2"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|name| name != ".credentials.json" && name != ".credentials.json.lock")
        .collect();
    assert!(leftovers.is_empty(), "no temp file is left: {leftovers:?}");
    assert_eq!(
        scratch.read(".claude/.credentials.json"),
        default_login,
        "the default login is never touched"
    );
    assert!(
        !scratch.path(".claude/.credentials.json.lock").exists(),
        "not even locked"
    );
}
