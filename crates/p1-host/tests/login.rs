//! Must-pass of spec §6 through the host: `p1 login <route>`, `p1 login --list` and
//! `p1 logout <route>` (ADR-0044).
//!
//! Every route file is a scratch file, every home is a scratch home, every key is
//! obviously fake, and the terminal is a recording fake — no test here reads a real
//! login, touches the real home or needs a real TTY. The key is fed through the same
//! injected line source the interactive prompt loop reads stdin through.

mod common;

use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use common::Harness;
use p1_auth::CredentialSpec;
use p1_contracts::BoxFuture;
use p1_host::LineSource;
use p1_host::login::{EchoControl, Guard};
use p1_provider_http::testing::ScriptedTransport;
use serde_json::Value;

const ROUTE: &str = "test-route";
const OTHER: &str = "other-route";
const SENTINEL: &str = "FAKE-SENTINEL-KEY";

/// A scratch host: a routes directory with the route files a test writes, a scratch
/// home for p1's store and for the borrowed logins, and an empty environments
/// directory.
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

    fn environments(&self) -> PathBuf {
        self.dir.path().join("environments")
    }

    fn home(&self) -> PathBuf {
        self.dir.path().join("home")
    }

    fn store_path(&self) -> PathBuf {
        self.home().join(".config/p1/auth.json")
    }

    /// The store as it is on disk.
    fn store(&self) -> String {
        std::fs::read_to_string(self.store_path()).unwrap()
    }

    /// One route file whose `[credential]` table is `credential`.
    fn write_route(&self, id: &str, credential: &str) {
        let text = format!(
            "id           = \"{id}\"\n\
             origin_route = \"openai-chat/{id}\"\n\
             adapter      = \"openai-chat\"\n\
             endpoint     = \"https://example.invalid/v1/chat/completions\"\n\
             \n[credential]\n{credential}\n\
             \n[adapter_settings]\ndialect = \"retained-thinking\"\n"
        );
        std::fs::write(
            self.dir.path().join("routes").join(format!("{id}.toml")),
            text,
        )
        .unwrap();
    }

    /// A store file with `mode`, in a 0700 directory (the mode is the only thing a
    /// test here means to change).
    fn write_store(&self, text: &str, mode: u32) {
        let dir = self.home().join(".config/p1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::write(self.store_path(), text).unwrap();
        std::fs::set_permissions(self.store_path(), std::fs::Permissions::from_mode(mode)).unwrap();
    }

    /// A borrowed OpenCode login holding one key.
    fn write_opencode_login(&self, key: &str) {
        let dir = self.home().join(".local/share/opencode");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("auth.json"),
            format!(r#"{{"opencode-go":{{"type":"api","key":"{key}"}}}}"#),
        )
        .unwrap();
    }

    /// The host deps a test drives the login surface through, with `lines` as the
    /// injected stdin.
    fn harness(&self, lines: &[&str]) -> Harness {
        let mut harness = Harness::new(vec![self.environments()], lines);
        harness.deps.home = Some(self.home());
        // The host sees NO ambient environment: no credential path can point outside
        // the scratch directories this test wrote.
        harness.deps.shell_env = Some(Vec::new());
        harness
    }
}

/// An API-key route's `[credential]` table.
fn api_key() -> &'static str {
    "kind   = \"api-key\"\nenv    = \"P1_LOGIN_TEST_KEY\"\nborrow = [\"opencode:opencode-go\"]\n"
}

fn mode(path: &Path) -> u32 {
    std::fs::metadata(path).unwrap().permissions().mode() & 0o777
}

/// The recording fake of the terminal: what a real one would do, written down.
struct RecordingEcho {
    log: Arc<Mutex<Vec<String>>>,
}

impl RecordingEcho {
    fn new() -> Self {
        Self {
            log: Arc::new(Mutex::new(Vec::new())),
        }
    }

    fn log(&self) -> Vec<String> {
        self.log.lock().unwrap().clone()
    }
}

impl EchoControl for RecordingEcho {
    fn disable(&self) -> Result<Guard, String> {
        self.log.lock().unwrap().push("disable".to_string());
        let log = self.log.clone();
        Ok(Guard::from_restore(move || {
            log.lock().unwrap().push("restore".to_string());
        }))
    }
}

/// A terminal that refuses: the key must then not be read visibly.
struct BrokenEcho;

