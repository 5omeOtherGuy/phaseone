//! The self-contained credential policy through the host (ADR-0061): every shipped
//! route carries `store_only = true`, the policy is visible in `p1 env show` and
//! `p1 login --list`, a `[credential]` table that sets the field and still borrows is
//! a load error, and `p1 login` on a store-only OAuth route says where its credential
//! really comes from.
//!
//! No test here reads a credential file, the network or the real home: the shipped
//! routes are probed with an injected EMPTY environment and no home, so every source
//! is absent.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Harness, isolated_environment, run_args, shipped_environments};
use p1_host::login::{EchoControl, Guard};
use p1_provider_http::CredentialSource;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

const ROUTE: &str = "self-contained-oauth";

fn shipped(relative: &str) -> std::path::PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../")
        .join(relative)
}

/// A scratch host with an empty routes directory, the way `p1 login --list` reads
/// them: nothing here can point outside the scratch tree.
struct Scratch {
    dir: tempfile::TempDir,
}

impl Scratch {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for sub in ["environments", "routes", "home"] {
            std::fs::create_dir_all(dir.path().join(sub)).unwrap();
        }
        Self { dir }
    }

    fn route_path(&self, id: &str) -> std::path::PathBuf {
        self.dir.path().join("routes").join(format!("{id}.toml"))
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn write_route(&self, id: &str, credential: &str) {
        let text = format!(
            "id           = \"{id}\"\n\
             origin_route = \"openai-chat/{id}\"\n\
             adapter      = \"openai-chat\"\n\
             endpoint     = \"https://example.invalid/v1/chat/completions\"\n\
             \n[credential]\n{credential}\n\
             \n[adapter_settings]\ndialect = \"retained-thinking\"\n"
        );
        std::fs::write(self.route_path(id), text).unwrap();
    }

    fn harness(&self) -> Harness {
        let mut harness = Harness::new(vec![self.dir.path().join("environments")], &[]);
        harness.deps.home = Some(self.home());
        harness.deps.shell_env = Some(Vec::new());
        harness
    }
}

/// p1's store as the crate writes it: 0600 in a 0700 directory, or the read is
/// refused.
fn write_store(home: &Path, contents: &str) {
    let dir = home.join(".config/p1");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let path = dir.join("auth.json");
    std::fs::write(&path, contents).unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
}

fn store_path(home: &Path) -> PathBuf {
    home.join(".config/p1/auth.json")
}

/// A perfectly good Claude Code login file, which a store-only route must never read.
fn write_claude_login(home: &Path, access: &str) -> PathBuf {
    let dir = home.join(".claude");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(".credentials.json");
    std::fs::write(
        &path,
        serde_json::json!({
            "claudeAiOauth": {
                "accessToken": access,
                "refreshToken": "FAKE-CLI-REFRESH",
                "expiresAt": 4_102_444_800_000u64,
            }
        })
        .to_string(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// A valid Codex CLI auth file, which a store-only route must never read or write.
fn write_codex_login(home: &Path) -> PathBuf {
    let dir = home.join(".codex");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("auth.json");
    std::fs::write(
        &path,
        serde_json::to_vec_pretty(&serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "access_token": "FAKE-CLI-ACCESS",
                "refresh_token": "FAKE-CLI-REFRESH",
                "account_id": "FAKE-CLI-ACCOUNT",
            },
            "last_refresh": "2000-01-01T00:00:00Z",
        }))
        .unwrap(),
    )
    .unwrap();
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
    path
}

/// The REAL credential source the host composes for a shipped route, over a scratch
/// home: the same `credential_source_at` the catalog factory calls.
fn shipped_source(
    route_id: &str,
    home: &Path,
    transport: Arc<ScriptedTransport>,
) -> Arc<dyn CredentialSource> {
    let route = p1_host::routes::load_route_by_id(&[shipped_environments()], route_id)
        .unwrap_or_else(|error| panic!("{route_id}: {error}"));
    assert!(route.credential.store_only, "{route_id} is store-only");
    let locations = p1_auth::Locations::none().with_home(Some(home.to_path_buf()));
    p1_host::auth::credential_source_at(&route, transport, &locations)
}

// -------------------------------------------------------------- the shipped routes

#[test]
fn the_shipped_oauth_routes_are_store_only() {
    for (file, kind) in [
        ("routes/anthropic-subscription.toml", "claude-code-oauth"),
        ("routes/openai-codex-subscription.toml", "codex-oauth"),
    ] {
        let path = shipped(file);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(
            text.contains("store_only = true"),
            "ADR-0061: {file} must not read another tool's login"
        );
        let route =
            p1_host::routes::load_route(&path).unwrap_or_else(|error| panic!("{file}: {error}"));
        assert!(route.credential.store_only, "{file}");
        assert_eq!(route.credential.kind.name(), kind, "{file}");
    }
}

