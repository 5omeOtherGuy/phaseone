//! p1's own credential store (spec §3): `$XDG_CONFIG_HOME/p1/auth.json`, else
//! `~/.config/p1/auth.json`.
//!
//! One JSON object keyed by ROUTE id, with `{"type":"api_key","key":…}` and
//! `{"type":"oauth","access":…,"refresh":…,"expires":…,"account_id":…}` entries.
//! It READS it, and it WRITES it for two callers: an `oauth` entry that had to be
//! refreshed is written back to the store, and `p1 login`/`p1 logout` (spec §6,
//! ADR-0044) put one pasted API key in and take one out; `p1 login <route>
//! --from-claude-code` copies one Claude Code login in as an `oauth` entry
//! (ADR-0074). Every write goes under
//! the same non-blocking lock, through the same staged 0600 writer
//! ([`crate::credential_file`]).
//!
//! Endpoint approvals (ADR-0110) live in `auth.json.origins`, a separate protected
//! metadata document so origin refusal never reads the credential document. Login
//! records approval after publishing the key/import; logout revokes both.
//!
//! A store file or directory that is group/world-accessible is REFUSED (spec §3):
//! plain text on disk is only as private as its mode. So is a store reached through
//! a directory someone else owns or can write to, a symlinked or hard-linked store
//! file, or one that is not a regular file (issue #484). The borrowed files of other
//! tools are read as they are — their permissions are theirs.
//!
//! Linux-only today: the checks use `std::os::unix` and `rustix`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::{BoxFuture, ProviderError};
use p1_provider_http::{Credential, HttpRequest, Transport};
use serde_json::{Value, json};

use crate::api_key::usable_key;
use crate::claude_code::{DEFAULT_SCOPES, OAUTH_BETA};
use crate::codex::percent_encode;
use crate::credential_file::{CredentialDir, CredentialLock, DirKind, FileError, PublishError};
use crate::locations::Locations;
use crate::refresh_http::{self, RefreshIoError};
use crate::resolve::{Entry, Presence, SourceName};
use crate::{CredentialKind, auth};

/// The OAuth dialect a store entry belongs to: which token endpoint refreshes it.
/// It follows the ROUTE kind, never the entry (spec §2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OauthDialect {
    ClaudeCode,
    Codex,
}

/// One `oauth` entry of the store, as `current` reads it. A refresh re-reads the
/// whole document instead, so the refresh token is not carried here.
struct StoredOauth {
    access: String,
    expires_ms: Option<u64>,
    account_id: Option<String>,
}

impl OauthDialect {
    fn token_url(self) -> &'static str {
        match self {
            OauthDialect::ClaudeCode => "https://platform.claude.com/v1/oauth/token",
            OauthDialect::Codex => "https://auth.openai.com/oauth/token",
        }
    }

    fn client_id(self) -> &'static str {
        match self {
            OauthDialect::ClaudeCode => "9d1c250a-e61b-44d9-88ed-5944d1962f5e",
            OauthDialect::Codex => "app_EMoamEEZ73f0CkXaXp7hrann",
        }
    }

    /// The refresh request, byte for byte what the borrowed source of the same
    /// dialect sends.
    fn request(self, refresh_token: &str) -> HttpRequest {
        match self {
            OauthDialect::ClaudeCode => HttpRequest {
                url: self.token_url().to_string(),
                headers: vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("anthropic-beta".to_string(), OAUTH_BETA.to_string()),
                ],
                body: serde_json::to_vec(&json!({
                    "grant_type": "refresh_token",
                    "refresh_token": refresh_token,
                    "client_id": self.client_id(),
                    "scope": DEFAULT_SCOPES,
                }))
                .unwrap_or_default(),
            },
            OauthDialect::Codex => HttpRequest {
                url: self.token_url().to_string(),
                headers: vec![
                    (
                        "Content-Type".to_string(),
                        "application/x-www-form-urlencoded".to_string(),
                    ),
                    ("Accept".to_string(), "application/json".to_string()),
                ],
                body: format!(
                    "grant_type=refresh_token&refresh_token={}&client_id={}",
                    percent_encode(refresh_token),
                    percent_encode(self.client_id()),
                )
                .into_bytes(),
            },
        }
    }

    /// The rotated tokens, each checked on its own; nothing here names a value.
    fn parse(self, value: &Value) -> Refreshed {
        let text = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let access = match text("access_token") {
            None => Err("the p1 store token refresh response is missing an access token"),
            Some(access) if !usable_key(&access) => Err(
                "the p1 store token refresh response holds an access token that is not a \
                 header-safe token",
            ),
            Some(access) => Ok(access),
        };
        let lifetime_ms = match value.get("expires_in").and_then(Value::as_u64) {
            None => Err("the p1 store token refresh response is missing the token lifetime"),
            Some(seconds) => refresh_http::lifetime_ms(seconds).ok_or(
                "the p1 store token refresh response granted a token lifetime too short to use",
            ),
        };
        Refreshed {
            access,
            refresh: text("refresh_token"),
            lifetime_ms,
        }
    }
}

