//! The `kind = "none"` credential (issue #134): a route that sends NO credential
//! because an egress proxy injects the provider's credential after the request
//! leaves the process.
//!
//! The claim is a NEGATIVE one, so every test here builds the sources a chain would
//! otherwise reach — a p1 store entry, a borrowed OpenCode login, a Claude Code
//! login — and proves none of them is read. The store file is written 0644 on
//! purpose: any read of it is REFUSED with the chmod message, so a chain that
//! touched it would fail instead of staying quiet. Nothing here reads a real file or
//! the real environment.

mod support;

use std::sync::Arc;

use p1_auth::{
    CredentialKind, CredentialPolicy, CredentialSpec, Locations, Presence, describe, resolve,
};
use p1_contracts::ProviderErrorKind;
use p1_provider_http::CredentialSource;
use p1_provider_http::testing::ScriptedTransport;
use support::{FakeEnv, NEVER_EXPIRES_MS, Scratch, claude_login, login};

const ROUTE: &str = "proxy-route";
const SENTINEL: &str = "FAKE-PROXY-SENTINEL-9c1f";
const STORE: &str = ".config/p1/auth.json";
const OPENCODE: &str = ".local/share/opencode/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).expect("the `[credential]` table parses")
}

fn none() -> CredentialSpec {
    spec(r#"{"kind":"none"}"#)
}

/// The source a route file's table resolves to, over `locations`.
fn source(spec: &CredentialSpec, locations: &Locations) -> Arc<dyn CredentialSource> {
    resolve(
        ROUTE,
        spec,
        Arc::new(ScriptedTransport::new(Vec::new())),
        locations,
    )
}

/// A home holding the three sources a chain could reach: a p1 store entry, a
/// borrowed OpenCode login and a Claude Code login. Every value is a sentinel.
fn home_with_every_source(scratch: &Scratch) -> String {
    let store = serde_json::json!({
        ROUTE: { "type": "api_key", "key": SENTINEL },
    })
    .to_string();
    scratch.write(STORE, &store);
    // 0644: a store read is refused with the chmod message, never silently used.
    scratch.set_mode(STORE, 0o644);
    scratch.write(OPENCODE, &login("opencode", "opencode-go", SENTINEL));
    scratch.write(CLAUDE, &claude_login(SENTINEL, SENTINEL, NEVER_EXPIRES_MS));
    store
}

// ------------------------------------------------------------------ the table

#[test]
fn a_none_route_parses_and_reads_no_variable() {
    let parsed = none();
    assert_eq!(parsed.kind, CredentialKind::None);
    assert_eq!(parsed.kind.name(), "none");
    assert_eq!(parsed.kind.label(), "none (proxy-injected)");
    assert!(parsed.env.is_none());
    assert!(parsed.borrow.is_empty());
    assert!(!parsed.store_only);
    assert!(parsed.validate().is_ok());
}

#[test]
fn a_none_route_may_not_name_a_credential_source() {
    for (json, field) in [
        (r#"{"kind":"none","env":"ZAI_API_KEY"}"#, "env"),
        (r#"{"kind":"none","borrow":["pi:zai"]}"#, "borrow"),
        (r#"{"kind":"none","store_only":true}"#, "store_only"),
    ] {
        let error = spec(json).validate().unwrap_err();
        assert!(
            error.contains("none") && error.contains(field),
            "{json}: {error}"
        );
    }
}

// ------------------------------------------------------- the report and the line

#[test]
fn a_none_route_is_visibly_proxy_injected_and_has_no_source() {
    let scratch = Scratch::new();
    let env = FakeEnv::new();
    // Even a variable that names a key changes nothing: this route reads none.
    env.set("ZAI_API_KEY", SENTINEL);
    let locations = env.locations(&scratch);

    let report = describe(ROUTE, &none(), &locations);
    assert_eq!(report.policy, CredentialPolicy::ProxyInjected);
    assert_eq!(report.policy.name(), "proxy-injected");
    assert!(
        report.tried.is_empty(),
        "no source is in the chain: {:?}",
        report.tried
    );
    assert_eq!(report.chosen, None);

    let line = report.line();
    assert!(line.contains("none (proxy-injected)"), "{line}");
    assert!(line.contains("egress proxy"), "{line}");
    assert!(!line.contains(SENTINEL), "the line leaked a value: {line}");
}

// ------------------------------------------------------------ the resolved source

#[tokio::test]
async fn a_none_route_hands_the_driver_an_empty_placeholder_and_refuses_to_refresh() {
    let scratch = Scratch::new();
    let locations = scratch.locations();
    let source = source(&none(), &locations);

    assert!(source.proxy_injected(), "adapters must send no credential");
    let credential = source.access().await.expect("there is a placeholder");
    assert!(
        credential.bearer.is_empty(),
        "p1 has no credential to send on this route"
    );
    assert_eq!(credential.account_id, None);

    let error = source.refresh(&credential).await.unwrap_err();
    assert_eq!(error.kind, ProviderErrorKind::Authentication);
    for part in ["proxy credential", "kind = \"none\"", ROUTE] {
        assert!(error.message.contains(part), "{}: {part}", error.message);
    }
    assert!(
        !error.message.contains(SENTINEL),
        "the refusal leaked a value: {}",
        error.message
    );
}

#[tokio::test]
async fn a_none_route_never_reads_the_store_or_a_login() {
    let scratch = Scratch::new();
    let store = home_with_every_source(&scratch);
    let locations = scratch.locations();
    let source = source(&none(), &locations);

    // The store is 0644, so a chain that READ it would fail with the chmod message;
    // a chain that read the OpenCode or Claude login would hand back the sentinel.
    let credential = source
        .access()
        .await
        .expect("nothing is read, nothing fails");
    assert!(credential.bearer.is_empty(), "{}", credential.bearer);
    assert_eq!(credential.account_id, None);

    let report = describe(ROUTE, &none(), &locations);
    assert!(report.tried.is_empty(), "{:?}", report.tried);
    assert_eq!(report.chosen, None);

    // Neither the refusal nor the report names a source it did not read.
    let error = source.refresh(&credential).await.unwrap_err();
    assert!(!error.message.contains("p1 store"), "{}", error.message);
    assert!(!error.message.contains("login"), "{}", error.message);

    // And nothing was written back or touched.
    assert_eq!(scratch.read(STORE), store);
    assert_eq!(
        scratch.read(OPENCODE),
        login("opencode", "opencode-go", SENTINEL)
    );
}

/// The contrast that makes the negative claims above mean something: the very same
/// home answers an API-key route — through the STORE, whose wide mode is exactly the
/// refusal a `none` route never sees.
#[tokio::test]
async fn the_same_home_still_answers_an_api_key_route() {
    let scratch = Scratch::new();
    home_with_every_source(&scratch);
    // Narrow the store again so the api-key chain can read it.
    scratch.set_mode(STORE, 0o600);
    let locations = scratch.locations();

    let api_key = spec(r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY"}"#);
    let source = source(&api_key, &locations);
    assert!(!source.proxy_injected());
    assert_eq!(source.access().await.unwrap().bearer, SENTINEL);

    let report = describe(ROUTE, &api_key, &locations);
    assert_eq!(report.policy, CredentialPolicy::Chain);
    assert_eq!(report.chosen, Some(p1_auth::SourceName::P1Store));
    assert_eq!(
        report.tried,
        vec![
            (
                p1_auth::SourceName::Env("P1_AUTH_TEST_KEY".into()),
                Presence::Absent
            ),
            (p1_auth::SourceName::P1Store, Presence::Present),
        ]
    );
}