impl EchoControl for BrokenEcho {
    fn disable(&self) -> Result<Guard, String> {
        Err("`stty -echo` exited with 1".to_string())
    }
}

/// A line source that counts its reads, so a test can prove stdin was never touched.
struct CountingLines {
    reads: Arc<AtomicUsize>,
}

impl LineSource for CountingLines {
    fn next_line<'a>(&'a self) -> BoxFuture<'a, Option<String>> {
        self.reads.fetch_add(1, Ordering::SeqCst);
        Box::pin(async { Some("FAKE-NEVER-READ".to_string()) })
    }
}

// --------------------------------------------------------------- the piped path

#[tokio::test]
async fn login_on_a_fresh_home_stores_the_key_in_a_private_store() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&["FAKE-PIPED-KEY"]);
    let echo = RecordingEcho::new();

    let code = p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await;

    assert_eq!(code, 0);
    assert_eq!(
        harness.stdout.text(),
        format!("stored for {ROUTE} · source now: p1 store\n")
    );
    assert!(
        harness.stderr.text().is_empty(),
        "piped input needs no prompt: {}",
        harness.stderr.text()
    );
    assert!(
        echo.log().is_empty(),
        "no echo control without a terminal: {:?}",
        echo.log()
    );
    assert_eq!(mode(&scratch.home().join(".config/p1")), 0o700);
    assert_eq!(mode(&scratch.store_path()), 0o600);
    assert_eq!(
        serde_json::from_str::<Value>(&scratch.store()).unwrap(),
        serde_json::json!({ ROUTE: { "type": "api_key", "key": "FAKE-PIPED-KEY" } }),
        "exactly one entry, in the documented shape"
    );
}

#[tokio::test]
async fn a_tty_prompts_and_hides_the_key_for_the_read_only() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&["FAKE-TYPED-KEY"]);
    let echo = RecordingEcho::new();

    let code = p1_host::login::login_with(&harness.deps, ROUTE, true, &echo).await;

    assert_eq!(code, 0);
    assert_eq!(
        harness.stderr.text(),
        format!("key for {ROUTE} (input hidden): \n")
    );
    assert_eq!(
        echo.log(),
        ["disable", "restore"],
        "echo is switched off for the read and put back afterwards"
    );
    assert!(scratch.store().contains("FAKE-TYPED-KEY"));
}

/// The guard restores echo on the failure paths too: a terminal must never be left
/// without echo because a key was empty.
#[tokio::test]
async fn echo_is_restored_when_the_key_is_unusable_and_when_the_terminal_refuses() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&["  "]);
    let echo = RecordingEcho::new();

    let code = p1_host::login::login_with(&harness.deps, ROUTE, true, &echo).await;

    assert_eq!(code, 1);
    assert!(
        harness.stderr.text().contains("empty"),
        "{}",
        harness.stderr.text()
    );
    assert_eq!(echo.log(), ["disable", "restore"]);
    assert!(!scratch.store_path().exists(), "nothing was written");

    // A terminal where echo cannot be switched off: the key is not read at all.
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let reads = Arc::new(AtomicUsize::new(0));
    let mut harness = scratch.harness(&[]);
    harness.deps.lines = Arc::new(CountingLines {
        reads: reads.clone(),
    });

    let code = p1_host::login::login_with(&harness.deps, ROUTE, true, &BrokenEcho).await;

    assert_eq!(code, 1);
    assert!(
        harness.stderr.text().contains("stty -echo"),
        "{}",
        harness.stderr.text()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0, "the key was never read");
}

#[tokio::test]
async fn the_key_is_trimmed_and_an_empty_or_unusable_key_writes_nothing() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&["  FAKE-TRIMMED\t"]);
    let echo = RecordingEcho::new();

    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        0
    );
    assert_eq!(
        serde_json::from_str::<Value>(&scratch.store()).unwrap()[ROUTE]["key"],
        "FAKE-TRIMMED",
        "surrounding whitespace is trimmed"
    );

    // A key with a space inside is not a header-safe token.
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&["FAKE KEY"]);
    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        1
    );
    assert!(
        harness.stderr.text().contains("printable ASCII"),
        "{}",
        harness.stderr.text()
    );
    assert!(!scratch.store_path().exists());

    // stdin that ends without a line: nothing to store.
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&[]);
    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        1
    );
    assert!(
        harness.stderr.text().contains("no key was read"),
        "{}",
        harness.stderr.text()
    );
    assert!(!scratch.store_path().exists());
}