/// A refresh response, each field checked on its own: a rotated refresh token is
/// kept even when the rest of the response cannot be used, so the login survives.
struct Refreshed {
    /// The new access token, or why it cannot be used.
    access: Result<String, &'static str>,
    /// The rotated refresh token; `None` when the server rotates none (an older server).
    refresh: Option<String>,
    /// The granted lifetime in milliseconds, or why it cannot be used.
    lifetime_ms: Result<u64, &'static str>,
}

/// Refresh this far ahead of expiry so an in-flight request never races a token
/// going stale.
const REFRESH_MARGIN_MS: u64 = refresh_http::REFRESH_MARGIN_MS;

/// The store's file and its lock file, inside the store directory.
const STORE_FILE: &str = "auth.json";
const STORE_LOCK: &str = "auth.json.lock";
// Same protected credential-file family as auth.json, but contains no credentials:
// origin refusal must not open the file holding keys or OAuth tokens.
const ORIGINS_FILE: &str = "auth.json.origins";

/// A parsed store entry: what the chain needs, and nothing else.
enum EntryValue {
    ApiKey(String),
    Oauth {
        access: String,
        refresh: Option<String>,
        expires_ms: Option<u64>,
        account_id: Option<String>,
    },
}

/// The store's directory, opened and checked (0700, owned by this user, nothing
/// writable by anyone else above it). `Ok(None)` means p1 has no store directory yet.
fn open_dir(path: &Path) -> Result<Option<CredentialDir>, String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("/"));
    match CredentialDir::open(dir, DirKind::Private) {
        Ok(opened) => Ok(Some(opened)),
        Err(FileError::Missing) => Ok(None),
        Err(FileError::Refused(reason)) => Err(reason),
        Err(FileError::Io) => Err(format!(
            "the p1 store directory {} could not be opened",
            dir.display()
        )),
    }
}

/// The store's directory for a write: created 0700 when missing, refused when an
/// existing one lets anyone else in.
fn writable_dir(path: &Path) -> Result<CredentialDir, String> {
    let dir = path.parent().unwrap_or_else(|| Path::new("/"));
    CredentialDir::create_private(dir).map_err(|error| match error {
        FileError::Refused(reason) => reason,
        FileError::Missing | FileError::Io => format!(
            "the p1 store directory {} could not be created",
            dir.display()
        ),
    })
}

/// The store document in `dir`. `Ok(None)` means there is no store file; `Err` means
/// the file exists but must not be used (its type, owner, mode, size or shape).
fn read_document(dir: &CredentialDir, path: &Path) -> Result<Option<Value>, String> {
    let bytes = match dir.read(STORE_FILE) {
        Ok(bytes) => bytes,
        Err(FileError::Missing) => return Ok(None),
        Err(FileError::Refused(reason)) => return Err(reason),
        Err(FileError::Io) => {
            return Err(format!("the p1 store {} could not be read", path.display()));
        }
    };
    let document: Value = serde_json::from_slice(&bytes)
        .map_err(|_| format!("the p1 store {} is malformed", path.display()))?;
    if !document.is_object() {
        return Err(format!(
            "the p1 store {} is not a JSON object keyed by route",
            path.display()
        ));
    }
    Ok(Some(document))
}

/// Whether a kept, unpublished store copy may replace the store (a JSON object).
fn valid_store(bytes: &[u8]) -> bool {
    serde_json::from_slice::<Value>(bytes).is_ok_and(|document| document.is_object())
}

/// The store's path and document. `Ok(None)` means p1 has no store yet; `Err` means
/// the store exists but must not be used.
fn load(locations: &Locations) -> Result<Option<(PathBuf, Value)>, String> {
    let Some(path) = locations.p1_store_path() else {
        return Ok(None);
    };
    let Some(dir) = open_dir(&path)? else {
        return Ok(None);
    };
    Ok(read_document(&dir, &path)?.map(|document| (path, document)))
}

/// Take the store lock. Under it, a login a previous refresh could not publish is
/// adopted first.
async fn lock(dir: &CredentialDir) -> Result<CredentialLock, String> {
    let lock = dir.lock(STORE_LOCK).await.map_err(|error| match error {
        FileError::Refused(reason) => reason,
        FileError::Missing | FileError::Io => "the p1 store lock could not be acquired".to_string(),
    })?;
    dir.recover(STORE_FILE, valid_store);
    dir.recover(ORIGINS_FILE, valid_store);
    Ok(lock)
}

