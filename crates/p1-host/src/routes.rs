//! Route files (`routes/<id>.toml`): how an ACCOUNT and ENDPOINT are reached, as
//! data (`docs/design/routes-and-profiles.md` §1.2). A route file names an ADAPTER
//! KEY that `p1-host::catalog` has compiled in, and it holds a credential
//! REFERENCE, never a value. The lookup directory is the one profiles use:
//! `<environments dir>/../routes`.
//!
//! Loading is total: a file stem that disagrees with `id`, a secret-looking header,
//! an unknown adapter, an unknown credential kind, a credential kind whose source is
//! not data-driven yet, a settings key the adapter does not know, or an unusable
//! model binding are all load errors reported before any provider is built.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The adapter keys a route file may name. `catalog` dispatches on exactly this set;
/// an unknown `adapter` is a load error listing these.
pub const ADAPTER_KEYS: &[&str] = &["openai-chat"];

/// The credential kinds a route file may name (spec 1.2). The two OAuth kinds parse,
/// but a route that uses one is rejected until step 4 makes their sources
/// data-driven.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    /// An API key: `env` and/or `borrow` say where it is read from.
    ApiKey,
    /// The Claude Code CLI login (its source stays the compiled one).
    ClaudeCodeOauth,
    /// The Codex CLI login (its source stays the compiled one).
    CodexOauth,
}

/// A credential file a route borrows a key from, in the order the file lists them.
/// Parsed from `"<store>:<key>"`. The paths behind each store live in `auth`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowSource {
    pub store: BorrowStore,
    /// The provider key inside that store's credential file.
    pub key: String,
}

impl CredentialKind {
    /// The spelling a route file uses for this kind.
    pub fn name(self) -> &'static str {
        match self {
            CredentialKind::ApiKey => "api-key",
            CredentialKind::ClaudeCodeOauth => "claude-code-oauth",
            CredentialKind::CodexOauth => "codex-oauth",
        }
    }
}

/// The credential files a route may borrow from (ADR-0040: read-only reuse of an
/// existing CLI login).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowStore {
    /// The OpenCode CLI's shared data directory.
    Opencode,
    /// The Pi CLI's agent directory.
    Pi,
}

impl<'de> Deserialize<'de> for BorrowSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(deserializer)?;
        let (store, key) = text.split_once(':').ok_or_else(|| {
            D::Error::custom(format!(
                "borrow source \"{text}\" is not `<store>:<key>`; the stores are {}",
                known_stores()
            ))
        })?;
        let store = match store {
            "opencode" => BorrowStore::Opencode,
            "pi" => BorrowStore::Pi,
            other => {
                return Err(D::Error::custom(format!(
                    "unknown borrow store \"{other}\" in \"{text}\"; the stores are {}",
                    known_stores()
                )));
            }
        };
        if key.is_empty() {
            return Err(D::Error::custom(format!(
                "borrow source \"{text}\" names no key"
            )));
        }
        Ok(Self {
            store,
            key: key.to_string(),
        })
    }
}

/// The `[credential]` table: a reference to a key, never the key itself. A route
/// cannot hold a secret, only the name of the place a secret is read from.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CredentialRef {
    pub kind: CredentialKind,
    /// The environment variable that precedes every borrowed file.
    #[serde(default)]
    pub env: Option<String>,
    /// Borrowed store entries, tried in this order after `env`.
    #[serde(default)]
    pub borrow: Vec<BorrowSource>,
}

impl CredentialRef {
    /// Only an API-key reference has a data-driven source yet (ADR-0039 step 4 gives
    /// the two OAuth kinds theirs).
    pub fn validate_source(&self) -> Result<(), String> {
        match self.kind {
            CredentialKind::ApiKey => Ok(()),
            kind => Err(format!(
                "credential kind \"{}\" is not yet data-driven (ADR-0039 step 4)",
                kind.name()
            )),
        }
    }
}

/// One `[models.<profile id>]` entry: the wire model this route reaches that profile
/// by, plus the route's own ceilings on it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelBinding {
    /// The name this route's wire protocol knows the model by. It is NOT the profile
    /// id: one profile may be bound to different wire names on different routes.
    pub wire_model: String,
    /// An optional route ceiling on the context window. It lowers the profile's
    /// ceiling, never raises it. Parsed and carried; nothing consumes it yet.
    #[serde(default)]
    pub context_limit: Option<u64>,
    /// An optional route ceiling on output tokens. It lowers the profile's ceiling,
    /// never raises it.
    #[serde(default)]
    pub output_limit: Option<u32>,
}