/// Every shipped API-key route is self-contained too: the documented variable and
/// p1's own store, and no nonempty `borrow` list that could reach another CLI.
#[test]
fn every_shipped_key_route_is_store_only() {
    let expected = [
        ("routes/glm-subscription.toml", "ZAI_API_KEY"),
        ("routes/kimi-coding-subscription.toml", "KIMI_API_KEY"),
        ("routes/opencode-go-subscription.toml", "OPENCODE_API_KEY"),
        (
            "routes/opencode-go-1-subscription.toml",
            "OPENCODE_GO_1_API_KEY",
        ),
        (
            "routes/opencode-go-2-subscription.toml",
            "OPENCODE_GO_2_API_KEY",
        ),
        (
            "routes/opencode-go-3-subscription.toml",
            "OPENCODE_GO_3_API_KEY",
        ),
        ("routes/opencode-zen-1.toml", "OPENCODE_ZEN_1_API_KEY"),
        ("routes/opencode-zen-2.toml", "OPENCODE_ZEN_2_API_KEY"),
        ("routes/opencode-zen-3.toml", "OPENCODE_ZEN_3_API_KEY"),
        ("routes/opencode-zen-free.toml", "OPENCODE_ZEN_API_KEY"),
        ("routes/cline-pass-1.toml", "CLINE_PASS_1_API_KEY"),
        ("routes/cline-pass-2.toml", "CLINE_PASS_2_API_KEY"),
    ];
    for (file, env) in expected {
        let route = p1_host::routes::load_route(&shipped(file))
            .unwrap_or_else(|error| panic!("{file}: {error}"));
        assert_eq!(route.credential.kind.name(), "api-key", "{file}");
        assert_eq!(route.credential.env.as_deref(), Some(env), "{file}");
        assert!(route.credential.store_only, "{file} must be self-contained");
        assert!(
            route.credential.borrow.is_empty(),
            "{file} must list no borrow source"
        );
    }
}

// -------------------------------------------------------------- the visible policy

#[test]
fn env_show_shows_the_store_only_policy() {
    let mut harness = Harness::new(vec![shipped_environments()], &[]);
    isolated_environment(&mut harness);
    let code = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap()
        .block_on(run_args(&mut harness, &["env", "show", "claude"]));
    assert_eq!(code, 0, "{}", harness.stderr.text());

    let stdout = harness.stdout.text();
    let line = stdout.lines().next().unwrap_or_default();
    assert!(line.starts_with("credential  "), "{stdout}");
    assert!(
        line.contains("add an entry to the p1 store"),
        "the guidance names p1's store: {line}"
    );
    assert!(
        line.contains("[p1 store only]"),
        "the policy is visible: {line}"
    );
    assert!(
        !line.contains("Claude Code login"),
        "a store-only route never names the borrowed login: {line}"
    );
}

#[test]
fn login_list_shows_the_store_only_policy() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, "kind = \"codex-oauth\"\nstore_only = true\n");
    let harness = scratch.harness();

    assert_eq!(p1_host::login::list(&harness.deps), 0);

    let listed = harness.stdout.text();
    let line = listed
        .lines()
        .find(|line| line.starts_with(ROUTE))
        .unwrap_or_else(|| panic!("no line for {ROUTE}: {listed}"));
    assert!(line.contains("codex-oauth"), "{listed}");
    assert!(line.ends_with("[p1 store only]"), "{listed}");
    assert!(
        !line.contains("codex login"),
        "a store-only route never offers the CLI login: {listed}"
    );
}

// -------------------------------------------------------------- a load error

#[test]
fn a_store_only_table_that_still_borrows_fails_the_load() {
    let scratch = Scratch::new();
    scratch.write_route(
        ROUTE,
        "kind = \"api-key\"\nenv = \"P1_KEY\"\nborrow = [\"pi:zai\"]\nstore_only = true\n",
    );
    let error = p1_host::routes::load_route(&scratch.route_path(ROUTE)).unwrap_err();
    assert!(
        error.contains("store_only") && error.contains("borrow"),
        "{error}"
    );
    assert!(
        error.contains(&format!("{ROUTE}.toml")),
        "the error names the file: {error}"
    );
}

// -------------------------------------------------------------- the login guidance

/// Echo control the usage-error path never reaches: `p1 login` refuses a non-API-key
/// route before it reads stdin or touches the terminal.
struct NoEcho;

impl EchoControl for NoEcho {
    fn disable(&self) -> Result<Guard, String> {
        Err("no terminal in this test".to_string())
    }
}