/// Replace the store with `document`: staged 0600, synced, checked, renamed.
fn publish(dir: &CredentialDir, _lock: &CredentialLock, document: &Value) -> Result<(), String> {
    publish_file(dir, STORE_FILE, document)
}

fn publish_file(dir: &CredentialDir, name: &str, document: &Value) -> Result<(), String> {
    let encoded = encode(document);
    let mut staging = dir
        .stage(name, encoded.len())
        .map_err(|_| "the p1 store could not be written".to_string())?;
    staging
        .publish(encoded.as_bytes())
        .map_err(|error| publish_message(&error))
}

fn publish_message(error: &PublishError) -> String {
    match error {
        PublishError::NotPublished(Some(reason)) => {
            format!("the p1 store was not replaced: {reason}")
        }
        PublishError::NotPublished(None) => "the p1 store could not be replaced".to_string(),
        PublishError::NotDurable => {
            "the p1 store was replaced but could not be flushed to disk".to_string()
        }
        PublishError::Changed(reason) => format!("the p1 store was not replaced: {reason}"),
    }
}

/// An expiry field: absent or `null` is "no expiry"; anything but a non-negative
/// integer is an error, never a silently fresh token.
fn expiry(raw: &Value, field: &str) -> Result<Option<u64>, ()> {
    match raw.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(value) => value.as_u64().map(Some).ok_or(()),
    }
}

/// The entry for one route, checked against the kind the route declared.
fn entry_of(
    document: &Value,
    route_id: &str,
    kind: CredentialKind,
) -> Result<Option<EntryValue>, String> {
    let Some(raw) = document.get(route_id) else {
        return Ok(None);
    };
    let declared = raw.get("type").and_then(Value::as_str);
    match kind {
        CredentialKind::ApiKey => {
            if declared != Some("api_key") {
                return Err(format!(
                    "the p1 store entry for route \"{route_id}\" is not an api_key entry"
                ));
            }
            let key = raw.get("key").and_then(Value::as_str).unwrap_or_default();
            if key.starts_with('!') {
                return Err(
                    "command-backed keys are unsupported; set the documented key environment \
                     variable"
                        .into(),
                );
            }
            if !usable_key(key) {
                return Err(format!(
                    "the p1 store entry for route \"{route_id}\" holds no usable key"
                ));
            }
            Ok(Some(EntryValue::ApiKey(key.to_string())))
        }
        CredentialKind::ClaudeCodeOauth | CredentialKind::CodexOauth => {
            if declared != Some("oauth") {
                return Err(format!(
                    "the p1 store entry for route \"{route_id}\" is not an oauth entry"
                ));
            }
            let access = raw
                .get("access")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .trim()
                .to_string();
            if access.is_empty() {
                return Err(format!(
                    "the p1 store oauth entry for route \"{route_id}\" has no access token"
                ));
            }
            // The token goes into a header: anything a header cannot carry is refused
            // here, naming the entry, instead of failing later as a transport error.
            if !usable_key(&access) {
                return Err(format!(
                    "the p1 store oauth entry for route \"{route_id}\" holds an access token \
                     that is not a header-safe token"
                ));
            }
            let refresh = match raw.get("refresh") {
                None | Some(Value::Null) => None,
                Some(Value::String(refresh)) if !refresh.trim().is_empty() => {
                    Some(refresh.trim().to_string())
                }
                Some(_) => {
                    return Err(format!(
                        "the p1 store oauth entry for route \"{route_id}\" has an invalid \
                         refresh token"
                    ));
                }
            };
            let expires_ms = expiry(raw, "expires").map_err(|()| {
                format!(
                    "the p1 store oauth entry for route \"{route_id}\" has an invalid expiry \
                     (not a millisecond timestamp)"
                )
            })?;
            let account_id = match raw.get("account_id") {
                None | Some(Value::Null) => None,
                Some(Value::String(id)) if usable_key(id) => Some(id.clone()),
                Some(_) => {
                    return Err(format!(
                        "the p1 store oauth entry for route \"{route_id}\" has an account id \
                         that is not a header-safe token"
                    ));
                }
            };
            Ok(Some(EntryValue::Oauth {
                access,
                refresh,
                expires_ms,
                account_id,
            }))
        }
        // A route that sends no credential never reads the store (issue #134): the
        // arm exists only because the match is total, and it answers what an absent
        // entry answers.
        CredentialKind::None => Ok(None),
    }
}