// --------------------------------------------------------------- usage errors

#[tokio::test]
async fn a_wide_store_is_refused_before_the_key_is_read() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    scratch.write_store("{}", 0o644);
    let reads = Arc::new(AtomicUsize::new(0));
    let mut harness = scratch.harness(&[]);
    harness.deps.lines = Arc::new(CountingLines {
        reads: reads.clone(),
    });
    let echo = RecordingEcho::new();

    let code = p1_host::login::login_with(&harness.deps, ROUTE, true, &echo).await;

    assert_eq!(code, 1);
    assert!(
        harness.stderr.text().contains("chmod 600"),
        "{}",
        harness.stderr.text()
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0, "the key was never read");
    assert!(echo.log().is_empty(), "echo was never switched off");
    assert_eq!(scratch.store(), "{}", "the store is left alone");
}

#[tokio::test]
async fn an_unknown_route_is_a_usage_error_listing_the_routes() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    scratch.write_route(OTHER, api_key());
    let reads = Arc::new(AtomicUsize::new(0));
    let mut harness = scratch.harness(&[]);
    harness.deps.lines = Arc::new(CountingLines {
        reads: reads.clone(),
    });
    let echo = RecordingEcho::new();

    let code = p1_host::login::login_with(&harness.deps, "nope", false, &echo).await;

    assert_eq!(code, 2);
    let stderr = harness.stderr.text();
    assert!(stderr.contains("route `nope` was not found"), "{stderr}");
    assert!(
        stderr.contains(ROUTE) && stderr.contains(OTHER),
        "the error lists the routes: {stderr}"
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0, "the key was never read");
    assert!(!scratch.store_path().exists());

    // logout names the same problem the same way.
    assert_eq!(
        p1_host::login::logout(&harness.deps, "nope").await,
        2,
        "stderr: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn a_route_whose_login_is_borrowed_is_a_usage_error_that_names_its_cli() {
    let scratch = Scratch::new();
    scratch.write_route("claude-route", "kind = \"claude-code-oauth\"\n");
    scratch.write_route("codex-route", "kind = \"codex-oauth\"\n");
    let reads = Arc::new(AtomicUsize::new(0));
    let mut harness = scratch.harness(&[]);
    harness.deps.lines = Arc::new(CountingLines {
        reads: reads.clone(),
    });
    let echo = RecordingEcho::new();

    assert_eq!(
        p1_host::login::login_with(&harness.deps, "claude-route", false, &echo).await,
        2
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("claude-code-oauth") && stderr.contains("Claude Code CLI (`claude`)"),
        "the error says where that login comes from: {stderr}"
    );

    assert_eq!(
        p1_host::login::login_with(&harness.deps, "codex-route", false, &echo).await,
        2
    );
    assert!(
        harness.stderr.text().contains("Codex CLI (`codex login`)"),
        "{}",
        harness.stderr.text()
    );

    assert_eq!(
        p1_host::login::logout(&harness.deps, "codex-route").await,
        2
    );
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no key was ever read");
    assert!(!scratch.store_path().exists());
}

// -------------------------------------------------------------------- --list