/// `p1 login <route>` on a store-only OAuth route: the credential is p1's own store's,
/// the CLI login is NOT the answer, and no OAuth browser flow is pretended. A legacy
/// OAuth route (no `store_only`) keeps its old "your login comes from the CLI" message.
#[tokio::test]
async fn login_on_a_store_only_oauth_route_never_sends_you_to_the_cli() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, "kind = \"claude-code-oauth\"\nstore_only = true\n");
    let harness = scratch.harness();

    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &NoEcho).await,
        2
    );
    let stderr = harness.stderr.text();
    assert!(stderr.contains("store_only"), "{stderr}");
    assert!(stderr.contains("p1's own store"), "{stderr}");
    assert!(stderr.contains("independent OAuth flow"), "{stderr}");
    assert!(stderr.contains("NOT read by this route"), "{stderr}");
    assert!(
        !stderr.contains("its login comes from"),
        "the old CLI-first guidance must be gone: {stderr}"
    );

    // The legacy chain keeps the old guidance and its test.
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, "kind = \"claude-code-oauth\"\n");
    let harness = scratch.harness();
    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &NoEcho).await,
        2
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("its login comes from") && stderr.contains("Claude Code CLI (`claude`)"),
        "{stderr}"
    );
}

// --------------------------------------------------- the shipped route, end to end

/// The runtime the catalog composes for the shipped Claude route: a token in p1's
/// store wins, and with the store entry gone the chain does NOT fall back to a
/// perfectly good Claude Code login file sitting right there.
#[tokio::test]
async fn the_shipped_claude_route_reads_only_p1_store() {
    let scratch = Scratch::new();
    let home = scratch.home();
    std::fs::create_dir_all(&home).unwrap();
    let cli_login = write_claude_login(&home, "FAKE-CLI-FILE-TOKEN");
    write_store(
        &home,
        &serde_json::json!({
            "anthropic-subscription": {
                "type": "oauth",
                "access": "FAKE-STORE-TOKEN",
                "refresh": "FAKE-STORE-REFRESH",
                "expires": 4_102_444_800_000u64,
            }
        })
        .to_string(),
    );

    let transport = Arc::new(ScriptedTransport::new(Vec::new()));
    let source = shipped_source("anthropic-subscription", &home, transport.clone());
    let credential = source.access().await.unwrap();
    assert_eq!(credential.bearer, "FAKE-STORE-TOKEN");
    assert!(
        transport.requests().is_empty(),
        "a fresh store token needs no refresh"
    );
    assert!(
        std::fs::read_to_string(&cli_login)
            .unwrap()
            .contains("FAKE-CLI-FILE-TOKEN"),
        "the store answer never touches the CLI login"
    );

    // The store entry gone: the chain must NOT fall through to the CLI login.
    std::fs::remove_file(store_path(&home)).unwrap();
    let source = shipped_source(
        "anthropic-subscription",
        &home,
        Arc::new(ScriptedTransport::new(Vec::new())),
    );
    let error = source.access().await.unwrap_err();
    assert!(
        error.message.contains("no credential source has an entry"),
        "{}",
        error.message
    );
    assert!(
        !error.message.contains("FAKE-CLI-FILE-TOKEN") && !error.message.contains("Claude Code"),
        "no CLI login was tried: {}",
        error.message
    );
}

/// The same for the shipped Codex route: an expired store entry refreshes through the
/// transport and lands back in p1's store, and the valid Codex CLI auth file next to it
/// is neither read for an answer nor written.
#[tokio::test]
async fn the_shipped_codex_route_refreshes_into_p1_store_not_the_cli_file() {
    let scratch = Scratch::new();
    let home = scratch.home();
    std::fs::create_dir_all(&home).unwrap();
    let cli_login = write_codex_login(&home);
    let cli_before = std::fs::read(&cli_login).unwrap();
    write_store(
        &home,
        &serde_json::json!({
            "openai-codex-subscription": {
                "type": "oauth",
                "access": "FAKE-OLD-ACCESS",
                "refresh": "FAKE-REFRESH-OLD",
                "expires": 1u64,
                "account_id": "FAKE-STORE-ACCOUNT",
            }
        })
        .to_string(),
    );

    let transport = Arc::new(ScriptedTransport::new(vec![ScriptedResponse {
        status: 200,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        chunks: vec![
            serde_json::to_vec(&serde_json::json!({
                "access_token": "FAKE-NEW-ACCESS",
                "refresh_token": "FAKE-REFRESH-NEW",
                "expires_in": 3600,
            }))
            .unwrap(),
        ],
        end: BodyEnd::Eof,
    }]));
    let source = shipped_source("openai-codex-subscription", &home, transport.clone());
    let credential = source.access().await.unwrap();
    assert_eq!(credential.bearer, "FAKE-NEW-ACCESS");
    assert_eq!(credential.account_id.as_deref(), Some("FAKE-STORE-ACCOUNT"));

    let requests = transport.requests();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].url, "https://auth.openai.com/oauth/token");

    let store: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(store_path(&home)).unwrap()).unwrap();
    assert_eq!(
        store["openai-codex-subscription"]["access"],
        "FAKE-NEW-ACCESS"
    );
    assert_eq!(
        store["openai-codex-subscription"]["refresh"],
        "FAKE-REFRESH-NEW"
    );
    assert_ne!(
        store["openai-codex-subscription"]["access"],
        "FAKE-CLI-ACCESS"
    );
    assert_eq!(
        std::fs::read(&cli_login).unwrap(),
        cli_before,
        "the Codex CLI auth file is never written by a store-only route"
    );
}
