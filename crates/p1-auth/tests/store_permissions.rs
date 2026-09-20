//! Must-pass (d) of the spec: a store file or directory that anyone but the owner
//! can read is REFUSED — with the `chmod` to run — never quietly used.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::sync::Arc;

use p1_auth::{CredentialSpec, resolve};
use p1_provider_http::testing::ScriptedTransport;
use support::{FakeEnv, Scratch};

const STORE: &str = ".config/p1/auth.json";

fn api_key() -> CredentialSpec {
    serde_json::from_str(r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY"}"#).unwrap()
}

/// The store entry this route needs, so only the MODE can be the reason to refuse.
fn entry() -> String {
    serde_json::json!({ "test-route": { "type": "api_key", "key": "FAKE-KEY" } }).to_string()
}

/// The chain over this scratch home, with no variable set: the store is reached.
async fn access(scratch: &Scratch) -> Result<String, String> {
    let env = FakeEnv::new();
    let locations = env.locations(scratch);
    resolve(
        "test-route",
        &api_key(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        &locations,
    )
    .access()
    .await
    .map(|credential| credential.bearer)
    .map_err(|error| error.message)
}

#[tokio::test]
async fn a_group_readable_store_file_is_refused_with_the_chmod_message() {
    let scratch = Scratch::new();
    scratch.write(STORE, &entry());
    scratch.set_mode(STORE, 0o644);

    let message = access(&scratch).await.unwrap_err();
    assert!(message.contains("chmod 600"), "{message}");
    assert!(
        message.contains("auth.json"),
        "the message names the file: {message}"
    );
    assert!(!message.contains("FAKE-KEY"), "no value leaks: {message}");
}

#[tokio::test]
async fn a_world_accessible_store_directory_is_refused_with_the_chmod_message() {
    let scratch = Scratch::new();
    scratch.write(STORE, &entry());
    scratch.set_mode(".config/p1", 0o755);

    let message = access(&scratch).await.unwrap_err();
    assert!(message.contains("chmod 700"), "{message}");
    assert!(!message.contains("FAKE-KEY"), "no value leaks: {message}");
}

#[tokio::test]
async fn a_0600_store_file_in_a_0700_directory_is_used() {
    let scratch = Scratch::new();
    scratch.write(STORE, &entry());

    assert_eq!(access(&scratch).await.unwrap(), "FAKE-KEY");
    assert_eq!(
        std::fs::metadata(scratch.path(STORE))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600,
        "reading never loosens the mode"
    );
}
