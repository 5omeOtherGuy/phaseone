//! The credential-method port (ADR-0211): what a build composes, how a table names a method,
//! and the chain run over a method this crate does not define, so a new method needs no change
//! to the chain, the store or the report.

use std::sync::Arc;

use p1_auth::{
    BorrowedLogin, CredentialKind, CredentialMethod, CredentialPolicy, CredentialSpec, Entry,
    Locations, Presence, SourceName, StoreForm, describe, methods,
};
use p1_provider_http::Transport;

#[test]
fn a_build_composes_the_core_methods_and_its_parts() {
    let names: Vec<&str> = methods().iter().map(|method| method.name()).collect();
    #[cfg(feature = "subscriptions")]
    assert_eq!(
        names,
        ["api-key", "claude-code-oauth", "codex-oauth", "none"]
    );
    #[cfg(not(feature = "subscriptions"))]
    assert_eq!(names, ["api-key", "none"]);
    assert_eq!(CredentialKind::API_KEY.label(), "api-key");
    assert_eq!(CredentialKind::NONE.label(), "none (proxy-injected)");
}

#[test]
fn a_table_names_a_method_this_build_has_or_is_refused_listing_them() {
    let spec: CredentialSpec =
        serde_json::from_str(r#"{"kind":"api-key","env":"A_KEY"}"#).expect("a known method");
    assert_eq!(spec.kind, CredentialKind::API_KEY);
    let error = serde_json::from_str::<CredentialSpec>(r#"{"kind":"carrier-pigeon"}"#)
        .unwrap_err()
        .to_string();
    assert!(error.contains("carrier-pigeon"), "{error}");
    assert!(error.contains("api-key, "), "{error}");
}

/// A method this crate does not define: a key in p1's store, then one borrowed login.
struct Relay;

impl CredentialMethod for Relay {
    fn name(&self) -> &'static str {
        "relay"
    }

    fn owner(&self) -> &'static str {
        "the relay"
    }

    fn store(&self) -> StoreForm {
        StoreForm::ApiKey
    }

    fn borrowed(&self, _spec: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
        vec![Box::new(RelayLogin)]
    }
}

const RELAY_LOGIN: SourceName = SourceName::Login {
    name: "relay login",
    guidance: "run the relay",
};

struct RelayLogin;

impl BorrowedLogin for RelayLogin {
    fn name(&self) -> SourceName {
        RELAY_LOGIN
    }

    fn presence(&self, _locations: &Locations) -> Presence {
        Presence::Present
    }

    fn entry(&self, _locations: &Locations, _transport: Arc<dyn Transport>) -> Box<dyn Entry> {
        unreachable!("describe builds no entry")
    }
}

#[test]
fn the_chain_runs_over_any_method() {
    let home = tempfile::tempdir().unwrap();
    let locations = Locations::none().with_home(Some(home.path().to_owned()));
    let mut spec = CredentialSpec {
        kind: CredentialKind::of(&Relay),
        env: None,
        borrow: Vec::new(),
        store_only: false,
        login_dir: None,
    };
    spec.validate().expect("the method's table is valid");
    let report = describe("relay-route", &spec, &locations);
    assert_eq!(report.chosen, Some(RELAY_LOGIN));
    assert_eq!(
        report.tried,
        vec![
            (SourceName::P1Store, Presence::Absent),
            (RELAY_LOGIN, Presence::Present),
        ]
    );
    assert_eq!(report.policy, CredentialPolicy::Chain);

    // A store-only route never asks the method for its borrowed logins (ADR-0061).
    spec.store_only = true;
    let report = describe("relay-route", &spec, &locations);
    assert_eq!(report.tried, vec![(SourceName::P1Store, Presence::Absent)]);
    assert_eq!(report.chosen, None);
    assert!(
        report.line().contains("add an entry to the p1 store"),
        "{}",
        report.line()
    );

    // A method takes a `login_dir` only when it says so; the refusal names those that do.
    spec.store_only = false;
    spec.login_dir = Some("~/relay".into());
    let error = spec.validate().unwrap_err();
    assert!(error.contains("login_dir"), "{error}");
}

#[test]
fn guidance_comes_from_the_borrowed_login() {
    let locations = Locations::none();
    struct Absent;
    impl BorrowedLogin for Absent {
        fn name(&self) -> SourceName {
            RELAY_LOGIN
        }
        fn presence(&self, _locations: &Locations) -> Presence {
            Presence::Absent
        }
        fn entry(&self, _: &Locations, _: Arc<dyn Transport>) -> Box<dyn Entry> {
            unreachable!()
        }
    }
    struct Quiet;
    impl CredentialMethod for Quiet {
        fn name(&self) -> &'static str {
            "quiet"
        }
        fn owner(&self) -> &'static str {
            "nobody"
        }
        fn store(&self) -> StoreForm {
            StoreForm::ApiKey
        }
        fn borrowed(&self, _: &CredentialSpec) -> Vec<Box<dyn BorrowedLogin>> {
            vec![Box::new(Absent)]
        }
    }
    let spec = CredentialSpec {
        kind: CredentialKind::of(&Quiet),
        env: Some("QUIET_KEY".into()),
        borrow: Vec::new(),
        store_only: false,
        login_dir: None,
    };
    let line = describe("quiet", &spec, &locations).line();
    assert_eq!(
        line,
        "none — set QUIET_KEY, or add an entry to the p1 store, or run the relay"
    );
}
