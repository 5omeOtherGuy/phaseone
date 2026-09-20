//! Must-pass (c) of the spec: a refresh is written back to the source the
//! credential came from, and nothing else in that file changes.

mod support;

use std::sync::Arc;

use p1_auth::{CredentialSpec, Locations, resolve};
use p1_provider_http::CredentialSource;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use support::{LONG_EXPIRED_MS, Scratch, claude_login};

const ROUTE: &str = "claude-subscription";
const STORE: &str = ".config/p1/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";

fn claude_oauth() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"claude-code-oauth"}"#).unwrap()
}

/// One scripted token-refresh response.
fn refreshed(access: &str, refresh: &str) -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            serde_json::json!({
                "access_token": access,
                "refresh_token": refresh,
                "expires_in": 3600,
                "scope": "user:inference",
            })
            .to_string()
            .into_bytes(),
        ],
        end: BodyEnd::Eof,
    }
}

fn source(
    spec: &CredentialSpec,
    locations: &Locations,
    transport: ScriptedTransport,
) -> Arc<dyn CredentialSource> {
    resolve(ROUTE, spec, Arc::new(transport), locations)
}

/// The store document this test writes: the route's expired oauth entry, plus an
/// entry of ANOTHER route that a refresh must leave byte for byte alone.
fn store(access: &str) -> String {
    support::document(&serde_json::json!({
        ROUTE: {
            "type": "oauth",
            "access": access,
            "refresh": "FAKE-OLD-REFRESH",
            "expires": LONG_EXPIRED_MS,
            "account_id": "FAKE-ACCOUNT",
        },
        "zz-other-route": { "type": "api_key", "key": "FAKE-OTHER" },
    }))
}

#[tokio::test]
async fn a_refreshed_borrowed_token_lands_in_the_file_it_came_from() {
    let scratch = Scratch::new();
    // The document's own shape: the field p1 does not touch sorts last, so its
    // exact text can be compared after the write.
    let document = support::document(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": "FAKE-OLD-ACCESS",
            "refreshToken": "FAKE-OLD-REFRESH",
            "expiresAt": LONG_EXPIRED_MS,
            "subscriptionType": "max",
        },
        "zz-root-extra": { "kept": true },
    }));
    scratch.write(CLAUDE, &document);
    let untouched = scratch.tail_from(CLAUDE, "\"zz-root-extra\"");

    let transport = ScriptedTransport::new(vec![refreshed("FAKE-NEW-ACCESS", "FAKE-NEW-REFRESH")]);
    let credential = source(&claude_oauth(), &scratch.locations(), transport.clone())
        .access()
        .await
        .unwrap();

    assert_eq!(credential.bearer, "FAKE-NEW-ACCESS");
    assert_eq!(transport.requests().len(), 1, "exactly one rotation");
    let written = scratch.read(CLAUDE);
    assert!(written.contains("FAKE-NEW-ACCESS"), "{written}");
    assert!(written.contains("FAKE-NEW-REFRESH"), "{written}");
    assert!(
        written.ends_with(&untouched),
        "nothing after the refreshed fields may change:\n{written}"
    );
}

#[tokio::test]
async fn a_refreshed_p1_store_entry_lands_in_the_p1_store_and_never_another_source() {
    let scratch = Scratch::new();
    scratch.write(STORE, &store("FAKE-OLD-ACCESS"));
    let untouched = scratch.tail_from(STORE, "\"zz-other-route\"");
    // A borrowed login is present and older: the write must NOT land there.
    let login_before = claude_login("FAKE-CLAUDE-ACCESS", "FAKE-CLAUDE-REFRESH", LONG_EXPIRED_MS);
    scratch.write(CLAUDE, &login_before);

    let transport = ScriptedTransport::new(vec![refreshed("FAKE-STORE-NEW", "FAKE-STORE-REFRESH")]);
    let credential = source(&claude_oauth(), &scratch.locations(), transport)
        .access()
        .await
        .unwrap();

    assert_eq!(credential.bearer, "FAKE-STORE-NEW");
    let written = scratch.read(STORE);
    assert!(written.contains("FAKE-STORE-NEW"), "{written}");
    assert!(written.contains("FAKE-STORE-REFRESH"), "{written}");
    assert!(
        written.contains("FAKE-ACCOUNT"),
        "account_id is kept: {written}"
    );
    assert!(
        written.ends_with(&untouched),
        "another route's entry must not change:\n{written}"
    );
    assert_eq!(
        scratch.read(CLAUDE),
        login_before,
        "the refresh must not be written to a different source"
    );
}

#[tokio::test]
async fn a_rejected_store_token_is_rotated_in_the_store() {
    let scratch = Scratch::new();
    scratch.write(STORE, &store("FAKE-REJECTED-ACCESS"));

    let transport = ScriptedTransport::new(vec![refreshed("FAKE-ROTATED", "FAKE-ROTATED-REFRESH")]);
    let source = source(&claude_oauth(), &scratch.locations(), transport);
    let rejected = p1_provider_http::Credential {
        bearer: "FAKE-REJECTED-ACCESS".to_string(),
        account_id: Some("FAKE-ACCOUNT".to_string()),
    };

    let credential = source.refresh(&rejected).await.unwrap();
    assert_eq!(credential.bearer, "FAKE-ROTATED");
    assert!(scratch.read(STORE).contains("FAKE-ROTATED-REFRESH"));
}
