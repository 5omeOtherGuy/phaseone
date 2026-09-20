//! Must-pass (f) of the spec: no test in this crate reads the real home or the
//! real process environment.
//!
//! The first test proves it for the SUITE (a source scan), the second for the CODE
//! under test (an environment with no location at all finds nothing, so it cannot
//! have read anything).

mod support;

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use p1_auth::{CredentialSpec, Locations, describe, resolve};
use p1_provider_http::Credential;
use p1_provider_http::testing::ScriptedTransport;

/// Every test source of this crate: the test files and the shared module.
fn test_sources() -> Vec<PathBuf> {
    let tests = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests");
    let mut paths: Vec<PathBuf> = std::fs::read_dir(&tests)
        .expect("the tests directory")
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("rs")))
        .collect();
    paths.push(tests.join("support/mod.rs"));
    paths.sort();
    paths
}

#[test]
fn no_test_reads_the_real_home_or_the_process_environment() {
    // Assembled from pieces so this file does not match itself.
    let forbidden = [
        format!("std::env::{}", "var"),
        format!("Locations::{}", "from_process"),
        format!("{}::home_dir", "dirs"),
    ];
    let sources = test_sources();
    assert!(
        sources.len() >= 8,
        "the scan must see the whole suite, saw {} files",
        sources.len()
    );
    for path in sources {
        let text = std::fs::read_to_string(&path).expect("a test source is readable");
        for pattern in &forbidden {
            assert!(
                !text.contains(pattern.as_str()),
                "{} reads the real environment or home ({pattern})",
                path.display()
            );
        }
    }
}

#[tokio::test]
async fn an_environment_with_no_location_finds_no_source() {
    // `Locations::none()` is what a test starts from: no home, no directory
    // variable, nothing in the environment. Every source is absent, so the chain
    // reads no file at all — and a host built this way can never reach a real login.
    let locations = Locations::none();
    for spec in [
        r#"{"kind":"api-key","env":"P1_AUTH_TEST_KEY","borrow":["opencode:opencode-go","pi:opencode-go"]}"#,
        r#"{"kind":"claude-code-oauth"}"#,
        r#"{"kind":"codex-oauth"}"#,
    ] {
        let spec: CredentialSpec = serde_json::from_str(spec).unwrap();
        let report = describe("test-route", &spec, &locations);
        assert_eq!(report.chosen, None);
        assert!(
            report
                .tried
                .iter()
                .all(|(_, presence)| *presence == p1_auth::Presence::Absent),
            "{:?}",
            report.tried
        );
        let error = resolve(
            "test-route",
            &spec,
            Arc::new(ScriptedTransport::new(Vec::new())),
            &locations,
        )
        .access()
        .await
        .unwrap_err();
        assert_eq!(error.kind, p1_contracts::ProviderErrorKind::Authentication);
        assert!(
            error.message.contains("no credential source has an entry"),
            "{}",
            error.message
        );
    }

    // And a source with no location answers through the chain, never by guessing a
    // path: the rejected credential is not rotated against a path nobody named.
    let error = resolve(
        "test-route",
        &serde_json::from_str::<CredentialSpec>(r#"{"kind":"claude-code-oauth"}"#).unwrap(),
        Arc::new(ScriptedTransport::new(Vec::new())),
        &locations,
    )
    .refresh(&Credential {
        bearer: "FAKE-REJECTED".to_string(),
        account_id: None,
    })
    .await
    .unwrap_err();
    assert_eq!(error.kind, p1_contracts::ProviderErrorKind::Authentication);
}