fn read_entry(
    locations: &Locations,
    route_id: &str,
    kind: CredentialKind,
) -> Result<Option<EntryValue>, String> {
    match load(locations)? {
        Some((_, document)) => entry_of(&document, route_id, kind),
        None => Ok(None),
    }
}

/// Whether the store has an entry for this route, for [`crate::describe`].
pub(crate) fn presence(locations: &Locations, route_id: &str, kind: CredentialKind) -> Presence {
    match read_entry(locations, route_id, kind) {
        Ok(Some(_)) => Presence::Present,
        Ok(None) => Presence::Absent,
        Err(reason) => Presence::Unusable(reason),
    }
}

// ------------------------------------------------------------------ the write side (spec §6)

/// Write one route's API key into p1's store (spec §6, ADR-0044): read-modify-write
/// under the store lock, written atomically, with every other entry left as it was.
///
/// The store is created 0700/0600 when it is missing. An existing file or directory
/// anyone but the owner can reach is REFUSED with the `chmod` to run, never silently
/// tightened. The key is checked here, next to the readers that apply the same rule,
/// and never appears in an error.
pub async fn put_api_key(route_id: &str, key: &str, locations: &Locations) -> Result<(), String> {
    put_api_key_at_origin(route_id, key, None, locations).await
}

/// Store a key and its approved endpoint origin under the same lock (ADR-0110).
/// `None` supports callers that do not register a route; it revokes old origin trust.
pub async fn put_api_key_at_origin(
    route_id: &str,
    key: &str,
    origin: Option<&str>,
    locations: &Locations,
) -> Result<(), String> {
    if key.is_empty() {
        return Err(format!(
            "the key for route \"{route_id}\" is empty; nothing was written"
        ));
    }
    if !usable_key(key) {
        return Err(format!(
            "the key for route \"{route_id}\" is not a header-safe token (printable ASCII, no \
             spaces); nothing was written"
        ));
    }
    let entry = json!({"type": "api_key", "key": key});
    write_entry(route_id, entry, origin, locations).await
}

/// Put `entry` into the store under `route_id`, every other entry left as it was.
async fn write_entry(
    route_id: &str,
    entry: Value,
    origin: Option<&str>,
    locations: &Locations,
) -> Result<(), String> {
    let path = store_path(locations)?;
    check_writable(locations)?;
    let dir = writable_dir(&path)?;
    let lock = lock(&dir).await?;
    let mut document =
        read_document(&dir, &path)?.unwrap_or_else(|| Value::Object(serde_json::Map::new()));
    let mut origins = read_origins(&dir)?;
    // Revoke first: an interrupted login may leave an untrusted new key, never a
    // new key trusted for the previous key's origin.
    origins.as_object_mut().unwrap().remove(route_id);
    publish_file(&dir, ORIGINS_FILE, &origins)?;
    if let Some(object) = document.as_object_mut() {
        object.insert(route_id.to_string(), entry);
    }
    publish(&dir, &lock, &document)?;
    if let Some(origin) = origin {
        origins[route_id] = json!(origin);
        publish_file(&dir, ORIGINS_FILE, &origins)?;
    }
    Ok(())
}

fn read_origins(dir: &CredentialDir) -> Result<Value, String> {
    let bytes = match dir.read(ORIGINS_FILE) {
        Ok(bytes) => bytes,
        Err(FileError::Missing) => return Ok(json!({})),
        Err(FileError::Refused(reason)) => return Err(reason),
        Err(FileError::Io) => return Err("the p1 store endpoint origins could not be read".into()),
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| "the p1 store endpoint origins are malformed".to_string())?;
    if !value.as_object().is_some_and(|entries| {
        entries
            .values()
            .all(|origin| origin.as_str().is_some_and(|text| !text.is_empty()))
    }) {
        return Err("the p1 store endpoint origins are not an object of origin strings".into());
    }
    Ok(value)
}

/// Read only protected origin metadata, never the credential document (ADR-0110).
pub fn endpoint_origin(route_id: &str, locations: &Locations) -> Result<Option<String>, String> {
    let Some(path) = locations.p1_store_path() else {
        return Ok(None);
    };
    let Some(dir) = open_dir(&path)? else {
        return Ok(None);
    };
    Ok(read_origins(&dir)?
        .get(route_id)
        .and_then(Value::as_str)
        .map(str::to_string))
}

/// Approve an endpoint for an environment key; no key is read or stored.
pub async fn trust_endpoint(
    route_id: &str,
    origin: &str,
    locations: &Locations,
) -> Result<(), String> {
    let path = store_path(locations)?;
    let dir = writable_dir(&path)?;
    let _lock = lock(&dir).await?;
    let mut origins = read_origins(&dir)?;
    origins[route_id] = json!(origin);
    publish_file(&dir, ORIGINS_FILE, &origins)
}