#[tokio::test]
async fn login_list_names_every_route_its_kind_and_its_source() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    scratch.write_route("codex-route", "kind = \"codex-oauth\"\n");
    scratch.write_opencode_login("FAKE-OPENCODE");
    let mut harness = scratch.harness(&["FAKE-STORED"]);
    let echo = RecordingEcho::new();

    assert_eq!(p1_host::login::list(&harness.deps), 0);
    let listed = harness.stdout.text();
    let lines: Vec<&str> = listed.lines().collect();
    assert_eq!(lines.len(), 2, "{listed}");
    assert!(
        lines[0].starts_with("codex-route") && lines[0].contains("codex-oauth"),
        "{listed}"
    );
    assert!(lines[0].ends_with("run `codex login`"), "{listed}");
    assert!(
        lines[1].starts_with(ROUTE) && lines[1].contains("api-key"),
        "{listed}"
    );
    assert!(lines[1].ends_with("opencode login"), "{listed}");

    // After a login the same route reports p1's store; an environment variable still
    // overrides it and the line says so.
    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        0
    );
    let before = harness.stdout.text().len();
    assert_eq!(p1_host::login::list(&harness.deps), 0);
    let listed = harness.stdout.text()[before..].to_string();
    let line = listed
        .lines()
        .find(|line| line.starts_with(ROUTE))
        .unwrap_or_else(|| panic!("no line for {ROUTE}: {listed}"));
    assert!(line.ends_with("p1 store"), "{listed}");

    harness.deps.shell_env = Some(vec![("P1_LOGIN_TEST_KEY".into(), "FAKE-ENV".into())]);
    let before = harness.stdout.text().len();
    assert_eq!(p1_host::login::list(&harness.deps), 0);
    let listed = harness.stdout.text()[before..].to_string();
    let line = listed
        .lines()
        .find(|line| line.starts_with(ROUTE))
        .unwrap_or_else(|| panic!("no line for {ROUTE}: {listed}"));
    assert!(line.ends_with("env P1_LOGIN_TEST_KEY"), "{listed}");
}

/// Issue #134: a route that sends no credential is listed as such, and there is
/// nothing `p1 login` could store for it — the egress proxy injects the credential.
#[tokio::test]
async fn a_none_route_is_listed_as_proxy_injected_and_cannot_be_logged_in() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, "kind = \"none\"\n");
    let reads = Arc::new(AtomicUsize::new(0));
    let mut harness = scratch.harness(&[]);
    harness.deps.lines = Arc::new(CountingLines {
        reads: reads.clone(),
    });
    let echo = RecordingEcho::new();

    assert_eq!(p1_host::login::list(&harness.deps), 0);
    let listed = harness.stdout.text();
    let line = listed
        .lines()
        .find(|line| line.starts_with(ROUTE))
        .unwrap_or_else(|| panic!("no line for {ROUTE}: {listed}"));
    assert!(line.contains("none (proxy-injected)"), "{listed}");
    assert!(line.contains("egress proxy"), "{listed}");
    assert!(
        !line.contains("api-key") && !line.contains("oauth"),
        "the kind is not one this route resolves: {listed}"
    );

    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        2
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("kind = \"none\"") && stderr.contains("egress proxy"),
        "{stderr}"
    );
    assert_eq!(p1_host::login::logout(&harness.deps, ROUTE).await, 2);
    assert_eq!(reads.load(Ordering::SeqCst), 0, "no key was ever read");
    assert!(!scratch.store_path().exists());
}

// -------------------------------------------------- --from-claude-code (ADR-0074)

