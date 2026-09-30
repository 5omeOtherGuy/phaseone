//! Issue #484: a token rotation the server already performed is never lost — not to
//! an unusable response, a cancelled caller, a publication that failed, or a peer
//! that changed the file while the request was out — and the new expiry counts from
//! when the token was minted.

mod support;

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use p1_auth::{ClaudeCodeCredentials, CodexCliCredentials, CredentialSpec, Locations, resolve};
use p1_contracts::{BoxFuture, ProviderErrorKind};
use p1_provider_http::testing::ScriptedTransport;
use p1_provider_http::{
    Credential, CredentialSource, HttpRequest, HttpResponse, Transport, TransportError,
};
use serde_json::{Value, json};
use support::{Gated, LONG_EXPIRED_MS, NEVER_EXPIRES_MS, Scratch, settle, token_response};

const ROUTE: &str = "claude-subscription";
const STORE: &str = ".config/p1/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";
const CODEX: &str = ".codex/auth.json";

fn claude_oauth() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"claude-code-oauth","store_only":true}"#).unwrap()
}

fn codex_oauth() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"codex-oauth","store_only":true}"#).unwrap()
}

fn store_entry(access: &str, refresh: &str, expires: u64) -> String {
    support::document(&json!({
        ROUTE: {
            "type": "oauth",
            "access": access,
            "refresh": refresh,
            "expires": expires,
            "account_id": "FAKE-ACCOUNT",
        },
        "zz-other-route": { "type": "api_key", "key": "FAKE-OTHER" },
    }))
}

fn rejected(bearer: &str) -> Credential {
    Credential {
        bearer: bearer.to_string(),
        account_id: None,
    }
}

fn json_of(scratch: &Scratch, relative: &str) -> Value {
    serde_json::from_str(&scratch.read(relative)).unwrap()
}

fn request_body(transport: &ScriptedTransport, index: usize) -> String {
    String::from_utf8(transport.requests()[index].body.clone()).unwrap()
}

fn source(
    spec: &CredentialSpec,
    locations: &Locations,
    transport: impl Transport + 'static,
) -> Arc<dyn CredentialSource> {
    resolve(ROUTE, spec, Arc::new(transport), locations)
}

/// A JWT-shaped Codex access token with this `exp`, built at runtime.
fn jwt(exp: i64) -> String {
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
        encode(json!({ "exp": exp }).to_string().as_bytes())
    )
}

fn epoch_seconds() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
}

fn codex_login(access: &str, refresh: &str) -> String {
    serde_json::to_string_pretty(&json!({
        "tokens": {
            "access_token": access,
            "refresh_token": refresh,
            "account_id": "FAKE-ACCOUNT",
        },
        "unknown_field": { "keep": true },
    }))
    .unwrap()
}

// ---------------------------------------------------------------- finding 26

/// The server rotates the refresh token but hands back the rejected access token:
/// the call fails, but the NEW refresh token is kept, and the next refresh uses it.
#[tokio::test]
async fn a_rotation_that_returns_the_rejected_token_still_keeps_the_new_refresh_token() {
    // p1's store.
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_entry("FAKE-REJECTED", "FAKE-OLD-REFRESH", NEVER_EXPIRES_MS),
    );
    let transport = ScriptedTransport::new(vec![
        token_response(json!({
            "access_token": "FAKE-REJECTED", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
        })),
        token_response(json!({
            "access_token": "FAKE-FRESH", "refresh_token": "FAKE-NEWER-REFRESH", "expires_in": 3600,
        })),
    ]);
    let chain = source(&claude_oauth(), &scratch.locations(), transport.clone());
    let error = chain.refresh(&rejected("FAKE-REJECTED")).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert!(!error.message.contains("FAKE-"), "{}", error.message);
    assert_eq!(
        json_of(&scratch, STORE)[ROUTE]["refresh"],
        "FAKE-NEW-REFRESH"
    );

    assert_eq!(chain.access().await.unwrap().bearer, "FAKE-FRESH");
    assert!(request_body(&transport, 1).contains("FAKE-NEW-REFRESH"));

    // Claude Code's login.
    let scratch = Scratch::new();
    scratch.write(
        CLAUDE,
        &support::claude_login("FAKE-REJECTED", "FAKE-OLD-REFRESH", NEVER_EXPIRES_MS),
    );
    let transport = ScriptedTransport::new(vec![
        token_response(json!({
            "access_token": "FAKE-REJECTED", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
        })),
        token_response(json!({
            "access_token": "FAKE-FRESH", "refresh_token": "FAKE-NEWER-REFRESH", "expires_in": 3600,
        })),
    ]);
    let login = ClaudeCodeCredentials::at(scratch.path(CLAUDE), Arc::new(transport.clone()));
    assert!(login.refresh(&rejected("FAKE-REJECTED")).await.is_err());
    assert_eq!(
        json_of(&scratch, CLAUDE)["claudeAiOauth"]["refreshToken"],
        "FAKE-NEW-REFRESH"
    );
    assert_eq!(login.access().await.unwrap().bearer, "FAKE-FRESH");
    assert!(request_body(&transport, 1).contains("FAKE-NEW-REFRESH"));

    // The Codex login.
    let scratch = Scratch::new();
    let old = jwt(epoch_seconds() + 3600);
    scratch.write(CODEX, &codex_login(&old, "FAKE-OLD-REFRESH"));
    let fresh = jwt(epoch_seconds() + 7200);
    let transport = ScriptedTransport::new(vec![
        token_response(json!({ "access_token": old, "refresh_token": "FAKE-NEW-REFRESH" })),
        token_response(json!({ "access_token": fresh, "refresh_token": "FAKE-NEWER-REFRESH" })),
    ]);
    let login = CodexCliCredentials::at(scratch.path(CODEX), Arc::new(transport.clone()));
    assert!(login.refresh(&rejected(&old)).await.is_err());
    assert_eq!(
        json_of(&scratch, CODEX)["tokens"]["refresh_token"],
        "FAKE-NEW-REFRESH"
    );
    assert_eq!(login.refresh(&rejected(&old)).await.unwrap().bearer, fresh);
    assert!(request_body(&transport, 1).contains("FAKE-NEW-REFRESH"));
}

