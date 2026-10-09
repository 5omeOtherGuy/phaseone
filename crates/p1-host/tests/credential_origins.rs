//! Origin refusals precede credential resolution and authenticated transport.
mod common;

#[allow(dead_code)]
#[path = "../build.rs"]
mod build_script;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Harness, run_args};
use p1_host::routes::{
    RouteFile, check_credential_origin, check_shipped_origin, load_route, shipped_origins,
    shipped_origins_in,
};
use p1_provider_http::testing::{ScriptedResponse, ScriptedTransport};

fn repo(relative: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join(relative)
}

const ENV_KEY: &str = "TEST_KEY";

#[tokio::test]
async fn inspection_refuses_unapproved_origins_without_reading_credentials() {
    use p1_host::catalog::credential_line_for_route;
    let scratch = Scratch::new(
        "inspection-api",
        "kind = \"api-key\"\nenv = \"TEST_KEY\"\nstore_only = true",
        "https://unapproved.example/v1",
    );
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let locations = p1_auth::Locations::none()
        .with_home(Some(scratch.home()))
        .with_env_lookup(move |name| {
            if name == ENV_KEY {
                count.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    // An unreadable credential document must not affect the approval-only report.
    let store_dir = scratch.home().join(".config/p1");
    std::fs::create_dir_all(&store_dir).unwrap();
    std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::os::unix::fs::symlink("missing", store_dir.join("auth.json")).unwrap();
    let line = credential_line_for_route(
        "inspection-api",
        &[scratch.dir.path().join("environments")],
        &locations,
    )
    .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), 0);
    assert!(line.contains("https://unapproved.example"), "{line}");
    assert!(line.contains("not approved"), "{line}");
    assert!(
        line.contains("p1 login inspection-api --trust-endpoint"),
        "{line}"
    );
    for args in [
        vec!["env", "show", "chosen"],
        vec!["models"],
        vec!["login", "--list"],
    ] {
        let mut harness = scratch.harness(&[]);
        assert_eq!(
            run_args(&mut harness, &args).await,
            0,
            "{}",
            harness.stderr.text()
        );
        let text = harness.stdout.text();
        assert!(text.contains("not approved"), "{args:?}: {text}");
        assert!(text.contains("p1 login inspection-api"), "{text}");
        assert!(
            !text.contains("symlink"),
            "credential document inspected: {text}"
        );
    }
}

#[tokio::test]
async fn usage_skips_store_origin_mismatch_without_reading_a_key_or_document() {
    use p1_usage::{HttpProbe, Probe, UsageProbe, UsageRoute};
    let scratch = tempfile::tempdir().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let locations = p1_auth::Locations::none()
        .with_home(Some(scratch.path().to_owned()))
        .with_env_lookup(move |name| {
            if name == ENV_KEY {
                count.fetch_add(1, Ordering::SeqCst);
            }
            None
        });
    p1_auth::store::trust_endpoint("private-oauth", "https://private.example", &locations)
        .await
        .unwrap();
    std::os::unix::fs::symlink("missing", scratch.path().join(".config/p1/auth.json")).unwrap();
    let mut spec = api_route().credential;
    spec.kind = p1_auth::CredentialKind::ClaudeCodeOauth;
    spec.store_only = true;
    spec.borrow.clear();
    let route = UsageRoute {
        route_id: "private-oauth".into(),
        label: "private".into(),
        credential: "p1 store".into(),
        spec,
        store_id: None,
        probe: None,
    };
    let mut route = route;
    for store_only in [true, false] {
        route.spec.store_only = store_only;
        let result = HttpProbe
            .probe(&route, &locations, Arc::new(ScriptedTransport::new(vec![])))
            .await;
        assert_eq!(reads.load(Ordering::SeqCst), 0);
        match result.probe {
            Probe::Unsupported { reason } => assert!(
                reason.contains("https://api.anthropic.com") && reason.contains("not approved"),
                "{reason}"
            ),
            other => panic!("expected origin refusal, got {other:?}"),
        }
    }
    // Approval for the actual probe origin permits resolution; the deliberately
    // unreadable document then produces a credential failure, without a GET.
    p1_auth::store::trust_endpoint("private-oauth", "https://api.anthropic.com", &locations)
        .await
        .unwrap();
    route.spec.store_only = true;
    let result = HttpProbe
        .probe(&route, &locations, Arc::new(ScriptedTransport::new(vec![])))
        .await;
    assert!(reads.load(Ordering::SeqCst) > 0);
    assert!(matches!(
        result.probe,
        Probe::Failed {
            kind: p1_usage::FailKind::Credential,
            ..
        }
    ));
}

fn api_route() -> RouteFile {
    // A genuinely custom implicit account, not the shipped Z.ai account's store
    // identity and origin restrictions with its route id renamed.
    let scratch = Scratch::new(
        "new-api",
        "kind = \"api-key\"\nenv = \"TEST_KEY\"\nstore_only = true",
        "https://custom.example/v1/chat/completions",
    );
    load_route(&scratch.dir.path().join("routes/new-api.toml")).unwrap()
}

#[test]
fn missing_install_prefix_does_not_remove_the_origin_anchor() {
    let scratch = tempfile::tempdir().unwrap();
    let mut route = load_route(&repo("routes/anthropic-subscription.toml")).unwrap();
    route.endpoint = "https://attacker.example/v1".into();
    let origins = shipped_origins_in(&[scratch.path().join("missing/share/p1/routes")]);
    assert!(check_shipped_origin(&route, &origins).is_err());
    std::fs::write(scratch.path().join("broken.toml"), "[not valid").unwrap();
    assert_eq!(
        shipped_origins_in(&[scratch.path().to_owned()]),
        shipped_origins()
    );
}

#[test]
fn malformed_shipped_file_fails_the_build_table_generator() {
    let scratch = tempfile::tempdir().unwrap();
    let routes = scratch.path().join("routes");
    let accounts = scratch.path().join("accounts");
    std::fs::create_dir_all(&routes).unwrap();
    std::fs::create_dir_all(&accounts).unwrap();
    std::fs::copy(repo("accounts/zai.toml"), accounts.join("zai.toml")).unwrap();
    std::fs::copy(
        repo("routes/glm-subscription.toml"),
        routes.join("glm-subscription.toml"),
    )
    .unwrap();
    assert!(build_script::shipped_route_table(&routes).contains("glm-subscription"));
    std::fs::write(routes.join("broken.toml"), "[not valid").unwrap();
    assert!(std::panic::catch_unwind(|| build_script::shipped_route_table(&routes)).is_err());
}

struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new(id: &str, credential: &str, endpoint: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        for directory in ["environments/chosen", "routes", "profiles", "home"] {
            std::fs::create_dir_all(root.join(directory)).unwrap();
        }
        std::fs::write(root.join(format!("routes/{id}.toml")), format!(
            "id = \"{id}\"\norigin_route = \"openai-chat/{id}\"\nadapter = \"openai-chat\"\nendpoint = \"{endpoint}\"\n[credential]\n{credential}\n[adapter_settings]\ndialect = \"retained-thinking\"\n[models.model]\nwire_model = \"model\"\n"
        )).unwrap();
        std::fs::write(root.join("profiles/model.toml"), "id = \"model\"\nrevision = 1\nmodel_id = \"model\"\nfamily = \"test\"\nthinking = \"enabled\"\nefforts = [\"high\"]\ndefault_effort = \"high\"\n").unwrap();
        std::fs::write(
            root.join("environments/chosen/environment.toml"),
            format!("route = \"{id}\"\nprofile = \"model\"\n"),
        )
        .unwrap();
        std::fs::write(root.join("environments/chosen/prompt.md"), "test").unwrap();
        Self { dir }
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn harness(&self, lines: &[&str]) -> Harness {
        let mut harness = Harness::new(vec![self.dir.path().join("environments")], lines);
        harness.deps.home = Some(self.home());
        harness.deps.shell_env = Some(vec![(
            ENV_KEY.into(),
            format!("FAKE-{}", "sentinel").into(),
        )]);
        harness
    }

    async fn run(&self, harness: &mut Harness) -> i32 {
        run_args(
            harness,
            &[
                "--yes",
                "--env",
                "chosen",
                "--workspace",
                self.dir.path().to_str().unwrap(),
                "go",
            ],
        )
        .await
    }
}

#[tokio::test]
async fn attacker_overrides_are_refused_before_credentials_or_transport() {
    for (id, credential) in [
        (
            "anthropic-subscription",
            "kind = \"claude-code-oauth\"\nenv = \"TEST_KEY\"",
        ),
        (
            "new-claude",
            "kind = \"claude-code-oauth\"\nenv = \"TEST_KEY\"",
        ),
        ("new-codex", "kind = \"codex-oauth\"\nenv = \"TEST_KEY\""),
        (
            "new-borrow",
            "kind = \"api-key\"\nenv = \"TEST_KEY\"\nborrow = [\"pi:example\"]",
        ),
        (
            "new-env",
            "kind = \"api-key\"\nenv = \"TEST_KEY\"\nstore_only = true",
        ),
    ] {
        let scratch = Scratch::new(
            id,
            credential,
            "https://attacker.example/v1/chat/completions",
        );
        let mut harness = scratch.harness(&[]);
        let transport = Arc::new(ScriptedTransport::new(Vec::new()));
        harness.deps.transport = transport.clone();
        let code = scratch.run(&mut harness).await;
        assert_ne!(code, 0, "{id}");
        let error = harness.stderr.text();
        assert!(
            error.contains("credential")
                && (error.contains("endpoint") || error.contains("p1 login")),
            "{id}: {error}"
        );
        assert!(
            transport.requests().is_empty(),
            "{id}: no authenticated request"
        );
    }
}

#[tokio::test]
async fn origin_refusal_reads_no_key_and_metadata_is_protected() {
    let scratch = tempfile::tempdir().unwrap();
    let reads = Arc::new(AtomicUsize::new(0));
    let count = reads.clone();
    let locations = p1_auth::Locations::none()
        .with_home(Some(scratch.path().to_owned()))
        .with_env_lookup(move |_| {
            count.fetch_add(1, Ordering::SeqCst);
            Some(format!("FAKE-{}", "key"))
        });
    let route = api_route();
    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    assert!(
        check_credential_origin(&route, &locations)
            .unwrap_err()
            .contains("p1 login new-api --trust-endpoint")
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0);

    p1_auth::store::trust_endpoint(&route.id, "https://custom.example", &locations)
        .await
        .unwrap();
    let store = scratch.path().join(".config/p1/auth.json");
    let metadata = store.with_file_name("auth.json.origins");
    assert!(!store.exists(), "trust stores no key");
    assert_eq!(
        std::fs::metadata(&metadata).unwrap().permissions().mode() & 0o777,
        0o600
    );
    let workspace = p1_workspace::Workspace::new(scratch.path()).unwrap();
    assert!(
        p1_workspace::refuse_credentials(
            &workspace,
            metadata.to_str().unwrap(),
            Some(scratch.path()),
            &[]
        )
        .is_err()
    );
    // A credential document that cannot be read must not be opened by the origin check.
    std::fs::write(&store, "not a credential document").unwrap();
    check_credential_origin(&route, &locations).unwrap();
    let source = p1_host::auth::credential_source_at(&route, transport.clone(), &locations);
    source.access().await.unwrap();
    let credential_reads = reads.load(Ordering::SeqCst);
    assert!(credential_reads > 0);
    let mut moved = route.clone();
    moved.endpoint = "https://attacker.example".into();
    assert!(check_credential_origin(&moved, &locations).is_err());
    assert_eq!(reads.load(Ordering::SeqCst), credential_reads);
    assert!(transport.requests().is_empty());
    std::fs::set_permissions(&metadata, std::fs::Permissions::from_mode(0o644)).unwrap();
    assert!(check_credential_origin(&route, &locations).is_err());
}

#[test]
fn borrowed_kinds_keep_their_shipped_origins_and_loopback_is_explicit() {
    let locations = p1_auth::Locations::none();
    for id in ["anthropic-subscription", "openai-codex-subscription"] {
        let mut route = load_route(&repo(&format!("routes/{id}.toml"))).unwrap();
        route.id = format!("new-{id}");
        route.credential.store_only = false;
        check_credential_origin(&route, &locations).unwrap();
        route.endpoint = "https://attacker.example".into();
        assert!(check_credential_origin(&route, &locations).is_err());
    }
    let mut route = api_route();
    for endpoint in [
        "http://127.0.0.1:8181/v1",
        "https://localhost/v1",
        "http://[::1]:8181/v1",
    ] {
        route.endpoint = endpoint.into();
        check_credential_origin(&route, &locations).unwrap();
    }
    for endpoint in [
        "http://localhost.attacker.example",
        "http://localhost@attacker.example",
        "http://127.0.0.1@attacker.example",
        "http://[::1]@attacker.example",
        "http://localhost:bad",
    ] {
        route.endpoint = endpoint.into();
        assert!(
            check_credential_origin(&route, &locations).is_err(),
            "{endpoint}"
        );
    }
}

#[tokio::test]
async fn trusted_new_id_sends_an_environment_key_over_scripted_transport() {
    let scratch = Scratch::new(
        "new-env",
        "kind = \"api-key\"\nenv = \"TEST_KEY\"\nstore_only = true",
        "https://custom.example/v1/chat/completions",
    );
    let mut harness = scratch.harness(&["go", "/exit"]);
    assert_eq!(
        run_args(&mut harness, &["login", "new-env", "--trust-endpoint"]).await,
        0
    );
    assert!(!scratch.home().join(".config/p1/auth.json").exists());
    assert!(harness.stdout.text().contains("no key stored"));
    let sse = "data: {\"id\":\"one\",\"choices\":[{\"delta\":{\"role\":\"assistant\",\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"id\":\"one\",\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
    let transport = Arc::new(ScriptedTransport::new(vec![ScriptedResponse::ok_sse(sse)]));
    harness.deps.transport = transport.clone();
    assert_eq!(
        run_args(
            &mut harness,
            &[
                "--env",
                "chosen",
                "--workspace",
                scratch.dir.path().to_str().unwrap()
            ]
        )
        .await,
        0,
        "{}",
        harness.stderr.text()
    );
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0].url,
        "https://custom.example/v1/chat/completions"
    );
    assert!(
        requests[0]
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("authorization"))
    );
}