/// A Claude Code login file under the scratch home, holding the sentinel tokens.
fn write_claude_login(scratch: &Scratch, relative_dir: &str) -> PathBuf {
    let dir = scratch.home().join(relative_dir);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join(".credentials.json"),
        format!(
            r#"{{"claudeAiOauth":{{"accessToken":"{SENTINEL}","refreshToken":"{SENTINEL}-REFRESH","expiresAt":4102444800000}}}}"#
        ),
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn from_claude_code_imports_the_routes_login_dir_into_a_private_store() {
    let scratch = Scratch::new();
    scratch.write_route(
        "claude-route-2",
        "kind      = \"claude-code-oauth\"\nlogin_dir = \"~/.claude-2\"\n",
    );
    let dir = write_claude_login(&scratch, ".claude-2");
    let harness = scratch.harness(&[]);

    let code = p1_host::login::from_claude_code(&harness.deps, "claude-route-2", None).await;

    assert_eq!(code, 0, "{}", harness.stderr.text());
    assert_eq!(
        harness.stdout.text(),
        format!(
            "imported the Claude Code login in {} for claude-route-2 · source now: p1 store\n\
             this p1 store entry wins over any Claude Code login the route borrows until \
             `p1 logout claude-route-2` removes it\n",
            dir.display()
        )
    );
    assert_eq!(mode(&scratch.home().join(".config/p1")), 0o700);
    assert_eq!(mode(&scratch.store_path()), 0o600);
    assert_eq!(
        serde_json::from_str::<Value>(&scratch.store()).unwrap(),
        serde_json::json!({ "claude-route-2": {
            "type": "oauth",
            "access": SENTINEL,
            "refresh": format!("{SENTINEL}-REFRESH"),
            "expires": 4_102_444_800_000u64,
            "account_id": null,
        } }),
        "exactly the store's oauth shape"
    );
    let said = format!("{}{}", harness.stdout.text(), harness.stderr.text());
    assert!(!said.contains(SENTINEL), "a token was printed: {said}");

    // An explicit directory wins over the route's own, and a leading `~` is expanded.
    let scratch = Scratch::new();
    scratch.write_route(
        "claude-route",
        "kind = \"claude-code-oauth\"\nstore_only = true\n",
    );
    write_claude_login(&scratch, "elsewhere");
    let harness = scratch.harness(&[]);
    assert_eq!(
        p1_host::login::from_claude_code(&harness.deps, "claude-route", Some("~/elsewhere")).await,
        0,
        "{}",
        harness.stderr.text()
    );
    assert_eq!(
        serde_json::from_str::<Value>(&scratch.store()).unwrap()["claude-route"]["access"],
        SENTINEL
    );
    assert!(!harness.stdout.text().contains(SENTINEL));
}

#[tokio::test]
async fn from_claude_code_refuses_a_non_oauth_route_and_a_missing_login() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    scratch.write_route("codex-route", "kind = \"codex-oauth\"\n");
    scratch.write_route("claude-route", "kind = \"claude-code-oauth\"\n");
    write_claude_login(&scratch, ".claude");
    let harness = scratch.harness(&[]);

    for route in [ROUTE, "codex-route"] {
        assert_eq!(
            p1_host::login::from_claude_code(&harness.deps, route, None).await,
            2,
            "{route}"
        );
    }
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(&format!("route `{ROUTE}` is a api-key route"))
            && stderr.contains("route `codex-route` is a codex-oauth route")
            && stderr.contains("only a claude-code-oauth route reads"),
        "{stderr}"
    );

    // A directory with no login names the file and how to log in there.
    assert_eq!(
        p1_host::login::from_claude_code(&harness.deps, "claude-route", Some("~/nowhere")).await,
        2
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("no Claude Code login at")
            && stderr.contains("nowhere/.credentials.json")
            && stderr.contains("CLAUDE_CONFIG_DIR="),
        "{stderr}"
    );
    assert!(!scratch.store_path().exists(), "nothing was written");
    assert!(!stderr.contains(SENTINEL), "{stderr}");

    // The plain form on a Claude route now points at the import.
    assert_eq!(
        p1_host::login::login_with(&harness.deps, "claude-route", false, &RecordingEcho::new())
            .await,
        2
    );
    assert!(
        harness
            .stderr
            .text()
            .contains("`p1 login claude-route --from-claude-code [DIR]`"),
        "{}",
        harness.stderr.text()
    );
}

/// ADR-0074 review: an imported copy shadows the live login, so there must be a way back.
/// `p1 logout` removes an OAuth route's entry exactly like an API key's, every other entry
/// stays byte for byte, and the route borrows its Claude Code login again.
#[tokio::test]
async fn logout_removes_an_imported_oauth_entry_and_the_live_login_answers_again() {
    let scratch = Scratch::new();
    scratch.write_route(
        "claude-route-2",
        "kind      = \"claude-code-oauth\"\nlogin_dir = \"~/.claude-2\"\n",
    );
    scratch.write_route("codex-route", "kind = \"codex-oauth\"\n");
    write_claude_login(&scratch, ".claude-2");
    let others = serde_json::json!({
        "codex-route": {"type": "oauth", "access": "FAKE-CODEX", "refresh": null,
                        "expires": null, "account_id": "fake-acct", "extra": [1, 2]},
        "other-route": {"type": "api_key", "key": "FAKE-OTHER"},
    });
    let pretty = |value: &Value| format!("{}\n", serde_json::to_string_pretty(value).unwrap());
    scratch.write_store(&pretty(&others), 0o600);
    let harness = scratch.harness(&[]);

    assert_eq!(
        p1_host::login::from_claude_code(&harness.deps, "claude-route-2", None).await,
        0,
        "{}",
        harness.stderr.text()
    );
    let route = p1_host::routes::load_route_by_id(&harness.deps.environment_dirs, "claude-route-2")
        .unwrap();
    let locations = p1_auth::Locations::none().with_home(Some(scratch.home()));
    assert_eq!(
        p1_auth::describe("claude-route-2", &route.credential, &locations).chosen,
        Some(p1_auth::SourceName::P1Store),
        "the imported copy wins over the live login"
    );

    assert_eq!(
        p1_host::login::logout(&harness.deps, "claude-route-2").await,
        0,
        "{}",
        harness.stderr.text()
    );
    assert!(
        harness
            .stdout
            .text()
            .ends_with("removed claude-route-2 from p1's store\n"),
        "{}",
        harness.stdout.text()
    );
    assert_eq!(
        scratch.store(),
        pretty(&others),
        "every other entry is byte-identical"
    );
    assert_eq!(mode(&scratch.store_path()), 0o600);
    assert_eq!(
        p1_auth::describe("claude-route-2", &route.credential, &locations).chosen,
        Some(p1_auth::SourceName::ClaudeCodeLogin),
        "the borrowed login answers again"
    );

    // A codex-oauth route's entry is removable too.
    assert_eq!(
        p1_host::login::logout(&harness.deps, "codex-route").await,
        0
    );
    assert_eq!(
        scratch.store(),
        pretty(&serde_json::json!({"other-route": {"type": "api_key", "key": "FAKE-OTHER"}}))
    );
}

