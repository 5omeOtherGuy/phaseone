//! p1's store written by `p1 login` (spec §6) in the chain with a borrowed keyed login: the
//! stored key wins over the borrowed one, and the documented variable over both.

mod support;

use std::sync::Arc;

use p1_auth::store::{put_api_key, remove};
use p1_auth::{CredentialSpec, describe, resolve};
use p1_provider_http::testing::ScriptedTransport;
use support::{FakeEnv, Scratch, login};

const ROUTE: &str = "test-route";
const OPENCODE: &str = ".local/share/opencode/auth.json";

/// An API-key route: the variable, then p1's store, then a borrowed login.
fn api_key() -> CredentialSpec {
    serde_json::from_str(
        r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY","borrow":["opencode:opencode-go"]}"#,
    )
    .unwrap()
}

/// The key the chain resolves for this route over this scratch home.
async fn bearer(scratch: &Scratch, env: &FakeEnv) -> String {
    resolve(
        ROUTE,
        &api_key(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        &env.locations(scratch),
    )
    .access()
    .await
    .expect("the chain resolves")
    .bearer
}

/// Must-pass of §6: `resolve` for the route yields the stored key, and a documented
/// environment variable still wins over it.
#[tokio::test]
async fn resolve_yields_the_stored_key_and_an_environment_variable_still_wins() {
    let scratch = Scratch::new();
    let env = FakeEnv::new();
    scratch.write(OPENCODE, &login("opencode", "opencode-go", "FAKE-OPENCODE"));

    // Nothing stored yet: the borrowed login answers.
    assert_eq!(bearer(&scratch, &env).await, "FAKE-OPENCODE");

    put_api_key(ROUTE, "FAKE-STORED", &scratch.locations())
        .await
        .unwrap();
    assert_eq!(bearer(&scratch, &env).await, "FAKE-STORED");
    assert_eq!(
        describe(ROUTE, &api_key(), &env.locations(&scratch)).line(),
        "p1 store"
    );

    env.set("P1_AUTH_TEST_KEY", "FAKE-ENV");
    assert_eq!(bearer(&scratch, &env).await, "FAKE-ENV");
    assert_eq!(
        describe(ROUTE, &api_key(), &env.locations(&scratch)).line(),
        "env P1_AUTH_TEST_KEY",
        "the report says the variable overrides the store"
    );

    // After logout the chain falls through to the borrowed login again.
    assert!(remove(ROUTE, &scratch.locations()).await.unwrap());
    env.clear("P1_AUTH_TEST_KEY");
    assert_eq!(bearer(&scratch, &env).await, "FAKE-OPENCODE");
}