/// Why a Claude Code login was not imported. Every message names paths and routes
/// only, never a token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// The directory holds no Claude Code login: the fix is to log in there first.
    NoLogin(String),
    /// Anything else: an unusable login file, or a store that must not be written.
    Failed(String),
}

impl std::fmt::Display for ImportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ImportError::NoLogin(message) | ImportError::Failed(message) => f.write_str(message),
        }
    }
}

/// Copy the Claude Code login in `dir` (its `.credentials.json`) into p1's store as
/// this route's `oauth` entry (ADR-0074): `{"type":"oauth","access","refresh",
/// "expires","account_id"}`, with `null` for a field the login does not record. The
/// account id is Claude Code's `oauthAccount.accountUuid` from `dir/.claude.json`
/// when that file has one.
///
/// Read-modify-write under the store lock through the same atomic 0600 writer as
/// every other write; a store file or directory anyone but the owner can reach is
/// refused exactly as [`put_api_key`] refuses it. The login file itself is only read,
/// through the same checks as the borrowed login source.
pub async fn import_claude_code_login(
    route_id: &str,
    dir: &Path,
    locations: &Locations,
) -> Result<(), ImportError> {
    import_claude_code_login_at_origin(route_id, dir, None, locations).await
}

/// Import a login and bind its store entry to the route's approved origin.
pub async fn import_claude_code_login_at_origin(
    route_id: &str,
    dir: &Path,
    origin: Option<&str>,
    locations: &Locations,
) -> Result<(), ImportError> {
    let path = dir.join(".credentials.json");
    let no_login = || {
        ImportError::NoLogin(format!(
            "no Claude Code login at {}; log in with Claude Code for that directory \
             (`CLAUDE_CONFIG_DIR={} claude`, then `/login`) and run this again",
            path.display(),
            dir.display()
        ))
    };
    let login_dir = match CredentialDir::open(dir, DirKind::Borrowed) {
        Ok(login_dir) => login_dir,
        Err(FileError::Missing) => return Err(no_login()),
        Err(FileError::Refused(reason)) => return Err(ImportError::Failed(reason)),
        Err(FileError::Io) => {
            return Err(ImportError::Failed(format!(
                "the Claude Code login at {} could not be read",
                path.display()
            )));
        }
    };
    let raw = match login_dir.read(".credentials.json") {
        Ok(raw) => raw,
        Err(FileError::Missing) => return Err(no_login()),
        Err(FileError::Refused(reason)) => return Err(ImportError::Failed(reason)),
        Err(FileError::Io) => {
            return Err(ImportError::Failed(format!(
                "the Claude Code login at {} could not be read",
                path.display()
            )));
        }
    };
    let document: Value = serde_json::from_slice(&raw).map_err(|_| {
        ImportError::Failed(format!(
            "the Claude Code login at {} is malformed",
            path.display()
        ))
    })?;
    let login = crate::claude_code::parse_credentials(&document, &path)
        .map_err(|error| ImportError::Failed(error.message))?;
    let account_id = login_dir
        .read(".claude.json")
        .ok()
        .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
        .and_then(|config| {
            config
                .get("oauthAccount")
                .and_then(|account| account.get("accountUuid"))
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|id| usable_key(id))
                .map(str::to_string)
        });

    let entry = json!({
        "type": "oauth",
        "access": login.access,
        "refresh": login.refresh,
        "expires": login.expires_ms,
        "account_id": account_id,
    });
    write_entry(route_id, entry, origin, locations)
        .await
        .map_err(ImportError::Failed)
}

/// Remove one route's entry from p1's store (spec §6), leaving every other entry as
/// it was. `Ok(false)` means there was nothing to remove — a missing entry is
/// reported, not an error — and nothing is created for a route that has no store yet.
/// Only a store that does not EXIST is "nothing": one that cannot be read, is not a
/// regular file or may not be used is an error naming why.
pub async fn remove(route_id: &str, locations: &Locations) -> Result<bool, String> {
    let path = store_path(locations)?;
    let Some(dir) = open_dir(&path)? else {
        return Ok(false);
    };
    let lock = lock(&dir).await?;
    let mut origins = read_origins(&dir)?;
    let origin_removed = origins.as_object_mut().unwrap().remove(route_id).is_some();
    if origin_removed {
        publish_file(&dir, ORIGINS_FILE, &origins)?;
    }
    let Some(mut document) = read_document(&dir, &path)? else {
        return Ok(origin_removed);
    };
    let removed = document.as_object_mut().unwrap().remove(route_id).is_some();
    if removed {
        publish(&dir, &lock, &document)?;
    }
    Ok(removed || origin_removed)
}

