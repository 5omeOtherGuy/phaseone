//! Issue #484: credential files are only read and written through directories
//! nobody else can change, never through a symlink, a special file or a leftover
//! file someone planted, and two refreshes never run at once even when the lock file
//! is replaced.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use p1_auth::{
    ClaudeCodeCredentials, CodexCliCredentials, CredentialSpec, Presence, describe, resolve,
};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{Credential, CredentialSource};
use serde_json::json;
use support::{Gated, LONG_EXPIRED_MS, Scratch, token_response};

const ROUTE: &str = "test-route";
const STORE: &str = ".config/p1/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";
const CODEX: &str = ".codex/auth.json";

fn claude_oauth() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"claude-code-oauth"}"#).unwrap()
}

fn api_key() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY"}"#).unwrap()
}

fn store_key(key: &str) -> String {
    json!({ ROUTE: { "type": "api_key", "key": key } }).to_string()
}

fn rotation() -> ScriptedTransport {
    ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
    }))])
}

fn codex_login(refresh: &str) -> String {
    json!({ "tokens": { "access_token": "FAKE-OPAQUE", "refresh_token": refresh } }).to_string()
}

fn unusable(presence: &Presence) -> String {
    match presence {
        Presence::Unusable(reason) => reason.clone(),
        other => panic!("expected an unusable source, got {other}"),
    }
}

/// The p1-store presence of this route over this scratch home.
fn store_presence(scratch: &Scratch) -> Presence {
    let report = describe(ROUTE, &api_key(), &scratch.locations());
    report.tried[1].1.clone()
}

// ---------------------------------------------------------------- findings 0 and 1

/// The staging file names older p1 versions used are planted in advance, one as a
/// symlink to a victim and one as a world-readable file: the refresh writes through
/// neither and leaves both exactly as they were.
#[tokio::test]
async fn planted_staging_files_are_never_opened() {
    let scratch = Scratch::new();
    scratch.write(CODEX, &codex_login("FAKE-OLD-REFRESH"));
    let victim = scratch.write("victim.json", "victim");
    let pid = std::process::id();
    std::os::unix::fs::symlink(
        &victim,
        scratch.path(&format!(".codex/.auth.json.{pid}.tmp")),
    )
    .unwrap();
    let public = scratch.path(".codex/auth.tmp-planted");
    std::fs::write(&public, "").unwrap();
    std::fs::set_permissions(&public, std::fs::Permissions::from_mode(0o644)).unwrap();

    let login = CodexCliCredentials::at(scratch.path(CODEX), Arc::new(rotation()));
    let rejected = Credential {
        bearer: "FAKE-OPAQUE".to_string(),
        account_id: None,
    };
    assert_eq!(login.refresh(&rejected).await.unwrap().bearer, "FAKE-NEW");

    assert_eq!(scratch.read("victim.json"), "victim");
    assert_eq!(std::fs::read(&public).unwrap(), b"");
    let mode = std::fs::metadata(scratch.path(CODEX))
        .unwrap()
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600);
}

// ---------------------------------------------------------------- findings 2, 3, 4 and 6

