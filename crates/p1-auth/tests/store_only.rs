//! The self-contained credential policy (ADR-0061, spec §2): a route whose
//! `[credential]` table writes `store_only = true` reads only its documented
//! environment variable and p1's own store. The must-pass claim is a NEGATIVE one:
//! after an absent or rejected p1 entry, no other tool's login file is opened.
//!
//! No test here reads a real file. Every path is inside a scratch home, and the
//! CLI's credential path is deliberately made unreadable (a directory), so a chain
//! that DID read it would report it `unusable` instead of staying quiet.

mod support;

use std::sync::Arc;

use p1_auth::{
    CredentialPolicy, CredentialSpec, Locations, Presence, SourceName, describe, resolve,
};
use p1_provider_http::CredentialSource;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use support::{FakeEnv, LONG_EXPIRED_MS, Scratch};

const ROUTE: &str = "test-route";
const STORE: &str = ".config/p1/auth.json";
const CLAUDE: &str = ".claude/.credentials.json";
const CODEX: &str = ".codex/auth.json";
const SENTINEL: &str = "FAKE-PRIVATE-9c1f";

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).expect("the `[credential]` table parses")
}

fn claude_store_only() -> CredentialSpec {
    spec(r#"{"kind":"claude-code-oauth","store_only":true}"#)
}

fn codex_store_only() -> CredentialSpec {
    spec(r#"{"kind":"codex-oauth","store_only":true}"#)
}

/// Put a DIRECTORY where each CLI's credential FILE belongs: every attempt to read
/// it as a file fails, so a source that is in the chain would be reported `unusable`
/// rather than skipped. A store-only chain must never mention either path.
fn block_external_reads(scratch: &Scratch) {
    std::fs::create_dir_all(scratch.path(CLAUDE)).unwrap();
    std::fs::create_dir_all(scratch.path(CODEX)).unwrap();
}

/// The chain over `locations`, with a scripted transport that has no responses: a
/// source that tries to refresh fails loudly instead of silently passing.
fn source(spec: &CredentialSpec, locations: &Locations) -> Arc<dyn CredentialSource> {
    resolve(
        ROUTE,
        spec,
        Arc::new(ScriptedTransport::new(Vec::new())),
        locations,
    )
}

/// A store document with one expired oauth entry whose refresh the provider refuses.
fn expired_store_oauth() -> String {
    serde_json::json!({
        ROUTE: {
            "type": "oauth",
            "access": "FAKE-ACCESS",
            "refresh": "FAKE-REFRESH",
            "expires": LONG_EXPIRED_MS,
            "account_id": "FAKE-ACCOUNT",
        }
    })
    .to_string()
}

fn refusing_transport() -> ScriptedTransport {
    ScriptedTransport::new(vec![ScriptedResponse {
        status: 401,
        headers: Vec::new(),
        chunks: vec![b"denied".to_vec()],
        end: BodyEnd::Eof,
    }])
}

// ------------------------------------------------------------------ the table

#[test]
fn store_only_parses_and_conflicts_with_a_nonempty_borrow() {
    let parsed = claude_store_only();
    assert!(parsed.store_only);
    assert!(parsed.validate().is_ok());

    // An api-key route may be store-only too: it is the same policy as `borrow = []`.
    let api = spec(r#"{"kind":"api-key","env":"A_KEY","borrow":[],"store_only":true}"#);
    assert!(api.store_only);
    assert!(api.validate().is_ok());

    // The two spellings contradict each other, so the table is a load error.
    let conflict =
        spec(r#"{"kind":"api-key","env":"A_KEY","borrow":["pi:zai"],"store_only":true}"#);
    let error = conflict.validate().unwrap_err();
    assert!(
        error.contains("store_only") && error.contains("borrow"),
        "{error}"
    );
}

#[test]
fn a_misspelled_key_is_still_rejected() {
    let error =
        serde_json::from_str::<CredentialSpec>(r#"{"kind":"codex-oauth","storeOnly":true}"#)
            .unwrap_err()
            .to_string();
    assert!(error.contains("storeOnly"), "{error}");
}

// ------------------------------------------------------------ the visible policy

#[test]
fn a_store_only_route_names_only_the_store() {
    let scratch = Scratch::new();
    block_external_reads(&scratch);
    let env = FakeEnv::new();

    let report = describe(ROUTE, &claude_store_only(), &env.locations(&scratch));

    assert_eq!(report.policy, CredentialPolicy::StoreOnly);
    assert_eq!(report.policy.name(), "store-only");
    assert_eq!(report.chosen, None);
    assert_eq!(
        report.tried,
        vec![(SourceName::P1Store, Presence::Absent)],
        "the CLI's login is not in a store-only chain at all"
    );
    assert!(
        report.line().contains("[p1 store only]"),
        "{}",
        report.line()
    );
    assert!(
        !report.line().contains("Claude Code") && !report.line().contains("Codex"),
        "a store-only line never names a CLI login: {}",
        report.line()
    );
    assert!(report.line().contains("add an entry to the p1 store"));
}

/// The contrast that makes the first test meaningful: without the field, the very
/// same unreadable path IS in the chain and is reported as an unusable entry.
#[test]
fn without_the_field_the_chain_still_reaches_the_cli_login() {
    let scratch = Scratch::new();
    block_external_reads(&scratch);
    let env = FakeEnv::new();

    let report = describe(
        ROUTE,
        &spec(r#"{"kind":"claude-code-oauth"}"#),
        &env.locations(&scratch),
    );

    assert_eq!(report.policy, CredentialPolicy::Chain);
    assert_eq!(report.policy.marker(), "");
    assert!(
        report
            .tried
            .iter()
            .any(|(name, _)| *name == SourceName::ClaudeCodeLogin),
        "{:?}",
        report.tried
    );
    assert!(
        report
            .tried
            .iter()
            .any(|(_, presence)| matches!(presence, Presence::Unusable(_))),
        "the legacy chain reads the CLI login: {:?}",
        report.tried
    );
}

#[test]
fn a_store_only_route_still_takes_its_documented_variable() {
    let scratch = Scratch::new();
    block_external_reads(&scratch);
    let env = FakeEnv::new();
    env.set("CLAUDE_CODE_OAUTH_TOKEN", "FAKE-ENV-TOKEN");
    let spec =
        spec(r#"{"kind":"claude-code-oauth","env":"CLAUDE_CODE_OAUTH_TOKEN","store_only":true}"#);
    let locations = env.locations(&scratch);

    let report = describe(ROUTE, &spec, &locations);
    assert_eq!(
        report.chosen,
        Some(SourceName::Env("CLAUDE_CODE_OAUTH_TOKEN".to_string()))
    );
    assert!(
        report.line().ends_with(" [p1 store only]"),
        "{}",
        report.line()
    );

    let credential = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(source(&spec, &locations).access())
        .unwrap();
    assert_eq!(credential.bearer, "FAKE-ENV-TOKEN");
}

// ------------------------------------------------------------- the must-pass

#[tokio::test]
async fn an_absent_store_entry_is_an_error_that_reads_no_login() {
    let scratch = Scratch::new();
    block_external_reads(&scratch);
    let env = FakeEnv::new();
    let locations = env.locations(&scratch);

    for (spec, cli) in [
        (claude_store_only(), "Claude Code"),
        (codex_store_only(), "Codex"),
    ] {
        let error = source(&spec, &locations).access().await.unwrap_err();
        assert_eq!(error.kind, p1_contracts::ProviderErrorKind::Authentication);
        assert!(
            error.message.contains("no credential source has an entry"),
            "{}",
            error.message
        );
        assert!(
            !error.message.contains(cli),
            "a store-only chain named {cli}: {}",
            error.message
        );
        assert!(
            !error.message.contains("could not be read"),
            "something outside p1's store was read: {}",
            error.message
        );
    }
}

#[tokio::test]
async fn a_rejected_store_entry_never_falls_through_to_a_login() {
    let scratch = Scratch::new();
    block_external_reads(&scratch);
    scratch.write(STORE, &expired_store_oauth());
    let env = FakeEnv::new();
    let locations = env.locations(&scratch);

    // An expired entry refreshes; the provider refuses with 401.
    let chain = resolve(
        ROUTE,
        &claude_store_only(),
        Arc::new(refusing_transport()),
        &locations,
    );
    let error = chain.access().await.unwrap_err();
    assert!(
        error.message.contains("p1 store"),
        "the error names the store: {}",
        error.message
    );
    assert!(
        !error.message.contains("Claude Code") && !error.message.contains(SENTINEL),
        "{}",
        error.message
    );

    // The forced-refresh path after a 401 stops at the same store.
    let chain = resolve(
        ROUTE,
        &claude_store_only(),
        Arc::new(refusing_transport()),
        &locations,
    );
    let rejected = p1_provider_http::Credential {
        bearer: "FAKE-ACCESS".to_string(),
        account_id: None,
    };
    let error = chain.refresh(&rejected).await.unwrap_err();
    assert!(error.message.contains("p1 store"), "{}", error.message);
    assert!(!error.message.contains("Claude Code"), "{}", error.message);
}