/// An OAuth route with no p1 store entry: nothing to remove, and its login is the CLI's,
/// which `p1 logout` must not pretend to touch — a usage error.
#[tokio::test]
async fn logout_of_an_oauth_route_with_no_entry_is_a_usage_error() {
    let scratch = Scratch::new();
    scratch.write_route("claude-route", "kind = \"claude-code-oauth\"\n");
    scratch.write_store(
        "{\"other-route\":{\"type\":\"api_key\",\"key\":\"K\"}}",
        0o600,
    );
    let harness = scratch.harness(&[]);

    assert_eq!(
        p1_host::login::logout(&harness.deps, "claude-route").await,
        2
    );
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains("no claude-route entry in p1's store")
            && stderr.contains("Claude Code CLI (`claude`)"),
        "{stderr}"
    );
    assert_eq!(
        scratch.store(),
        "{\"other-route\":{\"type\":\"api_key\",\"key\":\"K\"}}",
        "the store is left alone"
    );
}

/// Importing twice replaces the route's entry; it never adds a second one.
#[tokio::test]
async fn from_claude_code_twice_replaces_the_entry() {
    let scratch = Scratch::new();
    scratch.write_route("claude-route", "kind = \"claude-code-oauth\"\n");
    write_claude_login(&scratch, "first");
    let second = scratch.home().join("second");
    std::fs::create_dir_all(&second).unwrap();
    std::fs::write(
        second.join(".credentials.json"),
        r#"{"claudeAiOauth":{"accessToken":"FAKE-SECOND","refreshToken":"FAKE-SECOND-R"}}"#,
    )
    .unwrap();
    let harness = scratch.harness(&[]);

    for dir in ["~/first", "~/second"] {
        assert_eq!(
            p1_host::login::from_claude_code(&harness.deps, "claude-route", Some(dir)).await,
            0,
            "{}",
            harness.stderr.text()
        );
    }
    let store: Value = serde_json::from_str(&scratch.store()).unwrap();
    assert_eq!(
        store,
        serde_json::json!({ "claude-route": {
            "type": "oauth",
            "access": "FAKE-SECOND",
            "refresh": "FAKE-SECOND-R",
            "expires": null,
            "account_id": null,
        } }),
        "one entry, the second login's"
    );
    assert_eq!(scratch.store().matches("claude-route").count(), 1);
}

// -------------------------------------------------------------------- logout

#[tokio::test]
async fn logout_removes_the_entry_and_reports_one_that_is_not_there() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&["FAKE-STORED"]);
    let echo = RecordingEcho::new();

    // Nothing stored yet: reported, not an error, and nothing is created.
    assert_eq!(p1_host::login::logout(&harness.deps, ROUTE).await, 0);
    assert_eq!(
        harness.stdout.text(),
        format!("no {ROUTE} entry in p1's store\n")
    );
    assert!(
        !scratch.home().join(".config").exists(),
        "logout creates no store for a route that has none"
    );

    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        0
    );
    assert_eq!(p1_host::login::logout(&harness.deps, ROUTE).await, 0);
    assert!(
        harness
            .stdout
            .text()
            .contains(&format!("removed {ROUTE} from p1's store")),
        "{}",
        harness.stdout.text()
    );
    assert_eq!(
        scratch.store(),
        "{}\n",
        "an empty object stays a valid file"
    );
    assert_eq!(mode(&scratch.store_path()), 0o600);
}

