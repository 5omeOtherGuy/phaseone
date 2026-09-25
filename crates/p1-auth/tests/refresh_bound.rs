//! Issue #164: an OAuth token refresh that gets no answer ENDS, as a named
//! authentication failure, instead of hanging the agent inside the refresh lock.
//! The bounds are ADR-0069's provider-read bounds: `FIRST_BYTE_TIMEOUT` for the
//! response headers, `STREAM_IDLE_TIMEOUT` between reads of the body.
//!
//! Fake time only: the runtime starts paused and the test advances the clock by
//! hand to just before the bound, then past it. A refresh without the bound never
//! ends, which the one-second guard after the bound turns into a failure.

mod support;

use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use p1_auth::{ClaudeCodeCredentials, CodexCliCredentials, CredentialSpec, resolve};
use p1_contracts::{BoxFuture, ProviderError, ProviderErrorKind};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{
    Credential, CredentialSource, FIRST_BYTE_TIMEOUT, HttpRequest, HttpResponse,
    STREAM_IDLE_TIMEOUT, Transport, TransportError,
};
use serde_json::json;
use support::{LONG_EXPIRED_MS, Scratch};

/// Accepts every request and never answers: `post` stays pending for good.
#[derive(Clone, Default)]
struct SilentTransport {
    requests: Arc<AtomicUsize>,
}

impl Transport for SilentTransport {
    fn post<'a>(
        &'a self,
        _request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        Box::pin(std::future::pending())
    }
}

/// Headers arrive (`200`), part of the body arrives, and then the body never ends.
fn stalled_body() -> ScriptedTransport {
    ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        chunks: vec![br#"{"access_token":"FAKE-NEVER-FINISHED"#.to_vec()],
        end: BodyEnd::Hang,
    }])
}

/// Run `refresh` on the paused clock: still pending one millisecond before `bound`,
/// ended right after it. Panics if it ends early or never ends.
async fn ends_at<T>(refresh: impl Future<Output = T>, bound: Duration) -> T {
    let start = tokio::time::Instant::now();
    tokio::pin!(refresh);
    tokio::select! {
        biased;
        _ = &mut refresh => panic!("the refresh ended before {} s", bound.as_secs()),
        () = tokio::time::advance(bound - Duration::from_millis(1)) => {}
    }
    tokio::time::advance(Duration::from_millis(1)).await;
    let output = tokio::time::timeout(Duration::from_secs(1), refresh)
        .await
        .unwrap_or_else(|_| panic!("the refresh did not end at its {} s bound", bound.as_secs()));
    assert!(start.elapsed() >= bound, "ended at {:?}", start.elapsed());
    output
}

/// The named failure: authentication, naming the bound and nothing else.
fn assert_bound_error(error: &ProviderError, bound: Duration) {
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(
        error.message,
        format!("token refresh got no response within {} s", bound.as_secs())
    );
}

fn rejected(bearer: &str) -> Credential {
    Credential {
        bearer: bearer.to_string(),
        account_id: None,
    }
}

// ------------------------------------------------------------ claude-code-oauth

fn claude_file(scratch: &Scratch) -> (std::path::PathBuf, Vec<u8>) {
    let path = scratch.write(
        ".claude/.credentials.json",
        &support::document(&json!({ "claudeAiOauth": {
            "accessToken": "FAKE-OLD-ACCESS",
            "refreshToken": "FAKE-OLD-REFRESH",
            "expiresAt": LONG_EXPIRED_MS,
            "subscriptionType": "max",
        }})),
    );
    let before = std::fs::read(&path).unwrap();
    (path, before)
}

#[tokio::test(start_paused = true)]
async fn claude_refresh_without_response_headers_ends_at_the_first_byte_bound() {
    let scratch = Scratch::new();
    let (path, before) = claude_file(&scratch);
    let transport = SilentTransport::default();
    let credentials = ClaudeCodeCredentials::at(path.clone(), Arc::new(transport.clone()));

    let rejected = rejected("FAKE-OLD-ACCESS");
    let error = ends_at(credentials.refresh(&rejected), FIRST_BYTE_TIMEOUT)
        .await
        .unwrap_err();

    assert_bound_error(&error, FIRST_BYTE_TIMEOUT);
    assert!(!error.message.contains("FAKE-"), "{}", error.message);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file byte-identical");
    assert_eq!(transport.requests.load(Ordering::SeqCst), 1, "one request");
}

