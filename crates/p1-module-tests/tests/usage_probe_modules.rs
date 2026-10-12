//! Usage-probe modules (ADR-0214): the built `usage-probe` components through p1's usage
//! ledger, their token origins at install, and their empty allocation. Every home, set and lock
//! is scratch data, and no probe reaches the network: each case stops before the request.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use p1_auth::{CredentialKind, CredentialSpec, Locations};
use p1_contracts::serde_json::json;
use p1_host::credentials::InstalledProbes;
use p1_module_tests::{fixture_dir, p1_binary, pack, publish_as};
use p1_provider_http::testing::ScriptedTransport;
use p1_usage::{FailKind, Probe, ProbeModules, UsageProbe, UsageRoute, WindowKind};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

/// The built set's manifest: p1's release in a development build.
fn release() -> PathBuf {
    fixture_dir()
        .parent()
        .expect("the build outputs directory")
        .join("manifest.json")
}

fn route(id: &str, kind: CredentialKind, probe: &str) -> UsageRoute {
    UsageRoute {
        route_id: id.into(),
        label: id.into(),
        credential: "test source".into(),
        store_id: None,
        probe: Some(probe.into()),
        compiled_origins: None,
        spec: CredentialSpec {
            kind,
            env: Some("FAKE_USAGE_KEY".into()),
            borrow: vec![],
            store_only: true,
            login_dir: None,
        },
    }
}

/// The release's components are the connectors' probes; the GLM one decodes its plan's body
/// into what the ledger shows, and the ledger reaches only the origin its manifest declares.
#[tokio::test]
async fn the_released_components_decode_and_describe_their_endpoints() {
    let probes = InstalledProbes::new(Vec::new(), Some(release()));
    let names = probes.names();
    for name in ["claude", "codex", "glm", "kimi", "opencode-go"] {
        assert!(names.iter().any(|known| known == name), "{names:?}");
    }
    let glm = probes.module("glm").unwrap().expect("the GLM probe");
    assert_eq!(glm.token_origins(), ["https://api.z.ai"]);
    let body = br#"{"data":{"limits":[
        {"type":"CREDIT_LIMIT","unit":3,"number":5,"percentage":0,"nextResetTime":1758700800000},
        {"type":"CREDIT_LIMIT","unit":6,"number":1,"percentage":100,"nextResetTime":1758876528000}]}}"#;
    let usage = p1_usage::decode_usage(
        &route("glm-subscription", CredentialKind::API_KEY, "glm"),
        glm.as_ref(),
        body,
    );
    assert!(matches!(usage.probe, Probe::Supported), "{:?}", usage.probe);
    assert!(matches!(usage.windows[0].kind, WindowKind::Session));
    assert_eq!(
        usage.windows[0].resets_at.as_deref(),
        Some("2025-09-24T08:00:00Z")
    );
    assert!(usage.windows[1].limit_reached);
    let refused = p1_usage::decode_usage(
        &route("glm-subscription", CredentialKind::API_KEY, "glm"),
        glm.as_ref(),
        br#"{"success":false,"message":"SENTINEL_PRIVATE_VALUE"}"#,
    );
    assert!(matches!(
        refused.probe,
        Probe::Failed {
            kind: FailKind::Credential,
            ..
        }
    ));

    // An unapproved store-only account stops at the probe origin the component describes,
    // before any key is read.
    let home = tempfile::tempdir().unwrap();
    let locations = Locations::none()
        .with_home(Some(home.path().to_owned()))
        .with_env_lookup(|_| None);
    let result = p1_usage::HttpProbe::new(Arc::new(probes))
        .probe(
            &route("private-kimi", CredentialKind::API_KEY, "kimi"),
            &locations,
            Arc::new(ScriptedTransport::new(Vec::new())),
        )
        .await;
    match result.probe {
        Probe::Unsupported { reason } => assert!(
            reason.contains("https://api.kimi.ai") && reason.contains("--trust-endpoint"),
            "{reason}"
        ),
        other => panic!("expected the origin refusal, got {other:?}"),
    }
}

/// `p1 modules install <archive> [--confirm-origins]` with `config` as the user's directory.
fn install(config: &Path, archive: &Path, confirm: bool) -> std::process::Output {
    let mut command = Command::new(p1_binary(&repo_root()));
    command.args(["modules", "install"]).arg(archive);
    if confirm {
        command.arg("--confirm-origins");
    }
    command
        .env("P1_CONFIG_DIR", config)
        .env("HOME", config)
        .output()
        .expect("p1")
}

/// ADR-0214: a usage probe's origins receive the account's credential, so they are shown at
/// install, installed only when the owner confirms them, and pinned in the lock.
#[test]
fn a_probes_origins_are_confirmed_at_install_and_pinned_in_the_lock() {
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("config");
    let build = publish_as(
        "p1-connector-usage-glm",
        "acme/glm-usage",
        scratch.path(),
        |_| {},
        |_| {},
    );
    let archive = scratch.path().join("glm-usage.p1pkg");
    pack(&build, &archive);

    let refused = install(&config, &archive, false);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("a usage probe") && stderr.contains("https://api.z.ai"),
        "{stderr}"
    );
    assert!(!config.join("modules.lock").exists(), "nothing installed");

    let installed = install(&config, &archive, true);
    assert!(installed.status.success(), "{installed:?}");
    let lock = std::fs::read_to_string(config.join("modules.lock")).unwrap();
    assert!(
        lock.contains(r#"token_origins = ["https://api.z.ai"]"#),
        "{lock}"
    );
    let probes = InstalledProbes::new(vec![config.join("environments")], Some(release()));
    assert!(probes.names().iter().any(|name| name == "glm-usage"));
    assert!(probes.module("glm-usage").unwrap().is_some());
}

/// The class's allocation is empty: a usage-probe component that imports anything is refused
/// at install whatever its manifest grants.
#[test]
fn a_probe_component_that_imports_anything_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let build = publish_as(
        "p1-module-fixture",
        "acme/greedy-usage",
        scratch.path(),
        |manifest| {
            manifest["kind"] = json!("usage-probe");
            manifest["world"] = json!("p1:module/usage-probe@1.2.0");
            manifest["capabilities"] = json!([]);
            manifest["token_origins"] = json!(["https://usage.acme.example"]);
        },
        |_| {},
    );
    let archive = scratch.path().join("greedy.p1pkg");
    pack(&build, &archive);
    let output = install(&scratch.path().join("config"), &archive, true);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not allocated"), "{stderr}");
}