/// A credential directory that is reached through a directory everyone can write to
/// is refused BEFORE any refresh token is sent, for the store and both logins.
#[tokio::test]
async fn a_credential_directory_below_a_world_writable_one_is_refused_before_any_request() {
    // p1's store, symlinked into a world-writable place.
    let scratch = Scratch::new();
    scratch.write("open/p1/auth.json", &store_key("FAKE-KEY"));
    scratch.set_mode("open", 0o777);
    std::fs::remove_dir(scratch.path(".config")).ok();
    std::fs::create_dir_all(scratch.path(".config")).unwrap();
    std::os::unix::fs::symlink(scratch.path("open/p1"), scratch.path(".config/p1")).unwrap();
    let reason = unusable(&store_presence(&scratch));
    assert!(reason.contains("writable by every user"), "{reason}");
    assert!(!reason.contains("FAKE-KEY"), "{reason}");

    // Claude Code's login and the Codex login, with the login directory itself open to
    // everyone and with only an ancestor (the home) open to everyone.
    for open in [".claude", ""] {
        let scratch = Scratch::new();
        scratch.write(
            CLAUDE,
            &support::claude_login("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
        );
        scratch.set_mode(open, 0o777);
        let transport = rotation();
        let error = ClaudeCodeCredentials::at(scratch.path(CLAUDE), Arc::new(transport.clone()))
            .access()
            .await
            .unwrap_err();
        assert!(
            error.message.contains("writable by every user"),
            "{open:?}: {}",
            error.message
        );
        assert!(transport.requests().is_empty(), "no refresh token was sent");
    }

    for open in [".codex", ""] {
        let scratch = Scratch::new();
        scratch.write(CODEX, &codex_login("FAKE-OLD-REFRESH"));
        scratch.set_mode(open, 0o777);
        let transport = rotation();
        let rejected = Credential {
            bearer: "FAKE-OPAQUE".to_string(),
            account_id: None,
        };
        let error = CodexCliCredentials::at(scratch.path(CODEX), Arc::new(transport.clone()))
            .refresh(&rejected)
            .await
            .unwrap_err();
        assert!(
            error.message.contains("writable by every user"),
            "{open:?}: {}",
            error.message
        );
        assert!(transport.requests().is_empty(), "no refresh token was sent");
    }
}

// ---------------------------------------------------------------- findings 5, 6 and 34

/// The store file is checked on the handle that is read: a symlinked, hard-linked,
/// special or oversized store is refused, and none of them blocks.
#[test]
fn a_store_that_is_not_one_private_regular_file_is_refused() {
    let scratch = Scratch::new();
    let target = scratch.write("elsewhere.json", &store_key("FAKE-KEY"));
    scratch.write(STORE, "{}");
    std::fs::remove_file(scratch.path(STORE)).unwrap();
    std::os::unix::fs::symlink(&target, scratch.path(STORE)).unwrap();
    assert!(unusable(&store_presence(&scratch)).contains("symbolic link"));

    let scratch = Scratch::new();
    scratch.write(STORE, &store_key("FAKE-KEY"));
    std::fs::hard_link(scratch.path(STORE), scratch.path("second-name")).unwrap();
    assert!(unusable(&store_presence(&scratch)).contains("hard link"));

    let scratch = Scratch::new();
    scratch.write(STORE, "{}");
    std::fs::remove_file(scratch.path(STORE)).unwrap();
    make_fifo(&scratch.path(STORE));
    assert!(unusable(&store_presence(&scratch)).contains("not a regular file"));

    let scratch = Scratch::new();
    scratch.write(STORE, "{}");
    std::fs::File::options()
        .write(true)
        .open(scratch.path(STORE))
        .unwrap()
        .set_len(2 << 20)
        .unwrap();
    assert!(unusable(&store_presence(&scratch)).contains("larger than"));
}

/// A named pipe, made with the `mkfifo` tool so the test needs no extra crate.
fn make_fifo(path: &std::path::Path) {
    let status = std::process::Command::new("mkfifo")
        .arg("-m")
        .arg("600")
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success());
}

/// A borrowed login that is a FIFO is refused as unusable instead of blocking the
/// thread that reads it.
#[test]
fn a_borrowed_login_that_is_a_fifo_is_refused_without_blocking() {
    let scratch = Scratch::new();
    scratch.write(CLAUDE, "{}");
    std::fs::remove_file(scratch.path(CLAUDE)).unwrap();
    make_fifo(&scratch.path(CLAUDE));
    let report = describe(ROUTE, &claude_oauth(), &scratch.locations());
    let reason = unusable(&report.tried[1].1);
    assert!(reason.contains("not a regular file"), "{reason}");
}

// ---------------------------------------------------------------- finding 8

/// The lock file is replaced while a refresh holds it, and only THEN does a second
/// refresh start: it still waits (the directory itself is locked too), then uses the
/// first one's rotation. The second has no scripted response, so a rotation of its own
/// would fail it.
#[tokio::test]
async fn a_replaced_lock_file_does_not_let_a_second_rotation_run() {
    let scratch = Scratch::new();
    scratch.write(
        CLAUDE,
        &support::claude_login("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let scripted = rotation();
    let gated = Gated::new(scripted.clone());
    let first = ClaudeCodeCredentials::at(scratch.path(CLAUDE), Arc::new(gated.clone()));
    let second = ClaudeCodeCredentials::at(
        scratch.path(CLAUDE),
        Arc::new(ScriptedTransport::new(Vec::new())),
    );

    let driver = async {
        gated.entered.notified().await;
        let lock = scratch.path(".claude/.credentials.json.lock");
        std::fs::remove_file(&lock).unwrap();
        std::fs::write(&lock, "").unwrap();
        let late = second.access();
        tokio::pin!(late);
        for _ in 0..100 {
            tokio::select! {
                biased;
                result = &mut late => panic!("the second refresh did not wait: {result:?}"),
                () = tokio::task::yield_now() => {}
            }
        }
        gated.gate.notify_one();
        late.await
    };
    let (a, b) = tokio::join!(first.access(), driver);
    assert_eq!(a.unwrap().bearer, "FAKE-NEW");
    assert_eq!(b.unwrap().bearer, "FAKE-NEW");
    assert_eq!(scripted.requests().len(), 1, "exactly one rotation");
}

// ---------------------------------------------------------------- finding 24

/// No staging or kept file is left beside a credential file after a refresh: only
/// the file itself and its lock remain.
#[tokio::test]
async fn a_refresh_leaves_no_staging_file_behind() {
    let scratch = Scratch::new();
    scratch.write(
        CLAUDE,
        &support::claude_login("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let chain = resolve(
        ROUTE,
        &claude_oauth(),
        Arc::new(rotation()),
        &scratch.locations(),
    );
    assert_eq!(chain.access().await.unwrap().bearer, "FAKE-NEW");
    let mut names: Vec<String> = std::fs::read_dir(scratch.path(".claude"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    names.sort();
    assert_eq!(names, [".credentials.json", ".credentials.json.lock"]);
}
