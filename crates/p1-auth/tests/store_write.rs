//! Must-pass of spec §6: the store's WRITE side — `p1 login` and `p1 logout`.
//!
//! Every path is a scratch home: no test in this crate reaches the real one
//! (`tests/no_real_environment.rs` asserts that for the whole suite), and every key
//! is obviously fake.

mod support;

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;

use p1_auth::store::{check_writable, put_api_key, remove};
use p1_auth::{CredentialSpec, describe, resolve};
use p1_provider_http::testing::ScriptedTransport;
use support::{FakeEnv, Scratch, login};

const ROUTE: &str = "test-route";
const OTHER: &str = "other-route";
const STORE: &str = ".config/p1/auth.json";
const OPENCODE: &str = ".local/share/opencode/auth.json";

/// An API-key route: the variable, then p1's store, then a borrowed login.
fn api_key() -> CredentialSpec {
    serde_json::from_str(
        r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY","borrow":["opencode:opencode-go"]}"#,
    )
    .unwrap()
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The store document, parsed, so a test asserts about entries and not about layout.
fn document(scratch: &Scratch) -> serde_json::Value {
    serde_json::from_str(&scratch.read(STORE)).unwrap()
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

#[tokio::test]
async fn a_fresh_home_gets_a_0700_directory_and_a_0600_file_with_one_entry() {
    let scratch = Scratch::new();

    put_api_key(ROUTE, "FAKE-KEY", &scratch.locations())
        .await
        .unwrap();

    assert_eq!(mode(&scratch.path(".config/p1")), 0o700);
    assert_eq!(mode(&scratch.path(STORE)), 0o600);
    assert_eq!(
        document(&scratch),
        serde_json::json!({ ROUTE: { "type": "api_key", "key": "FAKE-KEY" } }),
        "exactly one entry, in the documented shape"
    );
}

#[tokio::test]
async fn a_second_route_keeps_the_first_and_re_login_replaces_only_that_entry() {
    let scratch = Scratch::new();
    let locations = scratch.locations();
    put_api_key(ROUTE, "FAKE-FIRST", &locations).await.unwrap();
    put_api_key(OTHER, "FAKE-OTHER", &locations).await.unwrap();

    assert_eq!(document(&scratch)[ROUTE]["key"], "FAKE-FIRST");
    assert_eq!(document(&scratch)[OTHER]["key"], "FAKE-OTHER");

    put_api_key(ROUTE, "FAKE-SECOND", &locations).await.unwrap();

    let document = document(&scratch);
    assert_eq!(document[ROUTE]["key"], "FAKE-SECOND");
    assert_eq!(
        document[OTHER]["key"], "FAKE-OTHER",
        "re-login touches one entry"
    );
}

/// An entry this crate does not understand — another tool's extra field — survives a
/// write for a DIFFERENT route untouched, byte for byte in meaning.
#[tokio::test]
async fn another_entry_keeps_its_unknown_fields() {
    let scratch = Scratch::new();
    scratch.write(
        STORE,
        &support::document(&serde_json::json!({
            OTHER: { "type": "api_key", "key": "FAKE-OTHER", "comment": "keep me" },
        })),
    );

    put_api_key(ROUTE, "FAKE-KEY", &scratch.locations())
        .await
        .unwrap();

    let document = document(&scratch);
    assert_eq!(document[OTHER]["comment"], "keep me");
    assert_eq!(document[OTHER]["key"], "FAKE-OTHER");
    assert_eq!(document[ROUTE]["key"], "FAKE-KEY");
}

#[tokio::test]
async fn logout_removes_only_that_route_and_an_empty_object_stays_a_valid_file() {
    let scratch = Scratch::new();
    let locations = scratch.locations();
    put_api_key(ROUTE, "FAKE-KEY", &locations).await.unwrap();
    put_api_key(OTHER, "FAKE-OTHER", &locations).await.unwrap();

    assert!(remove(ROUTE, &locations).await.unwrap());
    assert_eq!(
        document(&scratch),
        serde_json::json!({ OTHER: { "type": "api_key", "key": "FAKE-OTHER" } })
    );

    assert!(remove(OTHER, &locations).await.unwrap());
    assert_eq!(
        scratch.read(STORE),
        "{}\n",
        "an empty object stays a valid file"
    );
    assert_eq!(mode(&scratch.path(STORE)), 0o600);
}

#[tokio::test]
async fn logout_of_a_missing_entry_is_reported_not_an_error_and_writes_nothing() {
    let scratch = Scratch::new();
    let locations = scratch.locations();
    put_api_key(ROUTE, "FAKE-KEY", &locations).await.unwrap();
    let before = scratch.read(STORE);

    assert!(
        !remove(OTHER, &locations).await.unwrap(),
        "nothing to remove"
    );
    assert_eq!(scratch.read(STORE), before, "the file is left alone");

    // A home with no store at all: no entry, no error, and nothing created.
    let fresh = Scratch::new();
    assert!(!remove(ROUTE, &fresh.locations()).await.unwrap());
    assert!(
        !fresh.path(".config").exists(),
        "logout creates no store for a route that has none"
    );
}

#[tokio::test]
async fn a_group_or_world_accessible_store_is_refused_with_the_chmod_message() {
    let scratch = Scratch::new();
    let locations = scratch.locations();
    put_api_key(ROUTE, "FAKE-KEY", &locations).await.unwrap();

    scratch.set_mode(STORE, 0o644);
    let error = put_api_key(ROUTE, "FAKE-SECOND", &locations)
        .await
        .unwrap_err();
    assert!(error.contains("chmod 600"), "{error}");
    assert_eq!(document(&scratch)[ROUTE]["key"], "FAKE-KEY");
    assert!(
        check_writable(&locations).is_err(),
        "refused before a key is read"
    );

    scratch.set_mode(STORE, 0o600);
    scratch.set_mode(".config/p1", 0o755);
    let error = put_api_key(ROUTE, "FAKE-SECOND", &locations)
        .await
        .unwrap_err();
    assert!(error.contains("chmod 700"), "{error}");
    assert!(
        remove(ROUTE, &locations).await.is_err(),
        "logout refuses a wide store too"
    );
    assert_eq!(document(&scratch)[ROUTE]["key"], "FAKE-KEY");
}

/// A file this crate could not preserve is not overwritten: a login that would
/// destroy another tool's data fails instead.
#[tokio::test]
async fn a_malformed_store_is_refused() {
    let scratch = Scratch::new();
    scratch.write(STORE, "not json");

    let error = put_api_key(ROUTE, "FAKE-KEY", &scratch.locations())
        .await
        .unwrap_err();

    assert!(error.contains("malformed"), "{error}");
    assert_eq!(scratch.read(STORE), "not json");
}

#[tokio::test]
async fn an_unusable_key_is_refused_and_nothing_is_written() {
    let scratch = Scratch::new();
    let locations = scratch.locations();

    for key in ["", "FAKE KEY", "FAKE\tKEY", "FAKE-ÜNICODE", "FAKE\nKEY"] {
        let error = put_api_key(ROUTE, key, &locations).await.unwrap_err();
        assert!(
            error.contains("nothing was written"),
            "the error says nothing was written: {error}"
        );
        assert!(
            !error.contains(key) || key.is_empty(),
            "no value in {error}"
        );
        assert!(
            !scratch.path(STORE).exists(),
            "an unusable key writes nothing"
        );
    }
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

/// Must-pass of §6: two logins for two routes, at the same time, on the same file.
/// Both land — the second waits for the first without blocking the only thread.
#[tokio::test(start_paused = true)]
async fn concurrent_logins_for_two_routes_both_land() {
    let scratch = Scratch::new();
    let locations = scratch.locations();

    let first = {
        let locations = locations.clone();
        tokio::spawn(async move { put_api_key(ROUTE, "FAKE-FIRST", &locations).await })
    };
    let second = {
        let locations = locations.clone();
        tokio::spawn(async move { put_api_key(OTHER, "FAKE-SECOND", &locations).await })
    };
    let (first, second) = tokio::join!(first, second);
    first.unwrap().unwrap();
    second.unwrap().unwrap();

    let document = document(&scratch);
    assert_eq!(document[ROUTE]["key"], "FAKE-FIRST");
    assert_eq!(document[OTHER]["key"], "FAKE-SECOND");
}
