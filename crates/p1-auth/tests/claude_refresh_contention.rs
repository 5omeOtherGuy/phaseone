//! Review finding R2: two agents on the same route refresh at the same time, on a
//! current-thread runtime. The first holds the credentials lock across its refresh
//! request; the second must WAIT for it without blocking the only thread — or the
//! first request can never complete and release the lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use p1_auth::ClaudeCodeCredentials;
use p1_contracts::{BoxFuture, ProviderErrorKind};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{
    CredentialSource, FIRST_BYTE_TIMEOUT, HttpRequest, HttpResponse, LOCK_PATIENCE, Transport,
    TransportError,
};
use serde_json::json;
use tokio::sync::Notify;

const NOW: u64 = 1_700_000_000_000;

fn now() -> u64 {
    NOW
}

/// Holds every request until the test opens the gate.
struct GatedTransport {
    inner: ScriptedTransport,
    entered: Arc<Notify>,
    gate: Arc<Notify>,
}

impl Transport for GatedTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.gate.notified().await;
            self.inner.post(request).await
        })
    }
}

#[tokio::test(start_paused = true)]
async fn a_second_refresh_waits_for_the_first_without_blocking_the_runtime() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    let expired = json!({ "claudeAiOauth": {
        "accessToken": "OLD-ACCESS", "refreshToken": "OLD-REFRESH", "expiresAt": NOW - 1,
    }});
    std::fs::write(&path, expired.to_string()).unwrap();

    // ONE scripted refresh response: a second refresh request would fail the test.
    let scripted = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            json!({ "access_token": "NEW-ACCESS", "expires_in": 3600, "refresh_token": "NEW-REFRESH" })
                .to_string()
                .into_bytes(),
        ],
        end: BodyEnd::Eof,
    }]);
    let entered = Arc::new(Notify::new());
    let gate = Arc::new(Notify::new());
    let transport = Arc::new(GatedTransport {
        inner: scripted.clone(),
        entered: entered.clone(),
        gate: gate.clone(),
    });
    // Two independent sources on the same file, as two agents have them.
    let first = ClaudeCodeCredentials::at(path.clone(), transport.clone()).with_clock(now);
    let second = ClaudeCodeCredentials::at(path.clone(), transport.clone()).with_clock(now);

    let driver = async {
        // The first source is inside its refresh request, holding the lock.
        entered.notified().await;
        // Let the second one run into the lock and wait there for a while.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        gate.notify_one();
    };
    let (a, b, ()) = tokio::join!(first.access(), second.access(), driver);

    assert_eq!(a.unwrap().bearer, "NEW-ACCESS");
    // The second re-read the file under the lock and used the rotated token.
    assert_eq!(b.unwrap().bearer, "NEW-ACCESS");
    assert_eq!(scripted.requests().len(), 1, "exactly one rotation");
}

/// Accepts the FIRST request and never answers it; every later request goes to
/// the scripted transport.
struct StallsFirstTransport {
    inner: ScriptedTransport,
    posts: AtomicUsize,
    entered: Arc<Notify>,
}

impl Transport for StallsFirstTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            if self.posts.fetch_add(1, Ordering::SeqCst) == 0 {
                self.entered.notify_one();
                std::future::pending::<()>().await;
            }
            self.inner.post(request).await
        })
    }
}

/// Issue #164: the holder's refresh gets no answer. It ends at the first-byte
/// bound and releases the lock, so a peer that queued behind it (and would give up
/// after `LOCK_PATIENCE`) gets its turn and does its own refresh.
#[tokio::test(start_paused = true)]
async fn a_peer_gets_the_lock_after_the_holders_refresh_times_out() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join(".credentials.json");
    let expired = json!({ "claudeAiOauth": {
        "accessToken": "OLD-ACCESS", "refreshToken": "OLD-REFRESH", "expiresAt": NOW - 1,
    }});
    std::fs::write(&path, expired.to_string()).unwrap();

    // ONE scripted response: the peer's refresh. The holder's never gets one.
    let scripted = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            json!({ "access_token": "NEW-ACCESS", "expires_in": 3600, "refresh_token": "NEW-REFRESH" })
                .to_string()
                .into_bytes(),
        ],
        end: BodyEnd::Eof,
    }]);
    let entered = Arc::new(Notify::new());
    let transport = Arc::new(StallsFirstTransport {
        inner: scripted.clone(),
        posts: AtomicUsize::new(0),
        entered: entered.clone(),
    });
    let holder = ClaudeCodeCredentials::at(path.clone(), transport.clone()).with_clock(now);
    let peer = ClaudeCodeCredentials::at(path.clone(), transport.clone()).with_clock(now);

    let peer_run = async {
        // The holder is inside its refresh request, holding the lock.
        entered.notified().await;
        tokio::time::advance(Duration::from_secs(1)).await;
        peer.access().await
    };
    // Without the bound the holder never returns: the guard turns that into a failure.
    let (held, queued) = tokio::time::timeout(FIRST_BYTE_TIMEOUT + LOCK_PATIENCE, async {
        tokio::join!(holder.access(), peer_run)
    })
    .await
    .expect("the holder's refresh never ended");

    let error = held.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    assert_eq!(
        error.message,
        format!(
            "token refresh got no response within {} s",
            FIRST_BYTE_TIMEOUT.as_secs()
        )
    );
    assert_eq!(queued.unwrap().bearer, "NEW-ACCESS");
    assert_eq!(transport.posts.load(Ordering::SeqCst), 2);
    assert_eq!(scripted.requests().len(), 1, "the peer's own refresh");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("NEW-ACCESS")
    );
}