#[tokio::test]
async fn trust_endpoint_approves_store_only_oauth_without_reading_credentials_or_stdin() {
    for kind in ["claude-code-oauth", "codex-oauth"] {
        let scratch = Scratch::new(
            "stored-oauth",
            &format!("kind = \"{kind}\"\nstore_only = true"),
            "https://custom.example/v1",
        );
        let store_dir = scratch.home().join(".config/p1");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::set_permissions(&store_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        // Parsing this sentinel would fail; approval must not open the document.
        let store = store_dir.join("auth.json");
        let sentinel = b"not a credential document\n";
        std::fs::write(&store, sentinel).unwrap();
        std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();
        let recovery = store_dir.join(".auth.json.p1-unsaved");
        let recovery_bytes = br#"{"stored-oauth":{"type":"oauth","access":"FAKE-RECOVERY","refresh":null,"expires":null}}"#;
        std::fs::write(&recovery, recovery_bytes).unwrap();
        std::fs::set_permissions(&recovery, std::fs::Permissions::from_mode(0o600)).unwrap();
        let metadata_recovery = store_dir.join(".auth.json.origins.p1-unsaved");
        std::fs::write(&metadata_recovery, br#"{"other":"https://other.example"}"#).unwrap();
        std::fs::set_permissions(&metadata_recovery, std::fs::Permissions::from_mode(0o600))
            .unwrap();
        let mut harness = scratch.harness(&["unconsumed"]);
        assert_eq!(
            run_args(&mut harness, &["login", "stored-oauth", "--trust-endpoint"]).await,
            0,
            "{}",
            harness.stderr.text()
        );
        assert_eq!(std::fs::read(&store).unwrap(), sentinel);
        assert_eq!(std::fs::read(&recovery).unwrap(), recovery_bytes);
        assert_eq!(
            harness.deps.lines.next_line().await.as_deref(),
            Some("unconsumed")
        );
        let locations = p1_auth::Locations::none().with_home(Some(scratch.home()));
        assert_eq!(
            p1_auth::store::endpoint_origin("stored-oauth", &locations)
                .unwrap()
                .as_deref(),
            Some("https://custom.example")
        );
        assert_eq!(
            p1_auth::store::endpoint_origin("other", &locations)
                .unwrap()
                .as_deref(),
            Some("https://other.example")
        );
        assert_eq!(
            std::fs::metadata(store_dir.join("auth.json.origins"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
        let route = load_route(&scratch.dir.path().join("routes/stored-oauth.toml")).unwrap();
        check_credential_origin(&route, &locations).unwrap();
    }
}

#[tokio::test]
async fn trust_endpoint_refuses_borrowed_oauth_and_none_without_changing_approval() {
    for credential in [
        "kind = \"claude-code-oauth\"",
        "kind = \"codex-oauth\"",
        "kind = \"none\"",
    ] {
        let scratch = Scratch::new("refused", credential, "https://attacker.example/v1");
        let locations = p1_auth::Locations::none().with_home(Some(scratch.home()));
        p1_auth::store::trust_endpoint("refused", "https://original.example", &locations)
            .await
            .unwrap();
        let mut harness = scratch.harness(&["unconsumed"]);
        assert_eq!(
            run_args(&mut harness, &["login", "refused", "--trust-endpoint"]).await,
            p1_host::run::EXIT_USAGE
        );
        assert!(
            harness
                .stderr
                .text()
                .contains("requires an api-key or store-only OAuth")
        );
        assert_eq!(
            harness.deps.lines.next_line().await.as_deref(),
            Some("unconsumed")
        );
        assert_eq!(
            p1_auth::store::endpoint_origin("refused", &locations)
                .unwrap()
                .as_deref(),
            Some("https://original.example")
        );
        assert!(!scratch.home().join(".config/p1/auth.json").exists());
    }
}

#[tokio::test]
async fn pasted_login_records_origin_and_logout_revokes_it() {
    let scratch = Scratch::new(
        "new-key",
        "kind = \"api-key\"\nenv = \"TEST_KEY\"\nstore_only = true",
        "https://custom.example/v1/chat/completions",
    );
    let harness = scratch.harness(&["FAKE-PASTED"]);
    assert_eq!(
        p1_host::login::login_with(
            &harness.deps,
            "new-key",
            false,
            &p1_host::login::TerminalEcho
        )
        .await,
        0
    );
    let locations = p1_auth::Locations::none().with_home(Some(scratch.home()));
    assert_eq!(
        p1_auth::store::endpoint_origin("new-key", &locations)
            .unwrap()
            .as_deref(),
        Some("https://custom.example")
    );
    assert_eq!(p1_host::login::logout(&harness.deps, "new-key").await, 0);
    assert_eq!(
        p1_auth::store::endpoint_origin("new-key", &locations).unwrap(),
        None
    );
}
