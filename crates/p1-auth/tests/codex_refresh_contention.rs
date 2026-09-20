//! Review finding R2: two agents on the same route refresh at the same time, on a
//! current-thread runtime. The first holds the auth-file lock across its refresh
//! request; the second must WAIT for it without blocking the only thread — or the
//! first request can never complete and release the lock.

use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use p1_auth::CodexCliCredentials;
use p1_contracts::BoxFuture;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{CredentialSource, HttpRequest, HttpResponse, Transport, TransportError};
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