/// One parsed `routes/<id>.toml`, validated. The host never interprets these fields
/// beyond routing: `[adapter_settings]` is handed to the adapter named by `adapter`
/// as-is (`docs/design/routes-and-profiles.md` §1.2).
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RouteFile {
    /// Must equal the file stem: the key an environment's `route` names.
    pub id: String,
    /// `Origin.route`, explicit in the file so it cannot drift from the route id.
    pub origin_route: String,
    /// A compiled adapter key ([`ADAPTER_KEYS`]).
    pub adapter: String,
    pub endpoint: String,
    pub credential: CredentialRef,
    /// Static, non-secret headers. Authentication comes exclusively from
    /// `[credential]`, so a secret-looking name here is a load error.
    #[serde(default)]
    pub headers: BTreeMap<String, String>,
    /// Kept as an uninterpreted table; [`RouteFile::settings`] types it.
    #[serde(default)]
    pub adapter_settings: Option<toml::Value>,
    /// Profile id -> binding. A profile without an entry is NOT served by this route.
    #[serde(default)]
    pub models: BTreeMap<String, ModelBinding>,
}

/// The `[adapter_settings]` table of one route, typed by the adapter that named it.
#[derive(Debug, Clone, PartialEq)]
pub enum AdapterSettings {
    OpenAiChat(p1_provider_openai_chat::ChatAdapterSettings),
}

impl RouteFile {
    /// The settings the adapter named by `adapter` parses for itself. The host knows
    /// only which type an adapter key selects; the fields belong to the adapter, so
    /// an unknown key fails there, next to the code that would consume it.
    pub fn settings(&self) -> Result<AdapterSettings, String> {
        match self.adapter.as_str() {
            "openai-chat" => self
                .typed_settings::<p1_provider_openai_chat::ChatAdapterSettings>()
                .map(AdapterSettings::OpenAiChat),
            other => Err(format!(
                "unknown adapter \"{other}\"; the known adapters are {}",
                known_adapters()
            )),
        }
    }

    fn typed_settings<T: serde::de::DeserializeOwned>(&self) -> Result<T, String> {
        let table = self
            .adapter_settings
            .clone()
            .unwrap_or_else(|| toml::Value::Table(toml::Table::new()));
        table
            .try_into::<T>()
            .map_err(|error| format!("invalid `[adapter_settings]`: {error}"))
    }

    /// The binding for one profile, or the spec §2 error naming what this route does
    /// serve. There is no pass-through of unknown model names.
    pub fn binding(&self, profile_id: &str) -> Result<&ModelBinding, String> {
        self.models.get(profile_id).ok_or_else(|| {
            let served = if self.models.is_empty() {
                "none".to_string()
            } else {
                self.models
                    .keys()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join(", ")
            };
            format!(
                "route \"{}\" does not serve profile \"{profile_id}\" (it serves: {served})",
                self.id
            )
        })
    }

    fn validate(&self, stem: &str) -> Result<(), String> {
        if self.id != stem {
            return Err(format!(
                "route id \"{}\" must equal the file stem \"{stem}\"",
                self.id
            ));
        }
        if self.origin_route.is_empty() || self.endpoint.is_empty() {
            return Err("`origin_route` and `endpoint` must be nonempty".into());
        }
        if !ADAPTER_KEYS.contains(&self.adapter.as_str()) {
            return Err(format!(
                "unknown adapter \"{}\"; the known adapters are {}",
                self.adapter,
                known_adapters()
            ));
        }
        for (name, value) in &self.headers {
            if is_secret_header(name) {
                return Err(format!(
                    "header \"{name}\" looks like a credential; a route file carries static, \
                     non-secret headers only, and authentication comes from `[credential]`"
                ));
            }
            if !is_header_name(name) {
                return Err(format!("`[headers]` name \"{name}\" is not a header name"));
            }
            if value.is_empty() || !value.bytes().all(|b| (32..=126).contains(&b)) {
                return Err(format!(
                    "`[headers]` value for \"{name}\" is not printable ASCII"
                ));
            }
        }
        if let Some(env) = &self.credential.env
            && !is_env_var_name(env)
        {
            return Err(format!(
                "`[credential]` env \"{env}\" is not an environment variable name"
            ));
        }
        self.credential.validate_source()?;
        for (id, binding) in &self.models {
            if id.trim().is_empty() || binding.wire_model.trim().is_empty() {
                return Err(
                    "every `[models.<profile id>]` entry needs a nonempty profile id and \
                     `wire_model`"
                        .into(),
                );
            }
            if binding.output_limit == Some(0) || binding.context_limit == Some(0) {
                return Err(format!(
                    "`[models.\"{id}\"]` declares a limit of 0; omit a limit that is unknown"
                ));
            }
        }
        self.settings()?;
        Ok(())
    }
}