// ---------------------------------------------------------------- finding 27

/// The caller is cancelled while the server's answer is on its way: the rotation
/// still lands in the file, so the consumed refresh token is never the last one kept.
#[tokio::test]
async fn a_cancelled_caller_does_not_lose_a_rotation_in_flight() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_entry("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let gated = Gated::new(ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
    }))]));
    let chain = source(&claude_oauth(), &scratch.locations(), gated.clone());

    tokio::select! {
        _ = chain.access() => panic!("the refresh cannot finish before the gate opens"),
        () = gated.entered.notified() => {}
    }
    // The caller is gone; the server answers now.
    gated.gate.notify_one();
    settle(|| scratch.read(STORE).contains("FAKE-NEW-REFRESH")).await;
    assert_eq!(json_of(&scratch, STORE)[ROUTE]["access"], "FAKE-NEW");
}

// ---------------------------------------------------------------- findings 7 and 28

/// The store directory is opened to others while the request is out: nothing is
/// published into it, the rotated login is kept privately beside the store, and the
/// next refresh (once the directory is private again) adopts it without a second
/// rotation.
#[tokio::test]
async fn a_store_that_turns_public_mid_refresh_gets_nothing_and_keeps_the_rotation() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_entry("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let before = scratch.read(STORE);
    let scripted = ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
    }))]);
    let gated = Gated::new(scripted.clone());
    let chain = source(&claude_oauth(), &scratch.locations(), gated.clone());

    let refresh = chain.access();
    tokio::pin!(refresh);
    tokio::select! {
        _ = &mut refresh => panic!("the refresh cannot finish before the gate opens"),
        () = gated.entered.notified() => {}
    }
    scratch.set_mode(".config/p1", 0o755);
    gated.gate.notify_one();
    let error = refresh.await.unwrap_err();
    assert!(error.message.contains("chmod 700"), "{}", error.message);
    assert!(!error.message.contains("FAKE-"), "{}", error.message);
    assert_eq!(scratch.read(STORE), before, "nothing was published");

    scratch.set_mode(".config/p1", 0o700);
    assert_eq!(chain.access().await.unwrap().bearer, "FAKE-NEW");
    assert_eq!(scripted.requests().len(), 1, "the kept rotation is adopted");
    assert_eq!(
        json_of(&scratch, STORE)[ROUTE]["refresh"],
        "FAKE-NEW-REFRESH"
    );
}

// ---------------------------------------------------------------- finding 30