/// Whether p1's store may be written: the host has a location for it, and what is
/// already there is private and readable. A login calls this BEFORE it reads a key
/// (spec §6), so a store with wider permissions — or a file this crate could not
/// preserve — is refused before the user types anything; the write checks again under
/// the lock.
pub fn check_writable(locations: &Locations) -> Result<(), String> {
    store_path(locations)?;
    load(locations)?;
    Ok(())
}

/// The store's path, or the error that names what to set when the host has no home.
fn store_path(locations: &Locations) -> Result<PathBuf, String> {
    locations.p1_store_path().ok_or_else(|| {
        "cannot locate p1's credential store: set HOME or XDG_CONFIG_HOME".to_string()
    })
}

/// The document as p1 writes it: pretty, newline-terminated, so rewriting an
/// unchanged document is byte-identical.
fn encode(document: &Value) -> String {
    let mut encoded =
        serde_json::to_string_pretty(document).unwrap_or_else(|_| document.to_string());
    encoded.push('\n');
    encoded
}

/// An API key read from p1's store. Re-read on every `access`: a key rotated in
/// the file is picked up without a restart.
pub(crate) struct StoreApiKey {
    locations: Locations,
    route_id: String,
}

impl StoreApiKey {
    pub(crate) fn new(locations: &Locations, route_id: &str) -> Self {
        Self {
            locations: locations.clone(),
            route_id: route_id.to_string(),
        }
    }

    fn read(&self) -> Result<Option<String>, String> {
        match read_entry(&self.locations, &self.route_id, CredentialKind::ApiKey)? {
            Some(EntryValue::ApiKey(key)) => Ok(Some(key)),
            Some(EntryValue::Oauth { .. }) => Err(format!(
                "the p1 store entry for route \"{}\" is not an api_key entry",
                self.route_id
            )),
            None => Ok(None),
        }
    }
}

impl Entry for StoreApiKey {
    fn name(&self) -> SourceName {
        SourceName::P1Store
    }

    fn presence(&self) -> Presence {
        match self.read() {
            Ok(Some(_)) => Presence::Present,
            Ok(None) => Presence::Absent,
            Err(reason) => Presence::Unusable(reason),
        }
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            match self.read() {
                Ok(Some(key)) => Ok(Credential {
                    bearer: key,
                    account_id: None,
                }),
                Ok(None) => Err(auth(format!(
                    "no p1 store entry for route \"{}\"; add one or set the documented key \
                     environment variable",
                    self.route_id
                ))),
                Err(reason) => Err(auth(reason)),
            }
        })
    }

    fn rotated<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            match self.read() {
                Ok(Some(key)) if key != rejected.bearer => Ok(Credential {
                    bearer: key,
                    account_id: None,
                }),
                Ok(_) => Err(auth(format!(
                    "the p1 store key for route \"{}\" was rejected; update its entry or set the \
                     documented key environment variable",
                    self.route_id
                ))),
                Err(reason) => Err(auth(reason)),
            }
        })
    }
}

/// An `oauth` entry in p1's store. An expired one is refreshed and written back to
/// the store — "write-back to the source", never to another one (ADR-0040).
pub(crate) struct StoreOauth {
    locations: Locations,
    route_id: String,
    dialect: OauthDialect,
    transport: Arc<dyn Transport>,
}

impl StoreOauth {
    pub(crate) fn new(
        locations: &Locations,
        route_id: &str,
        dialect: OauthDialect,
        transport: Arc<dyn Transport>,
    ) -> Self {
        Self {
            locations: locations.clone(),
            route_id: route_id.to_string(),
            dialect,
            transport,
        }
    }

    fn read(&self) -> Result<Option<StoredOauth>, String> {
        match read_entry(&self.locations, &self.route_id, self.kind())? {
            Some(EntryValue::Oauth {
                access,
                refresh: _,
                expires_ms,
                account_id,
            }) => Ok(Some(StoredOauth {
                access,
                expires_ms,
                account_id,
            })),
            Some(EntryValue::ApiKey(_)) => Err(format!(
                "the p1 store entry for route \"{}\" is not an oauth entry",
                self.route_id
            )),
            None => Ok(None),
        }
    }

    fn is_fresh(expires_ms: Option<u64>) -> bool {
        match expires_ms {
            Some(expires) => expires > now_ms().saturating_add(REFRESH_MARGIN_MS),
            None => true,
        }
    }

