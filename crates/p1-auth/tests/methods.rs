//! The credential-method port (ADR-0211) and credential modules (ADR-0212): what a lookup
//! composes, how a table names a method, the chain run over a module's method this crate knows
//! nothing about, and the token-origin check that runs before any refresh is sent.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_auth::{
    CredentialKind, CredentialModule, CredentialModules, CredentialPolicy, CredentialSpec,
    Locations, Presence, SourceName, describe, resolve,
};
use p1_provider_http::Credential;
use p1_provider_http::testing::ScriptedTransport;
use serde_json::json;

/// A credential module answering from a script: its description, the login it reads (the
/// file's text is the access token, `refresh:<token>` the refresh token) and the refresh
/// request's URL.
struct Relay {
    description: String,
    refresh_url: String,
    origins: Vec<String>,
    calls: Arc<AtomicUsize>,
}

impl CredentialModule for Relay {
    fn describe(&self) -> Result<String, String> {
        Ok(self.description.clone())
    }

    fn read_login(&self, file: &[u8], _key: Option<&str>) -> Result<String, String> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        let text = String::from_utf8_lossy(file);
        let (access, refresh) = text.split_once(' ').unwrap_or((&text, ""));
        Ok(json!({
            "access": access,
            "refresh": refresh.strip_prefix("refresh:"),
            "expires_at_ms": 1,
        })
        .to_string())
    }

    fn refresh_request(
        &self,
        _refresh_token: &str,
        _file: Option<&[u8]>,
    ) -> Result<String, String> {
        Ok(json!({"url": self.refresh_url, "headers": [], "body": ""}).to_string())
    }

    fn refresh_response(&self, _body: &[u8], _sent_at_ms: u64) -> Result<String, String> {
        Err("never reached".into())
    }

    fn merge_login(&self, _: &[u8], _: &[u8], _: u64, _: u64, _: bool) -> Result<Vec<u8>, String> {
        Err("never reached".into())
    }

    fn token_origins(&self) -> &[String] {
        &self.origins
    }
}

struct Installed {
    refresh_url: String,
    calls: Arc<AtomicUsize>,
}

impl CredentialModules for Installed {
    fn names(&self) -> Vec<String> {
        vec!["relay".into()]
    }

    fn module(&self, name: &str) -> Result<Option<Arc<dyn CredentialModule>>, String> {
        Ok((name == "relay").then(|| {
            Arc::new(Relay {
                description: json!({
                    "method": {"owner": "the relay", "store": "oauth", "label": "relay (oauth)"},
                    "login": {
                        "title": "relay login",
                        "guidance": "run the relay",
                        "dir_env": "RELAY_HOME",
                        "home_dir": ".relay",
                        "file": "login",
                    },
                })
                .to_string(),
                refresh_url: self.refresh_url.clone(),
                origins: vec!["https://tokens.relay.example".into()],
                calls: self.calls.clone(),
            }) as Arc<dyn CredentialModule>
        }))
    }
}

fn relay_login() -> SourceName {
    SourceName::Login {
        name: "relay login".into(),
        guidance: "run the relay".into(),
    }
}

fn installed(home: &std::path::Path, refresh_url: &str) -> (Locations, Arc<AtomicUsize>) {
    let calls = Arc::new(AtomicUsize::new(0));
    let locations = Locations::none()
        .with_home(Some(home.to_owned()))
        .with_modules(Arc::new(Installed {
            refresh_url: refresh_url.into(),
            calls: calls.clone(),
        }));
    (locations, calls)
}