/// The whole point of the store: after `login`, the chain the host composes for that
/// route yields the stored key; a documented variable still wins; after `logout` the
/// chain falls through to the borrowed login.
#[tokio::test]
async fn resolve_yields_the_stored_key_an_environment_variable_still_wins_and_logout_falls_through()
{
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    scratch.write_opencode_login("FAKE-OPENCODE");
    let mut harness = scratch.harness(&["FAKE-STORED", "FAKE-STORED-2"]);
    let echo = RecordingEcho::new();

    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        0
    );
    assert!(
        harness.stdout.text().ends_with("· source now: p1 store\n"),
        "{}",
        harness.stdout.text()
    );

    let route = p1_host::routes::load_route_by_id(&harness.deps.environment_dirs, ROUTE).unwrap();
    let locations = p1_auth::Locations::none().with_home(Some(scratch.home()));
    // The same locations, plus the documented variable — as the host composes them.
    let with_env = locations
        .clone()
        .with_env_lookup(|name| (name == "P1_LOGIN_TEST_KEY").then(|| "FAKE-ENV".to_string()));
    let source = |locations: &p1_auth::Locations| {
        p1_auth::resolve(
            ROUTE,
            &route.credential,
            Arc::new(ScriptedTransport::new(Vec::new())),
            locations,
        )
    };
    assert_eq!(
        source(&locations).access().await.unwrap().bearer,
        "FAKE-STORED"
    );

    // A variable that still overrides the store: login says so.
    harness.deps.shell_env = Some(vec![("P1_LOGIN_TEST_KEY".into(), "FAKE-ENV".into())]);
    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        0
    );
    assert!(
        harness
            .stdout
            .text()
            .ends_with("· source now: env P1_LOGIN_TEST_KEY\n"),
        "{}",
        harness.stdout.text()
    );
    assert_eq!(
        source(&with_env).access().await.unwrap().bearer,
        "FAKE-ENV",
        "an environment variable still wins over the store"
    );

    harness.deps.shell_env = Some(Vec::new());
    assert_eq!(p1_host::login::logout(&harness.deps, ROUTE).await, 0);
    assert_eq!(
        source(&locations).access().await.unwrap().bearer,
        "FAKE-OPENCODE",
        "logout falls through to the borrowed login"
    );
}

// ------------------------------------------------------------- the sentinel

/// Must-pass of §6: the key appears in the store file and NOWHERE else — not in
/// stdout, not in stderr, not in an error's `Display` or `Debug`, not in a report.
#[tokio::test]
async fn the_key_never_appears_outside_the_store_file() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let harness = scratch.harness(&[SENTINEL, SENTINEL]);
    let echo = RecordingEcho::new();

    assert_eq!(
        p1_host::login::login_with(&harness.deps, ROUTE, false, &echo).await,
        0
    );
    assert!(
        scratch.store().contains(SENTINEL),
        "the sentinel is in the store file"
    );

    assert_eq!(p1_host::login::list(&harness.deps), 0);

    // Every error and every report the write path can produce, `Display` and `Debug`.
    let locations = p1_auth::Locations::none().with_home(Some(scratch.home()));
    let mut seen = format!("{}{}", harness.stdout.text(), harness.stderr.text());
    scratch.write_store(&scratch.store(), 0o644);
    let error = p1_auth::store::put_api_key(ROUTE, SENTINEL, &locations)
        .await
        .unwrap_err();
    seen.push_str(&format!("{error} {error:?}"));
    let error = p1_auth::store::put_api_key(ROUTE, "", &locations)
        .await
        .unwrap_err();
    seen.push_str(&format!("{error} {error:?}"));
    let error = p1_auth::store::put_api_key(ROUTE, SENTINEL, &p1_auth::Locations::none())
        .await
        .unwrap_err();
    seen.push_str(&format!("{error} {error:?}"));
    let error = p1_auth::store::remove(ROUTE, &locations).await.unwrap_err();
    seen.push_str(&format!("{error} {error:?}"));
    let spec: CredentialSpec =
        serde_json::from_str(r#"{"kind":"api-key","env":"P1_LOGIN_TEST_KEY"}"#).unwrap();
    let report = p1_auth::describe(ROUTE, &spec, &locations);
    seen.push_str(&format!("{report:?} {} ", report.line()));
    seen.push_str(&format!("{:?}", report.tried));
    assert!(!seen.contains(SENTINEL), "the key leaked: {seen}");

    scratch.write_store(&scratch.store(), 0o600);
    assert_eq!(p1_host::login::logout(&harness.deps, ROUTE).await, 0);
    let after = format!("{}{}", harness.stdout.text(), harness.stderr.text());
    assert!(!after.contains(SENTINEL), "logout leaked the key: {after}");
    assert!(!scratch.store().contains(SENTINEL));
}