/// `<dir>/../routes`, for each environments directory the host was given, highest
/// priority first. The same rule `p1-assembly` uses for `<dir>/../profiles`.
pub fn routes_dirs(environment_dirs: &[PathBuf]) -> Vec<PathBuf> {
    environment_dirs
        .iter()
        .map(|dir| dir.join("../routes"))
        .collect()
}

/// Every `*.toml` in `dir`, sorted by file name, parsed and validated. A directory
/// that does not exist holds no routes: an environment naming a route that is not
/// there fails at assembly, where the error can list what exists.
pub fn load_routes(dir: &Path) -> Result<Vec<RouteFile>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "cannot read the route directory {}: {error}",
                dir.display()
            ));
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .collect();
    paths.sort();
    paths.iter().map(|path| load_route(path)).collect()
}

/// Every route file the host can see, highest-priority directory first: an id found
/// in more than one directory resolves to the first one, exactly like an environment
/// or a profile. Sorted by id, so the catalog registers them in a stable order.
pub fn load_all_routes(environment_dirs: &[PathBuf]) -> Result<Vec<RouteFile>, String> {
    let mut routes: Vec<RouteFile> = Vec::new();
    for dir in routes_dirs(environment_dirs) {
        for route in load_routes(&dir)? {
            if !routes.iter().any(|seen| seen.id == route.id) {
                routes.push(route);
            }
        }
    }
    routes.sort_by(|left, right| left.id.cmp(&right.id));
    Ok(routes)
}

/// The one route file an environment's `route` names, in the host's search order.
/// A missing file is an error that lists the ids the directories do hold, like the
/// profile lookup's; the host reports it before it builds a provider (spec §2).
pub fn load_route_by_id(environment_dirs: &[PathBuf], id: &str) -> Result<RouteFile, String> {
    let dirs = routes_dirs(environment_dirs);
    for dir in &dirs {
        let path = dir.join(format!("{id}.toml"));
        if path.is_file() {
            return load_route(&path);
        }
    }
    let searched = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "route `{id}` was not found in {searched}; available: {:?}",
        available_route_ids(&dirs)
    ))
}

/// The route ids the directories hold, for the not-found message. Unreadable
/// directories contribute nothing: this only decorates an error that already fired.
fn available_route_ids(dirs: &[PathBuf]) -> Vec<String> {
    let mut ids: Vec<String> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .filter_map(|path| path.file_stem().and_then(OsStr::to_str).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// Parse and validate one route file. Every error names the file.
pub fn load_route(path: &Path) -> Result<RouteFile, String> {
    let name = |message: String| format!("{}: {message}", path.display());
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| name("the route file name is not UTF-8".into()))?;
    let text = std::fs::read_to_string(path).map_err(|error| name(error.to_string()))?;
    let route: RouteFile = toml::from_str(&text).map_err(|error| name(error.to_string()))?;
    route.validate(stem).map_err(name)?;
    Ok(route)
}

fn known_adapters() -> String {
    ADAPTER_KEYS.join(", ")
}

fn known_stores() -> &'static str {
    "opencode, pi"
}

/// A header name that must never appear in a route file: it could hold a secret by
/// accident (spec 1.2). The adapter rejects these names too, so such a file could
/// never build a provider anyway.
fn is_secret_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    name.starts_with("x-auth")
        || matches!(
            name.as_str(),
            "authorization" | "proxy-authorization" | "x-api-key" | "api-key" | "cookie"
        )
}

/// The adapter rejects content-type, accept, host, content-length and
/// transfer-encoding as static headers; those are protocol-level and set by the
/// transport, so a route file naming one is an error here rather than later.
fn is_header_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "content-type" | "accept" | "host" | "content-length" | "transfer-encoding"
        )
}

fn is_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}