#[tokio::test(start_paused = true)]
async fn claude_refresh_whose_body_never_ends_ends_at_the_idle_bound() {
    let scratch = Scratch::new();
    let (path, before) = claude_file(&scratch);
    let transport = stalled_body();
    let credentials = ClaudeCodeCredentials::at(path.clone(), Arc::new(transport.clone()));

    let rejected = rejected("FAKE-OLD-ACCESS");
    let error = ends_at(credentials.refresh(&rejected), STREAM_IDLE_TIMEOUT)
        .await
        .unwrap_err();

    assert_bound_error(&error, STREAM_IDLE_TIMEOUT);
    assert!(!error.message.contains("FAKE-"), "{}", error.message);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file byte-identical");
    assert_eq!(transport.requests().len(), 1, "one request");
}

// ----------------------------------------------------------------- codex-oauth

fn codex_file(scratch: &Scratch) -> (std::path::PathBuf, Vec<u8>) {
    let path = scratch.write(
        ".codex/auth.json",
        &support::document(&json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "FAKE-OLD-ID",
                "access_token": "FAKE-OLD-ACCESS",
                "refresh_token": "FAKE-OLD-REFRESH",
                "account_id": "FAKE-ACCOUNT",
            },
            "last_refresh": "2000-01-01T00:00:00Z",
        })),
    );
    let before = std::fs::read(&path).unwrap();
    (path, before)
}

#[tokio::test(start_paused = true)]
async fn codex_refresh_without_response_headers_ends_at_the_first_byte_bound() {
    let scratch = Scratch::new();
    let (path, before) = codex_file(&scratch);
    let transport = SilentTransport::default();
    let credentials = CodexCliCredentials::at(path.clone(), Arc::new(transport.clone()));

    let rejected = rejected("FAKE-OLD-ACCESS");
    let error = ends_at(credentials.refresh(&rejected), FIRST_BYTE_TIMEOUT)
        .await
        .unwrap_err();

    assert_bound_error(&error, FIRST_BYTE_TIMEOUT);
    assert!(!error.message.contains("FAKE-"), "{}", error.message);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file byte-identical");
    assert_eq!(transport.requests.load(Ordering::SeqCst), 1, "one request");
}

#[tokio::test(start_paused = true)]
async fn codex_refresh_whose_body_never_ends_ends_at_the_idle_bound() {
    let scratch = Scratch::new();
    let (path, before) = codex_file(&scratch);
    let transport = stalled_body();
    let credentials = CodexCliCredentials::at(path.clone(), Arc::new(transport.clone()));

    let rejected = rejected("FAKE-OLD-ACCESS");
    let error = ends_at(credentials.refresh(&rejected), STREAM_IDLE_TIMEOUT)
        .await
        .unwrap_err();

    assert_bound_error(&error, STREAM_IDLE_TIMEOUT);
    assert!(!error.message.contains("FAKE-"), "{}", error.message);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file byte-identical");
    assert_eq!(transport.requests().len(), 1, "one request");
}

// ------------------------------------------------- an oauth entry in p1's store

#[tokio::test(start_paused = true)]
async fn p1_store_refresh_without_response_headers_ends_at_the_first_byte_bound() {
    const STORE: &str = ".config/p1/auth.json";
    let scratch = Scratch::new();
    let path = scratch.write(
        STORE,
        &support::document(&json!({
            "claude-subscription": {
                "type": "oauth",
                "access": "FAKE-OLD-ACCESS",
                "refresh": "FAKE-OLD-REFRESH",
                "expires": LONG_EXPIRED_MS,
            },
        })),
    );
    let before = std::fs::read(&path).unwrap();
    let spec: CredentialSpec = serde_json::from_str(r#"{"kind":"claude-code-oauth"}"#).unwrap();
    let transport = SilentTransport::default();
    let source = resolve(
        "claude-subscription",
        &spec,
        Arc::new(transport.clone()),
        &scratch.locations(),
    );

    let error = ends_at(source.access(), FIRST_BYTE_TIMEOUT)
        .await
        .unwrap_err();

    assert_bound_error(&error, FIRST_BYTE_TIMEOUT);
    assert_eq!(std::fs::read(&path).unwrap(), before, "file byte-identical");
    assert_eq!(transport.requests.load(Ordering::SeqCst), 1, "one request");
}
