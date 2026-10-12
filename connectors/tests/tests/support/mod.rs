//! Scratch locations and fake credential files for the connectors' logins in p1's credential
//! chain, and the four logins as the credential modules the chain resolves (ADR-0212): the same
//! functions the components export, called natively.
//!
//! Every path the chain looks at is built from a scratch home or an explicit variable table,
//! so no test here can reach the real home or the real environment.

#![allow(dead_code)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_auth::{CredentialModule, CredentialModules, Locations, ModuleMethod};
use p1_connector_logins::{ClaudeCode, Codex, Login, Opencode, Pi};
use p1_contracts::BoxFuture;
use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};
use p1_provider_http::{HttpRequest, HttpResponse, Transport, TransportError};
use tokio::sync::Notify;

/// A scratch home. Nothing outside it exists for a test that uses it.
pub struct Scratch {
    root: tempfile::TempDir,
}

impl Scratch {
    pub fn new() -> Self {
        Self {
            root: tempfile::tempdir().unwrap(),
        }
    }

    pub fn home(&self) -> PathBuf {
        self.root.path().to_path_buf()
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.path().join(relative)
    }

    /// The locations of this scratch home, with nothing in the environment and the
    /// connectors' four logins installed.
    pub fn locations(&self) -> Locations {
        connectors().with_home(Some(self.home()))
    }

    /// Write a file 0600, creating its parents 0700 (the modes p1 writes).
    pub fn write(&self, relative: &str, contents: &str) -> PathBuf {
        let path = self.path(relative);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
            set_mode(parent, 0o700);
        }
        write_mode(&path, contents, 0o600);
        path
    }

    pub fn read(&self, relative: &str) -> String {
        std::fs::read_to_string(self.path(relative)).unwrap()
    }

    /// Change the mode of a path this scratch already holds.
    pub fn set_mode(&self, relative: &str, mode: u32) {
        set_mode(&self.path(relative), mode);
    }

    /// Everything from the first occurrence of `marker` on, for a byte-compare of
    /// the part a refresh must not touch.
    pub fn tail_from(&self, relative: &str, marker: &str) -> String {
        let text = self.read(relative);
        let at = text
            .find(marker)
            .unwrap_or_else(|| panic!("{relative} holds no {marker}"));
        text[at..].to_string()
    }
}

pub fn set_mode(path: &Path, mode: u32) {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
}

pub fn write_mode(path: &Path, contents: &str, mode: u32) {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true).mode(mode);
    let mut file = options.open(path).unwrap();
    file.write_all(contents.as_bytes()).unwrap();
}

/// An environment a test controls: `set` makes a variable appear for the next
/// lookup, `clear` takes it away again. Nothing here touches the process one.
#[derive(Clone)]
pub struct FakeEnv {
    vars: Arc<Mutex<HashMap<String, String>>>,
}

impl FakeEnv {
    pub fn new() -> Self {
        Self {
            vars: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub fn set(&self, name: &str, value: &str) {
        self.vars
            .lock()
            .unwrap()
            .insert(name.to_string(), value.to_string());
    }

    pub fn clear(&self, name: &str) {
        self.vars.lock().unwrap().remove(name);
    }

    /// The scratch home with this variable table as the WHOLE environment.
    pub fn locations(&self, scratch: &Scratch) -> Locations {
        let vars = self.vars.clone();
        scratch
            .locations()
            .with_env_lookup(move |name| vars.lock().unwrap().get(name).cloned())
    }
}

/// One borrowed login's entry, as the two stores write it.
pub fn login(store: &str, key: &str, value: &str) -> String {
    let kind = if store == "opencode" {
        "api"
    } else {
        "api_key"
    };
    serde_json::json!({ key: { "type": kind, "key": value } }).to_string()
}

/// A `claude-code-oauth` login file, fresh unless `expires_at` says otherwise.
pub fn claude_login(access: &str, refresh: &str, expires_at: u64) -> String {
    serde_json::to_string_pretty(&serde_json::json!({
        "claudeAiOauth": {
            "accessToken": access,
            "refreshToken": refresh,
            "expiresAt": expires_at,
        }
    }))
    .unwrap()
}

/// A far-future millisecond timestamp: a token that needs no refresh.
pub const NEVER_EXPIRES_MS: u64 = 4_102_444_800_000;
/// A millisecond timestamp in the past: a token that must be refreshed.
pub const LONG_EXPIRED_MS: u64 = 1;

/// A JSON document in exactly the layout p1 writes: pretty, and newline-terminated.
/// A file written this way is byte-identical to its own rewrite except where a
/// refresh changed a value, which is what the write-back tests compare.
pub fn document(value: &serde_json::Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap();
    text.push('\n');
    text
}

/// One successful token-refresh response with this JSON body.
pub fn token_response(body: serde_json::Value) -> ScriptedResponse {
    ScriptedResponse {
        status: 200,
        headers: Vec::new(),
        chunks: vec![body.to_string().into_bytes()],
        end: BodyEnd::Eof,
    }
}

/// Holds every request until the test opens the gate, and says when one arrives:
/// the test can change the world while a refresh request is out.
#[derive(Clone)]
pub struct Gated {
    pub inner: ScriptedTransport,
    pub entered: Arc<Notify>,
    pub gate: Arc<Notify>,
}

impl Gated {
    pub fn new(inner: ScriptedTransport) -> Self {
        Self {
            inner,
            entered: Arc::new(Notify::new()),
            gate: Arc::new(Notify::new()),
        }
    }
}

impl Transport for Gated {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.gate.notified().await;
            self.inner.post(request).await
        })
    }
}

