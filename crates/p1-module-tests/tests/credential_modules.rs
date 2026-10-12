//! Credential modules (ADR-0212): the built `credential-method` components through p1's
//! credential chain, their token origins at install and at load, and their empty allocation.
//! Every home, set and lock is scratch data; the components are the ones
//! `scripts/build-modules.sh` built, republished under another publisher's namespace where an
//! install needs one.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use p1_auth::{CredentialKind, CredentialModules, CredentialSpec, Locations, resolve};
use p1_contracts::serde_json::{self, Value, json};
use p1_host::credentials::InstalledCredentials;
use p1_module_tests::{fixture_dir, p1_binary, pack, publish_as};
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

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

fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let triple = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(ALPHABET[((triple >> 18) & 0x3F) as usize] as char);
        out.push(ALPHABET[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[((triple >> 6) & 0x3F) as usize] as char);
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[(triple & 0x3F) as usize] as char);
        }
    }
    out
}

/// A fake JWT whose payload is `payload`.
fn jwt(payload: Value) -> String {
    format!(
        "e30.{}.FAKE-SIGNATURE",
        base64url(payload.to_string().as_bytes())
    )
}

fn write_private(path: &Path, text: &str) {
    use std::os::unix::fs::PermissionsExt;
    let dir = path.parent().unwrap();
    std::fs::create_dir_all(dir).unwrap();
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    std::fs::write(path, text).unwrap();
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn spec(json: &str) -> CredentialSpec {
    serde_json::from_str(json).expect("a credential table")
}

/// The release's components serve the connectors' methods: a Codex login refreshed through
/// the `p1/codex-oauth` component, its rotation written back, and an OpenCode key borrowed
/// through `p1/opencode` — the module only reads and writes the formats, p1 sends the request.
#[tokio::test]
async fn the_released_components_serve_the_chain_and_rotate_the_login_file() {
    let home = tempfile::tempdir().unwrap();
    let locations = Locations::none()
        .with_home(Some(home.path().to_owned()))
        .with_modules(Arc::new(InstalledCredentials::new(
            Vec::new(),
            Some(release()),
        )));
    let names = InstalledCredentials::new(Vec::new(), Some(release())).names();
    for name in ["claude-code-oauth", "codex-oauth", "opencode", "pi"] {
        assert!(names.iter().any(|known| known == name), "{names:?}");
    }

    let login = home.path().join(".codex/auth.json");
    write_private(
        &login,
        &serde_json::to_string_pretty(&json!({
            "tokens": {
                "access_token": jwt(json!({"exp": 1})),
                "refresh_token": "FAKE-REFRESH-OLD",
                "account_id": "FAKE-ACCOUNT",
            },
            "keep": {"me": true},
        }))
        .unwrap(),
    );
    let fresh = jwt(json!({"exp": 4_102_444_800_u64}));
    let transport = ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![
            json!({"access_token": fresh, "refresh_token": "FAKE-REFRESH-NEW"})
                .to_string()
                .into_bytes(),
        ],
        end: BodyEnd::Eof,
    }]);
    let credential = resolve(
        "codex-route",
        &spec(r#"{"kind":"codex-oauth"}"#),
        Arc::new(transport.clone()),
        &locations,
    )
    .access()
    .await
    .expect("the component refreshed the login");
    assert_eq!(credential.bearer, fresh);
    assert_eq!(credential.account_id.as_deref(), Some("FAKE-ACCOUNT"));
    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");
    let written: Value = serde_json::from_slice(&std::fs::read(&login).unwrap()).unwrap();
    assert_eq!(written["tokens"]["access_token"], json!(fresh));
    assert_eq!(written["tokens"]["refresh_token"], "FAKE-REFRESH-NEW");
    assert_eq!(written["keep"]["me"], true);
    assert!(written["last_refresh"].is_string());

    write_private(
        &home.path().join(".local/share/opencode/auth.json"),
        &json!({"go": {"type": "api", "key": "FAKE-GO-KEY"}}).to_string(),
    );
    let credential = resolve(
        "go-route",
        &spec(r#"{"kind":"api-key","env":"FAKE_GO_KEY","borrow":["opencode:go"]}"#),
        Arc::new(ScriptedTransport::new(Vec::new())),
        &locations,
    )
    .access()
    .await
    .expect("the keyed login answers");
    assert_eq!(credential.bearer, "FAKE-GO-KEY");
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

/// The Codex component republished as `acme/codex-login`, its manifest edited by `edit`.
fn archive(dir: &Path, edit: impl FnOnce(&mut Value)) -> PathBuf {
    let build = publish_as(
        "p1-module-codex-oauth",
        "acme/codex-login",
        dir,
        edit,
        |_| {},
    );
    let archive = dir.join("codex-login.p1pkg");
    pack(&build, &archive);
    archive
}

/// ADR-0212: a credential method's token origins are shown at install, installed only when the
/// owner confirms them, pinned in the lock beside the digest, confirmed again when they
/// change, and a lock that pins other origins than the set's manifest is refused at load.
#[test]
fn token_origins_are_confirmed_at_install_and_pinned_in_the_lock() {
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("config");
    let first = archive(&scratch.path().join("one"), |_| {});

    let refused = install(&config, &first, false);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("https://auth.openai.com") && stderr.contains("--confirm-origins"),
        "{stderr}"
    );
    assert!(!config.join("modules.lock").exists(), "nothing installed");

    let installed = install(&config, &first, true);
    assert!(installed.status.success(), "{installed:?}");
    let stdout = String::from_utf8_lossy(&installed.stdout);
    assert!(
        stdout.contains("token refreshes go to https://auth.openai.com"),
        "{stdout}"
    );
    let lock = std::fs::read_to_string(config.join("modules.lock")).unwrap();
    assert!(
        lock.contains(r#"token_origins = ["https://auth.openai.com"]"#),
        "{lock}"
    );
    // The same origins again need no confirmation.
    assert!(install(&config, &first, false).status.success());

    // The installed module serves a route that names it.
    let dirs = vec![config.join("environments")];
    let installed_modules = Arc::new(InstalledCredentials::new(dirs.clone(), Some(release())));
    let locations = Locations::none().with_modules(installed_modules.clone());
    if let Err(error) = locations.method(&CredentialKind::module("codex-login")) {
        panic!("the installed method resolves: {error}");
    }

    // Other origins are confirmed again.
    let moved = archive(&scratch.path().join("two"), |manifest| {
        manifest["token_origins"] = json!(["https://tokens.acme.example"]);
    });
    let refused = install(&config, &moved, false);
    assert!(!refused.status.success(), "{refused:?}");
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(
        stderr.contains("https://tokens.acme.example")
            && stderr.contains("the lock pins https://auth.openai.com today"),
        "{stderr}"
    );

    // A lock pinning other origins than the set's manifest refuses the module.
    std::fs::write(
        config.join("modules.lock"),
        lock.replace("https://auth.openai.com", "https://tokens.acme.example"),
    )
    .unwrap();
    let error = InstalledCredentials::new(dirs, Some(release()))
        .module("codex-login")
        .err()
        .expect("the pin no longer matches");
    assert!(error.contains("token_origins"), "{error}");
}

/// The class's allocation is empty: a component that imports anything is refused at install
/// whatever its manifest grants.
#[test]
fn a_credential_component_that_imports_anything_is_refused() {
    let scratch = tempfile::tempdir().unwrap();
    let build = publish_as(
        "p1-module-fixture",
        "acme/greedy-login",
        scratch.path(),
        |manifest| {
            manifest["kind"] = json!("credential-method");
            manifest["world"] = json!("p1:module/credential-method@1.1.0");
            manifest["capabilities"] = json!([]);
            manifest["token_origins"] = json!([]);
        },
        |_| {},
    );
    let archive = scratch.path().join("greedy.p1pkg");
    pack(&build, &archive);
    let output = install(&scratch.path().join("config"), &archive, true);
    assert!(!output.status.success(), "{output:?}");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("not allocated"), "{stderr}");
    assert!(!scratch.path().join("config/modules.lock").exists());
}

/// `p1 usage` composes the installed credential modules like every other command: a
/// subscription route whose method a release component implements gets its usage probe, and
/// without a login the probe stops at the credential, so no request leaves. Built without
/// them, both rows read "no usage endpoint" (#912's live check).
#[test]
fn usage_probes_the_routes_whose_method_is_a_credential_module() {
    let scratch = tempfile::tempdir().unwrap();
    let config = scratch.path().join("config");
    std::fs::create_dir(&config).unwrap();
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    // A cleared environment: no key in the caller's environment can start a probe.
    let output = Command::new(p1_binary(&repo_root()))
        .args(["usage", "--json"])
        .env_clear()
        .env("P1_CONFIG_DIR", &config)
        .env("HOME", scratch.path())
        .output()
        .expect("p1");
    assert!(output.status.success(), "{output:?}");
    let report: Value = serde_json::from_slice(&output.stdout).expect("a usage report");
    for route in ["anthropic-subscription", "openai-codex-subscription"] {
        let row = report["routes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["route_id"] == route)
            .unwrap_or_else(|| panic!("a {route} row in {report}"));
        assert_eq!(
            row["probe"]["Failed"]["kind"], "Credential",
            "{route} is probed and stops at its missing login: {row}"
        );
    }
}