/// Another writer installs a different login while the refresh request is out: it
/// is never overwritten by the rotation that started from the old one.
#[tokio::test]
async fn a_login_replaced_during_the_request_is_not_overwritten() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_entry("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let gated = Gated::new(ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-OURS", "refresh_token": "FAKE-OUR-REFRESH", "expires_in": 3600,
    }))]));
    let chain = source(&claude_oauth(), &scratch.locations(), gated.clone());

    let refresh = chain.access();
    tokio::pin!(refresh);
    tokio::select! {
        _ = &mut refresh => panic!("the refresh cannot finish before the gate opens"),
        () = gated.entered.notified() => {}
    }
    let replaced = store_entry("FAKE-IMPORTED", "FAKE-IMPORTED-REFRESH", NEVER_EXPIRES_MS);
    scratch.write(STORE, &replaced);
    gated.gate.notify_one();

    // The newer login answers; the file is exactly what the other writer wrote.
    assert_eq!(refresh.await.unwrap().bearer, "FAKE-IMPORTED");
    assert_eq!(scratch.read(STORE), replaced);
}

/// The login turns unreadable (a half-written edit) while the refresh request is out:
/// the server already rotated the refresh token, so the rotation is kept beside the
/// file instead of being dropped, and the usable access token is still returned.
#[tokio::test]
async fn a_login_unreadable_after_the_request_keeps_the_rotation() {
    for (login, file, kept) in [
        (
            CLAUDE,
            support::claude_login("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
            ".claude/..credentials.json.p1-unsaved",
        ),
        (
            CODEX,
            codex_login(&jwt(1), "FAKE-OLD-REFRESH"),
            ".codex/.auth.json.p1-unsaved",
        ),
    ] {
        let scratch = Scratch::new();
        scratch.write(login, &file);
        let gated = Gated::new(ScriptedTransport::new(vec![token_response(json!({
            "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
        }))]));
        let refresh = async {
            if login == CLAUDE {
                ClaudeCodeCredentials::at(scratch.path(login), Arc::new(gated.clone()))
                    .access()
                    .await
            } else {
                CodexCliCredentials::at(scratch.path(login), Arc::new(gated.clone()))
                    .access()
                    .await
            }
        };
        tokio::pin!(refresh);
        tokio::select! {
            _ = &mut refresh => panic!("the refresh cannot finish before the gate opens"),
            () = gated.entered.notified() => {}
        }
        scratch.write(login, "{ half written");
        gated.gate.notify_one();

        assert_eq!(refresh.await.unwrap().bearer, "FAKE-NEW", "{login}");
        assert_eq!(scratch.read(login), "{ half written", "{login}");
        let kept: Value = serde_json::from_str(&scratch.read(kept)).unwrap();
        assert!(
            kept.to_string().contains("FAKE-NEW-REFRESH"),
            "{login}: {kept}"
        );
    }
}

/// The Codex CLI writes a login for ANOTHER account while the refresh request is out:
/// its file is left alone, and the rotated token goes out with the account it belongs
/// to, never with the new file's account.
#[tokio::test]
async fn a_codex_login_replaced_during_the_request_never_mixes_accounts() {
    let scratch = Scratch::new();
    scratch.write(CODEX, &codex_login(&jwt(1), "FAKE-OLD-REFRESH"));
    let gated = Gated::new(ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
    }))]));
    let login = CodexCliCredentials::at(scratch.path(CODEX), Arc::new(gated.clone()));
    let refresh = login.access();
    tokio::pin!(refresh);
    tokio::select! {
        _ = &mut refresh => panic!("the refresh cannot finish before the gate opens"),
        () = gated.entered.notified() => {}
    }
    let other = serde_json::to_string_pretty(&json!({
        "tokens": {
            "access_token": "FAKE-OTHER",
            "refresh_token": "FAKE-OTHER-REFRESH",
            "account_id": "FAKE-OTHER-ACCOUNT",
        },
    }))
    .unwrap();
    scratch.write(CODEX, &other);
    gated.gate.notify_one();

    let credential = refresh.await.unwrap();
    assert_eq!(credential.bearer, "FAKE-NEW");
    assert_eq!(credential.account_id.as_deref(), Some("FAKE-ACCOUNT"));
    assert_eq!(scratch.read(CODEX), other);
}

// ---------------------------------------------------------------- finding 40

static SLOW_CLOCK: AtomicU64 = AtomicU64::new(0);
const NOW_MS: u64 = 1_700_000_000_000;

fn slow_clock() -> u64 {
    SLOW_CLOCK.load(Ordering::SeqCst)
}

/// Ten minutes pass between sending the refresh and reading its answer.
struct SlowAnswer(ScriptedTransport);

impl Transport for SlowAnswer {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            SLOW_CLOCK.store(NOW_MS + 600_000, Ordering::SeqCst);
            self.0.post(request).await
        })
    }
}