/// Let spawned tasks run until `done` holds (explicit synchronization: no clock).
pub async fn settle(done: impl Fn() -> bool) {
    for _ in 0..10_000 {
        if done() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("the background work never finished");
}

/// One login as a credential module: its functions called natively, with the token origins
/// its component's manifest declares (`modules/p1-module-*/Cargo.toml`).
pub struct Native {
    login: Box<dyn Login + Send + Sync>,
    origins: Vec<String>,
}

impl CredentialModule for Native {
    fn describe(&self) -> Result<String, String> {
        Ok(self.login.describe())
    }

    fn read_login(&self, file: &[u8], key: Option<&str>) -> Result<String, String> {
        self.login.read_login(file, key)
    }

    fn refresh_request(&self, refresh_token: &str, file: Option<&[u8]>) -> Result<String, String> {
        self.login.refresh_request(refresh_token, file)
    }

    fn refresh_response(&self, body: &[u8], sent_at_ms: u64) -> Result<String, String> {
        Ok(self.login.refresh_response(body, sent_at_ms))
    }

    fn merge_login(
        &self,
        file: &[u8],
        body: &[u8],
        sent_at_ms: u64,
        now_ms: u64,
        keep_access: bool,
    ) -> Result<Vec<u8>, String> {
        self.login
            .merge_login(file, body, sent_at_ms, now_ms, keep_access)
    }

    fn token_origins(&self) -> &[String] {
        &self.origins
    }
}

/// The module installed as `name`, as the release installs it.
pub fn native(name: &str) -> Option<Native> {
    let (login, origins): (Box<dyn Login + Send + Sync>, &[&str]) = match name {
        "claude-code-oauth" => (Box::new(ClaudeCode), &["https://platform.claude.com"]),
        "codex-oauth" => (Box::new(Codex), &["https://auth.openai.com"]),
        "opencode" => (Box::new(Opencode), &[]),
        "pi" => (Box::new(Pi), &[]),
        _ => return None,
    };
    Some(Native {
        login,
        origins: origins.iter().map(|origin| origin.to_string()).collect(),
    })
}

/// The connectors' four logins as an installed composition.
pub struct Connectors;

impl CredentialModules for Connectors {
    fn names(&self) -> Vec<String> {
        ["claude-code-oauth", "codex-oauth", "opencode", "pi"]
            .map(str::to_string)
            .to_vec()
    }

    fn module(&self, name: &str) -> Result<Option<Arc<dyn CredentialModule>>, String> {
        Ok(native(name).map(|module| Arc::new(module) as Arc<dyn CredentialModule>))
    }
}

/// Locations with nothing in them but the connectors' logins.
pub fn connectors() -> Locations {
    Locations::none().with_modules(Arc::new(Connectors))
}

/// The module installed as `name`, described.
pub fn method(name: &str) -> Arc<ModuleMethod> {
    Arc::new(
        ModuleMethod::new(name, Arc::new(native(name).expect("a connector login")))
            .expect("the connector describes itself"),
    )
}

/// The `claude-code-oauth` method's kind.
pub fn claude_code_oauth() -> p1_auth::CredentialKind {
    p1_auth::CredentialKind::module("claude-code-oauth")
}

/// The `codex-oauth` method's kind.
pub fn codex_oauth() -> p1_auth::CredentialKind {
    p1_auth::CredentialKind::module("codex-oauth")
}

/// Claude Code's login at `path` as a credential source.
pub fn claude_code_at(path: PathBuf, transport: Arc<dyn Transport>) -> p1_auth::LoginFile {
    p1_auth::LoginFile::at(method("claude-code-oauth"), path, transport)
}

/// The Codex CLI's login at `path` as a credential source.
pub fn codex_at(path: PathBuf, transport: Arc<dyn Transport>) -> p1_auth::LoginFile {
    p1_auth::LoginFile::at(method("codex-oauth"), path, transport)
}

/// One entry of OpenCode's (`opencode_format`) or Pi's keyed login at `path`.
pub fn key_file(path: PathBuf, key: &str, opencode_format: bool) -> p1_auth::KeyFile {
    let store = if opencode_format { "opencode" } else { "pi" };
    p1_auth::KeyFile::at(method(store), path, key)
}

/// The Claude Code login as a source.
pub fn claude_code_login() -> p1_auth::SourceName {
    p1_auth::SourceName::Login {
        name: "Claude Code login".into(),
        guidance: "run Claude Code login".into(),
    }
}

/// The Codex CLI's login as a source.
pub fn codex_login() -> p1_auth::SourceName {
    p1_auth::SourceName::Login {
        name: "Codex login".into(),
        guidance: "run `codex login`".into(),
    }
}

/// OpenCode's keyed login as a source.
pub fn opencode_login() -> p1_auth::SourceName {
    p1_auth::SourceName::Login {
        name: "opencode login".into(),
        guidance: "log in with opencode".into(),
    }
}

/// Pi's keyed login as a source.
pub fn pi_login() -> p1_auth::SourceName {
    p1_auth::SourceName::Login {
        name: "pi login".into(),
        guidance: "log in with pi".into(),
    }
}

/// `p1 login <route> --import DIR` for a `claude-code-oauth` route: the Claude Code login in
/// `dir` copied into p1's store.
pub async fn import_claude_code_login(
    route_id: &str,
    dir: &Path,
    locations: &Locations,
) -> Result<(), p1_auth::store::ImportError> {
    p1_auth::store::import_login(
        route_id,
        &method("claude-code-oauth"),
        &dir.join(".credentials.json"),
        &[],
        locations,
    )
    .await
}