fn spec(kind: &str) -> CredentialSpec {
    serde_json::from_str(&format!(r#"{{"kind":"{kind}"}}"#)).expect("a method name")
}

#[test]
fn a_lookup_composes_the_core_methods_and_the_installed_modules() {
    let home = tempfile::tempdir().unwrap();
    let names = |locations: &Locations| -> Vec<String> {
        locations
            .methods()
            .listed()
            .iter()
            .map(|method| method.label().to_owned())
            .collect()
    };
    assert_eq!(
        names(&Locations::none()),
        ["api-key", "none (proxy-injected)"]
    );
    let (locations, _) = installed(home.path(), "https://tokens.relay.example/token");
    assert_eq!(
        names(&locations),
        ["api-key", "relay (oauth)", "none (proxy-injected)"]
    );
}

#[test]
fn a_table_names_a_method_and_one_that_is_not_installed_is_refused_listing_them() {
    let home = tempfile::tempdir().unwrap();
    let (locations, _) = installed(home.path(), "https://tokens.relay.example/token");
    assert!(locations.method(&spec("relay").kind).is_ok());
    let error = locations
        .method(&spec("carrier-pigeon").kind)
        .err()
        .expect("not installed");
    assert!(error.contains("carrier-pigeon"), "{error}");
    assert!(error.contains("api-key, relay, none"), "{error}");
    // The chain over a method this p1 lacks names it, and reads nothing.
    let report = describe("pigeon-route", &spec("carrier-pigeon"), &locations);
    assert_eq!(report.chosen, None);
    assert!(
        report.line().contains("carrier-pigeon"),
        "{}",
        report.line()
    );
}

#[test]
fn the_chain_runs_over_a_module_method_and_its_login() {
    let home = tempfile::tempdir().unwrap();
    let (locations, calls) = installed(home.path(), "https://tokens.relay.example/token");
    let mut spec = spec("relay");
    spec.validate().expect("the table is valid");
    let report = describe("relay-route", &spec, &locations);
    assert_eq!(
        report.tried,
        vec![
            (SourceName::P1Store, Presence::Absent),
            (relay_login(), Presence::Absent),
        ]
    );
    assert_eq!(report.policy, CredentialPolicy::Chain);
    assert_eq!(
        report.line(),
        "none — add an entry to the p1 store, or run the relay"
    );
    assert_eq!(
        calls.load(Ordering::SeqCst),
        0,
        "no login file, nothing read"
    );

    std::fs::create_dir_all(home.path().join(".relay")).unwrap();
    std::fs::write(
        home.path().join(".relay/login"),
        "FAKE-ACCESS refresh:FAKE-R",
    )
    .unwrap();
    let report = describe("relay-route", &spec, &locations);
    assert_eq!(report.chosen, Some(relay_login()));

    // A store-only route never asks the method for its borrowed login (ADR-0061).
    spec.store_only = true;
    let report = describe("relay-route", &spec, &locations);
    assert_eq!(report.tried, vec![(SourceName::P1Store, Presence::Absent)]);

    // A method takes a `login_dir` only when its login says so.
    spec.store_only = false;
    spec.login_dir = Some("~/relay".into());
    let error = spec
        .validate_with(locations.method(&spec.kind).unwrap().as_ref())
        .unwrap_err();
    assert!(error.contains("login_dir"), "{error}");
}

/// ADR-0212: a refresh goes only over HTTPS to an origin the module's verified manifest
/// declares; any other request is refused before anything is sent or staged.
#[tokio::test]
async fn a_refresh_to_an_origin_the_manifest_does_not_declare_is_never_sent() {
    for url in [
        "https://exfiltrate.example/token",
        "http://tokens.relay.example/token",
        "https://tokens.relay.example@exfiltrate.example/token",
    ] {
        let home = tempfile::tempdir().unwrap();
        let (locations, _) = installed(home.path(), url);
        std::fs::create_dir_all(home.path().join(".relay")).unwrap();
        std::fs::write(
            home.path().join(".relay/login"),
            "FAKE-ACCESS refresh:FAKE-R",
        )
        .unwrap();
        let before = std::fs::read(home.path().join(".relay/login")).unwrap();
        let transport = ScriptedTransport::new(Vec::new());
        let source = resolve(
            "relay-route",
            &spec("relay"),
            Arc::new(transport.clone()),
            &locations,
        );
        // The login's token is long expired, so access asks for a refresh.
        let error = source.access().await.unwrap_err();
        assert!(
            error.message.contains("nothing was sent"),
            "{url}: {}",
            error.message
        );
        assert!(!error.message.contains("FAKE-"), "{}", error.message);
        assert!(transport.requests().is_empty(), "{url}: a request was sent");
        assert_eq!(
            std::fs::read(home.path().join(".relay/login")).unwrap(),
            before
        );
        let rejected = Credential {
            bearer: "FAKE-ACCESS".into(),
            account_id: None,
        };
        assert!(source.refresh(&rejected).await.is_err());
        assert!(transport.requests().is_empty(), "{url}: a request was sent");
    }
}

#[test]
fn a_keyed_login_is_borrowed_by_name_and_an_unknown_store_is_unusable() {
    let home = tempfile::tempdir().unwrap();
    let (locations, _) = installed(home.path(), "https://tokens.relay.example/token");
    let spec: CredentialSpec =
        serde_json::from_str(r#"{"kind":"api-key","env":"A_KEY","borrow":["relay:go","nope:go"]}"#)
            .unwrap();
    let report = describe("key-route", &spec, &locations);
    // `relay` is a method, not a keyed login, and `nope` is not installed: neither is read.
    assert!(
        report
            .tried
            .iter()
            .skip(2)
            .all(|(_, presence)| matches!(presence, Presence::Unusable(_))),
        "{:?}",
        report.tried
    );
    assert_eq!(report.chosen, None);
    assert_eq!(CredentialKind::API_KEY.name(), "api-key");
}
