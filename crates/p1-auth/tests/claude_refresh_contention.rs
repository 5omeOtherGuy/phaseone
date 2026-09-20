//! Review finding R2: two agents on the same route refresh at the same time, on a
//! current-thread runtime. The first holds the credentials lock across its refresh
//! request; the second must WAIT for it without blocking the only thread — or the
//! first request can never complete and release the lock.

use std::sync::Arc;

use p1_auth::ClaudeCodeCredentials;
use p1_contracts::BoxFuture;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{CredentialSource, HttpRequest, HttpResponse, Transport, TransportError};
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