/// The command line itself never takes the key as an argument (it would land in shell
/// history and in `ps`): the login surface is a route, a flag and a route.
#[test]
fn the_key_is_never_an_argument() {
    let args =
        |parts: &[&str]| -> Vec<String> { parts.iter().map(|part| part.to_string()).collect() };
    assert!(p1_host::cli::parse(&args(&["login"])).is_err());
    assert!(p1_host::cli::parse(&args(&["login", "--list"])).is_ok());
    assert!(p1_host::cli::parse(&args(&["login", ROUTE])).is_ok());
    assert!(p1_host::cli::parse(&args(&["logout", ROUTE])).is_ok());
    let usage = p1_host::cli::usage();
    assert!(usage.contains("p1 login <route>"), "{usage}");
    assert!(usage.contains("p1 logout <route>"), "{usage}");
}

/// The whole binary, end to end: a piped key is stored, the process contract holds,
/// and the key never reaches stdout or stderr. The child's `HOME` is the scratch
/// home, so it cannot reach the real store.
#[test]
fn the_binary_stores_a_piped_key_and_never_prints_it() {
    let scratch = Scratch::new();
    scratch.write_route(ROUTE, api_key());
    let run = |args: &[&str], stdin: &str| -> std::process::Output {
        use std::io::Write;
        let mut child = std::process::Command::new(env!("CARGO_BIN_EXE_p1"))
            .args(args)
            .env("HOME", scratch.home())
            .env("P1_CONFIG_DIR", scratch.dir.path().join("config"))
            .env("P1_ENVIRONMENTS_DIR", scratch.environments())
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_DATA_HOME")
            .env_remove("PI_CODING_AGENT_DIR")
            .env_remove("CLAUDE_CONFIG_DIR")
            .env_remove("CODEX_HOME")
            .env_remove("P1_LOGIN_TEST_KEY")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(stdin.as_bytes())
            .unwrap();
        child.wait_with_output().unwrap()
    };

    let output = run(&["login", ROUTE], &format!("{SENTINEL}\n"));
    assert!(
        output.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert_eq!(
        stdout,
        format!("stored for {ROUTE} · source now: p1 store\n")
    );
    assert!(scratch.store().contains(SENTINEL));
    assert!(!stdout.contains(SENTINEL), "{stdout}");
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SENTINEL));

    let output = run(&["login", "--list"], "");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    assert!(
        stdout.contains("api-key") && stdout.contains("p1 store"),
        "{stdout}"
    );
    assert!(!stdout.contains(SENTINEL), "{stdout}");

    let output = run(&["logout", "nope"], "");
    assert_eq!(output.status.code(), Some(2));
    assert!(!String::from_utf8_lossy(&output.stderr).contains(SENTINEL));

    let output = run(&["logout", ROUTE], "");
    assert!(output.status.success());
    assert_eq!(scratch.store(), "{}\n");

    // ADR-0074: the import through the binary — the default directory is the scratch
    // home's `~/.claude`, and neither stream carries the token.
    scratch.write_route("claude-route", "kind = \"claude-code-oauth\"\n");
    write_claude_login(&scratch, ".claude");
    let output = run(&["login", "claude-route", "--from-claude-code"], "");
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    assert!(output.status.success(), "stderr: {stderr}");
    assert!(
        stdout.contains("imported the Claude Code login"),
        "{stdout}"
    );
    assert!(scratch.store().contains(SENTINEL));
    assert!(!stdout.contains(SENTINEL) && !stderr.contains(SENTINEL));
}
