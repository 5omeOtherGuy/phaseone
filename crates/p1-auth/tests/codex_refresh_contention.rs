//! Review finding R2: two agents on the same route refresh at the same time, on a
//! current-thread runtime. The first holds the auth-file lock across its refresh
//! request; the second must WAIT for it without blocking the only thread — or the
//! first request can never complete and release the lock.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use p1_auth::CodexCliCredentials;
use p1_contracts::{BoxFuture, ProviderErrorKind};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{
    CredentialSource, FIRST_BYTE_TIMEOUT, HttpRequest, HttpResponse, LOCK_PATIENCE, Transport,
    TransportError,
};
use serde_json::json;
use tokio::sync::Notify;

fn base64url_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
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

/// A fake unsigned token expiring `offset` seconds from now.
fn jwt_expiring_in(offset: i64) -> String {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64;
    let payload = json!({ "exp": now + offset }).to_string();
    format!(
        "{}.{}.signature",
        base64url_encode(br#"{"alg":"none"}"#),
        base64url_encode(payload.as_bytes())
    )
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
    let path = dir.path().join("auth.json");
    let document = json!({ "tokens": {
        "id_token": "old-id", "access_token": jwt_expiring_in(-10),
        "refresh_token": "refresh-old", "account_id": "acct_1",
    }});
    std::fs::write(&path, document.to_string()).unwrap();

    let fresh = jwt_expiring_in(3600);
    // ONE scripted refresh response: a second refresh request would fail the test.
    let scripted = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        chunks: vec![
            json!({ "access_token": fresh, "refresh_token": "refresh-new", "id_token": "new-id" })
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
    let first = CodexCliCredentials::at(path.clone(), transport.clone());
    let second = CodexCliCredentials::at(path.clone(), transport.clone());

    let driver = async {
        // The first source is inside its refresh request, holding the lock.
        entered.notified().await;
        // Let the second one run into the lock and wait there for a while.
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
        gate.notify_one();
    };
    let (a, b, ()) = tokio::join!(first.access(), second.access(), driver);

    assert_eq!(a.unwrap().bearer, fresh);
    // The second re-read the file under the lock and used the rotated token.
    assert_eq!(b.unwrap().bearer, fresh);
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
    let path = dir.path().join("auth.json");
    let document = json!({ "tokens": {
        "id_token": "old-id", "access_token": jwt_expiring_in(-10),
        "refresh_token": "refresh-old", "account_id": "acct_1",
    }});
    std::fs::write(&path, document.to_string()).unwrap();

    let fresh = jwt_expiring_in(3600);

    // ONE scripted response: the peer's refresh. The holder's never gets one.
    let scripted = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        chunks: vec![
            json!({ "access_token": fresh, "refresh_token": "refresh-new", "id_token": "new-id" })
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
    let holder = CodexCliCredentials::at(path.clone(), transport.clone());
    let peer = CodexCliCredentials::at(path.clone(), transport.clone());

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
    assert_eq!(queued.unwrap().bearer, fresh);
    assert_eq!(transport.posts.load(Ordering::SeqCst), 2);
    assert_eq!(scripted.requests().len(), 1, "the peer's own refresh");
    assert!(
        std::fs::read_to_string(&path)
            .unwrap()
            .contains("refresh-new")
    );
}