    /// Refresh under the store lock: re-read, rotate, write back atomically. A peer
    /// that already rotated the rejected token is used instead of a second rotation.
    async fn refresh_locked(&self, rejected: Option<&str>) -> Result<Credential, ProviderError> {
        let path = store_path(&self.locations).map_err(auth)?;
        let no_entry = || {
            auth(format!(
                "the p1 store has no entry for route \"{}\"",
                self.route_id
            ))
        };
        let Some(dir) = open_dir(&path).map_err(auth)? else {
            return Err(no_entry());
        };
        let lock = lock(&dir).await.map_err(auth)?;
        let Some(document) = read_document(&dir, &path).map_err(auth)? else {
            return Err(no_entry());
        };
        let Some(EntryValue::Oauth {
            access,
            refresh,
            expires_ms,
            account_id,
        }) = entry_of(&document, &self.route_id, self.kind()).map_err(auth)?
        else {
            return Err(auth(format!(
                "the p1 store has no oauth entry for route \"{}\"",
                self.route_id
            )));
        };

        if Self::is_fresh(expires_ms) && rejected.is_none_or(|rejected| access != rejected) {
            return Ok(Credential {
                bearer: access,
                account_id,
            });
        }
        let refresh_token = refresh.ok_or_else(|| {
            auth(match rejected {
                // A 401 on a token the clock calls fresh: it was rejected, not expired.
                Some(_) => format!(
                    "the p1 store oauth entry for route \"{}\" records no refresh token, so \
                     its rejected access token cannot be replaced; import or log in again",
                    self.route_id
                ),
                None => format!(
                    "the p1 store oauth entry for route \"{}\" records no refresh token and its \
                     access token is expired",
                    self.route_id
                ),
            })
        })?;
        let rotation = StoreRotation {
            dir,
            _lock: lock,
            baseline: document,
            started: tokio::time::Instant::now(),
            path,
            route_id: self.route_id.clone(),
            kind: self.kind(),
            dialect: self.dialect,
            transport: self.transport.clone(),
            refresh_token,
            account_id,
            rejected: rejected.map(str::to_string),
        };
        refresh_http::detached(rotation.run()).await
    }

    fn kind(&self) -> CredentialKind {
        match self.dialect {
            OauthDialect::ClaudeCode => CredentialKind::ClaudeCodeOauth,
            OauthDialect::Codex => CredentialKind::CodexOauth,
        }
    }
}

/// One started rotation of a store entry. It owns everything it needs — the opened
/// directory and the held lock included — so it runs to its write-back even when the
/// caller that started it is cancelled ([`refresh_http::detached`]).
struct StoreRotation {
    dir: CredentialDir,
    /// Held until the rotation is written back.
    _lock: CredentialLock,
    /// The store as this rotation started from it: where the rotated tokens are kept
    /// when the store cannot be read back after the request.
    baseline: Value,
    /// When the refresh began: its time bounds count from here.
    started: tokio::time::Instant,
    path: PathBuf,
    route_id: String,
    kind: CredentialKind,
    dialect: OauthDialect,
    transport: Arc<dyn Transport>,
    refresh_token: String,
    account_id: Option<String>,
    rejected: Option<String>,
}

impl StoreRotation {
    async fn run(self) -> Result<Credential, ProviderError> {
        // The staging file is made, and the disk space reserved, BEFORE the refresh
        // token is spent: a store that cannot be written fails here, with the old
        // login still valid.
        let reserve = std::fs::metadata(&self.path).map_or(0, |metadata| metadata.len());
        let mut staging = self
            .dir
            .stage(STORE_FILE, usize::try_from(reserve).unwrap_or(0) * 2 + 4096)
            .map_err(|_| auth("the p1 store could not be prepared for the refreshed login"))?;
        // The lifetime counts from when the token was minted, never from when a slow
        // response finally arrived.
        let sent_at = now_ms();
        let bytes = refresh_http::exchange(
            self.transport.as_ref(),
            self.dialect.request(&self.refresh_token),
            self.started,
        )
        .await
        .map_err(|error| match error {
            RefreshIoError::TimedOut(error) => error,
            RefreshIoError::Status(status) => auth(format!(
                "the p1 store token refresh failed with status {status}"
            )),
            RefreshIoError::TooLarge => {
                auth("the p1 store token refresh response is too large to be one")
            }
            RefreshIoError::Transport(_) => auth("the p1 store token refresh request failed"),
        })?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| auth("the p1 store token refresh response is malformed"))?;
        let response = self.dialect.parse(&value);
        let usable = match (&response.access, &response.lifetime_ms) {
            (Ok(access), Ok(_)) if self.rejected.as_deref() == Some(access.as_str()) => {
                Err("the token refresh returned the rejected token; log in again for this route")
            }
            (Ok(access), Ok(lifetime)) => Ok((access.clone(), sent_at.saturating_add(*lifetime))),
            (Err(problem), _) | (_, Err(problem)) => Err(*problem),
        };