#[tokio::test]
async fn the_new_expiry_counts_from_when_the_request_was_sent() {
    let scratch = Scratch::new();
    SLOW_CLOCK.store(NOW_MS, Ordering::SeqCst);
    scratch.write(
        CLAUDE,
        &support::claude_login("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let transport = SlowAnswer(ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
    }))]));
    let login =
        ClaudeCodeCredentials::at(scratch.path(CLAUDE), Arc::new(transport)).with_clock(slow_clock);
    assert_eq!(login.access().await.unwrap().bearer, "FAKE-NEW");
    assert_eq!(
        json_of(&scratch, CLAUDE)["claudeAiOauth"]["expiresAt"],
        json!(NOW_MS + 3_600_000)
    );
}

// ---------------------------------------------------------------- finding 41

/// A zero lifetime, or one inside the refresh margin, is no usable token: the call
/// fails, but the rotated refresh token is kept and the entry is marked expired, so the
/// next access refreshes again instead of every access rotating a short-lived token.
#[tokio::test]
async fn a_lifetime_inside_the_refresh_margin_is_refused_but_the_rotation_is_kept() {
    for lifetime in [0, 120, 300] {
        let scratch = Scratch::new();
        scratch.write(
            STORE,
            &store_entry("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
        );
        let transport = ScriptedTransport::new(vec![token_response(json!({
            "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH",
            "expires_in": lifetime,
        }))]);
        let chain = source(&claude_oauth(), &scratch.locations(), transport);
        let error = chain.access().await.unwrap_err();
        assert!(
            error.message.contains("too short to use"),
            "{lifetime}: {}",
            error.message
        );
        let entry = &json_of(&scratch, STORE)[ROUTE];
        assert_eq!(entry["refresh"], "FAKE-NEW-REFRESH");
        assert_eq!(entry["expires"], 0);
        assert_eq!(entry["access"], "FAKE-OLD");
    }
}

// ---------------------------------------------------------------- finding 39

/// A peer's replacement that has itself expired is not handed back: the refresh
/// rotates.
#[tokio::test]
async fn codex_refresh_never_returns_an_already_expired_peer_token() {
    let scratch = Scratch::new();
    let expired_peer = jwt(epoch_seconds() - 60);
    scratch.write(CODEX, &codex_login(&expired_peer, "FAKE-REFRESH"));
    let fresh = jwt(epoch_seconds() + 3600);
    let transport = ScriptedTransport::new(vec![token_response(json!({
        "access_token": fresh, "refresh_token": "FAKE-NEW-REFRESH",
    }))]);
    let login = CodexCliCredentials::at(scratch.path(CODEX), Arc::new(transport.clone()));
    let credential = login
        .refresh(&rejected("FAKE-SOMETHING-ELSE"))
        .await
        .unwrap();
    assert_eq!(credential.bearer, fresh);
    assert_eq!(transport.requests().len(), 1);
}

// ---------------------------------------------------------------- finding 50

/// p1's store refreshes a Codex-dialect entry against the Codex endpoint, keeps the
/// account id and every other entry, and sends exactly one request.
#[tokio::test]
async fn a_codex_store_entry_refreshes_through_the_codex_endpoint() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_entry("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let transport = ScriptedTransport::new(vec![token_response(json!({
        "access_token": "FAKE-NEW", "refresh_token": "FAKE-NEW-REFRESH", "expires_in": 3600,
    }))]);
    let chain = source(&codex_oauth(), &scratch.locations(), transport.clone());
    let credential = chain.access().await.unwrap();
    assert_eq!(credential.bearer, "FAKE-NEW");
    assert_eq!(credential.account_id.as_deref(), Some("FAKE-ACCOUNT"));
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");
    assert!(request_body(&transport, 0).starts_with("grant_type=refresh_token"));
    let document = json_of(&scratch, STORE);
    assert_eq!(document[ROUTE]["refresh"], "FAKE-NEW-REFRESH");
    assert_eq!(document[ROUTE]["account_id"], "FAKE-ACCOUNT");
    assert_eq!(document["zz-other-route"]["key"], "FAKE-OTHER");
}

/// A Codex store refresh whose response lacks the rotation fields fails and leaves
/// the store byte-identical.
#[tokio::test]
async fn a_codex_store_refresh_without_the_needed_fields_writes_nothing() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &store_entry("FAKE-OLD", "FAKE-OLD-REFRESH", LONG_EXPIRED_MS),
    );
    let before = scratch.read(STORE);
    let transport =
        ScriptedTransport::new(vec![token_response(json!({ "access_token": "FAKE-NEW" }))]);
    let chain = source(&codex_oauth(), &scratch.locations(), transport);
    let error = chain.access().await.unwrap_err();
    assert!(error.message.contains("lifetime"), "{}", error.message);
    assert_eq!(scratch.read(STORE), before);
}