        // Another writer may have changed the store while the request was out. The
        // entry this rotation started from must still be there; every other entry
        // is taken from the file as it is NOW.
        let latest = match read_document(&self.dir, &self.path) {
            Ok(latest) => latest,
            // The server may have rotated the refresh token already: the rotation is
            // kept beside the store, computed from the store it started from.
            Err(reason) => {
                if response.refresh.is_none() {
                    return Err(auth(reason));
                }
                let mut kept = self.baseline.clone();
                apply_rotation(
                    &mut kept,
                    &self.route_id,
                    &usable,
                    response.refresh.as_deref(),
                );
                let reason = if staging.keep(encode(&kept).as_bytes()) {
                    format!(
                        "{reason}; the refreshed login was kept beside the store; the next \
                         refresh adopts it unless the store is changed first"
                    )
                } else {
                    reason
                };
                // A usable access token is still this request's credential.
                return match usable {
                    Ok((access, _)) => Ok(Credential {
                        bearer: access,
                        account_id: self.account_id,
                    }),
                    Err(_) => Err(auth(reason)),
                };
            }
        };
        let current = latest
            .as_ref()
            .map(|latest| entry_of(latest, &self.route_id, self.kind));
        let unchanged = matches!(
            &current,
            Some(Ok(Some(EntryValue::Oauth { refresh: Some(refresh), .. })))
                if *refresh == self.refresh_token
        );
        if !unchanged {
            return match current {
                Some(Ok(Some(EntryValue::Oauth {
                    access,
                    expires_ms,
                    account_id,
                    ..
                }))) if StoreOauth::is_fresh(expires_ms)
                    && self.rejected.as_deref() != Some(access.as_str()) =>
                {
                    Ok(Credential {
                        bearer: access,
                        account_id,
                    })
                }
                _ => Err(auth(format!(
                    "the p1 store entry for route \"{}\" changed while it was being \
                     refreshed; it was left as it is — retry",
                    self.route_id
                ))),
            };
        }
        let Some(mut latest) = latest else {
            return Err(auth("the p1 store disappeared during the token refresh"));
        };
        if usable.is_err() && response.refresh.is_none() {
            // Nothing was rotated: nothing is written, the store stays byte-identical.
            return Err(auth(usable.err().unwrap_or_default()));
        }
        apply_rotation(
            &mut latest,
            &self.route_id,
            &usable,
            response.refresh.as_deref(),
        );
        let encoded = encode(&latest);
        if let Err(error) = staging.publish(encoded.as_bytes()) {
            let kept = error.keeps() && staging.keep_for_recovery();
            let message = publish_message(&error);
            return Err(auth(if kept {
                format!(
                    "{message}; the refreshed login was kept beside the store and is used by \
                     the next refresh"
                )
            } else {
                message
            }));
        }
        let (access, _) = usable.map_err(auth)?;
        Ok(Credential {
            bearer: access,
            account_id: self.account_id,
        })
    }
}

/// Write one rotation into the route's entry of `document`.
fn apply_rotation(
    document: &mut Value,
    route_id: &str,
    usable: &Result<(String, u64), &str>,
    refresh: Option<&str>,
) {
    let Some(entry) = document.get_mut(route_id).and_then(Value::as_object_mut) else {
        return;
    };
    match usable {
        Ok((access, expires)) => {
            entry.insert("access".to_string(), json!(access));
            entry.insert("expires".to_string(), json!(expires));
        }
        // The response is unusable, but the server rotated the refresh token: keep the
        // new one, and mark the access token expired so the next access refreshes with
        // it instead of breaking the login.
        Err(_) => {
            entry.insert("expires".to_string(), json!(0));
        }
    }
    if let Some(refresh) = refresh {
        entry.insert("refresh".to_string(), json!(refresh));
    }
}

impl Entry for StoreOauth {
    fn name(&self) -> SourceName {
        SourceName::P1Store
    }

    fn presence(&self) -> Presence {
        match self.read() {
            Ok(Some(_)) => Presence::Present,
            Ok(None) => Presence::Absent,
            Err(reason) => Presence::Unusable(reason),
        }
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            match self.read().map_err(auth)? {
                Some(stored) if Self::is_fresh(stored.expires_ms) => Ok(Credential {
                    bearer: stored.access,
                    account_id: stored.account_id,
                }),
                Some(_) => self.refresh_locked(None).await,
                None => Err(auth(format!(
                    "no p1 store entry for route \"{}\"; add one or log in with the CLI that \
                     owns this login",
                    self.route_id
                ))),
            }
        })
    }

    fn rotated<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.refresh_locked(Some(&rejected.bearer)).await })
    }
}

fn now_ms() -> u64 {
    crate::claude_code::system_clock()
}
