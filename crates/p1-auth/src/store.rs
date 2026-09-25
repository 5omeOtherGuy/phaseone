//! p1's own credential store (spec §3): `$XDG_CONFIG_HOME/p1/auth.json`, else
//! `~/.config/p1/auth.json`.
//!
//! One JSON object keyed by ROUTE id, with `{"type":"api_key","key":…}` and
//! `{"type":"oauth","access":…,"refresh":…,"expires":…,"account_id":…}` entries.
//! It READS it, and it WRITES it for two callers: an `oauth` entry that had to be
//! refreshed is written back to the store, and `p1 login`/`p1 logout` (spec §6,
//! ADR-0044) put one pasted API key in and take one out. Every write goes under
//! the same non-blocking lock, through the same atomic 0600 writer.
//!
//! A store file or directory that is group/world-accessible is REFUSED (spec §3):
//! plain text on disk is only as private as its mode. The borrowed files of other
//! tools are read as they are — their permissions are theirs.
//!
//! Linux-only today: the check uses `std::os::unix::fs::PermissionsExt`.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::{BoxFuture, ProviderError};
use p1_provider_http::{
    ByteStream, Credential, HttpRequest, LOCK_PATIENCE, Transport, lock_exclusive,
};
use serde_json::{Value, json};

use crate::claude_code::{DEFAULT_SCOPES, OAUTH_BETA};
use crate::codex::percent_encode;
use crate::locations::Locations;
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

    /// The rotated tokens, or an error naming no value.
    fn parse(self, value: &Value) -> Result<Refreshed, ProviderError> {
        let text = |field: &str| {
            value
                .get(field)
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|text| !text.is_empty())
                .map(str::to_string)
        };
        let access = text("access_token").ok_or_else(|| {
            auth("the p1 store token refresh response is missing an access token")
        })?;
        let expires_in = value
            .get("expires_in")
            .and_then(Value::as_u64)
            .ok_or_else(|| {
                auth("the p1 store token refresh response is missing the token lifetime")
            })?;
        Ok(Refreshed {
            access,
            // The store entry keeps the prior refresh token when the server
            // rotates none (an older server).
            refresh: text("refresh_token"),
            expires_in_secs: expires_in,
        })
    }
}

struct Refreshed {
    access: String,
    refresh: Option<String>,
    expires_in_secs: u64,
}

/// Refresh this far ahead of expiry so an in-flight request never races a token
/// going stale.
const REFRESH_MARGIN_MS: u64 = 300_000;

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

/// The store's path and document. `Ok(None)` means p1 has no store yet; `Err` means
/// the store exists but must not be used (its mode, or its shape).
fn load(locations: &Locations) -> Result<Option<(PathBuf, Value)>, String> {
    let Some(path) = locations.p1_store_path() else {
        return Ok(None);
    };
    if let Some(dir) = path.parent() {
        check_mode(dir, 0o700)?;
    }
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => return Err(format!("the p1 store {} could not be read", path.display())),
    };
    check_mode(&path, 0o600)?;
    let document: Value = serde_json::from_str(&text)
        .map_err(|_| format!("the p1 store {} is malformed", path.display()))?;
    if !document.is_object() {
        return Err(format!(
            "the p1 store {} is not a JSON object keyed by route",
            path.display()
        ));
    }
    Ok(Some((path, document)))
}

/// A mode that lets anyone but the owner in is refused, with the chmod to run.
fn check_mode(path: &Path, want: u32) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let Ok(metadata) = std::fs::metadata(path) else {
        return Ok(());
    };
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        return Err(format!(
            "{} is group/world-accessible (mode {mode:o}); chmod {want:o} it",
            path.display()
        ));
    }
    Ok(())
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
            if !crate::api_key::usable_key(key) {
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
            Ok(Some(EntryValue::Oauth {
                access,
                refresh: raw
                    .get("refresh")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                expires_ms: raw.get("expires").and_then(Value::as_u64),
                account_id: raw
                    .get("account_id")
                    .and_then(Value::as_str)
                    .map(str::to_string),
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
    if key.is_empty() {
        return Err(format!(
            "the key for route \"{route_id}\" is empty; nothing was written"
        ));
    }
    if !crate::api_key::usable_key(key) {
        return Err(format!(
            "the key for route \"{route_id}\" is not a header-safe token (printable ASCII, no \
             spaces); nothing was written"
        ));
    }
    let path = store_path(locations)?;
    check_writable(locations)?;
    let _lock = lock(&path).await.map_err(|error| error.message)?;
    let mut document = match load(locations)? {
        Some((_, document)) => document,
        None => Value::Object(serde_json::Map::new()),
    };
    if let Some(object) = document.as_object_mut() {
        object.insert(route_id.to_string(), json!({"type": "api_key", "key": key}));
    }
    write_atomic(&path, &encode(&document)).map_err(|error| error.message)
}

/// Remove one route's entry from p1's store (spec §6), leaving every other entry as
/// it was. `Ok(false)` means there was nothing to remove — a missing entry is
/// reported, not an error — and nothing is created for a route that has no store yet.
pub async fn remove(route_id: &str, locations: &Locations) -> Result<bool, String> {
    let path = store_path(locations)?;
    if !path.is_file() {
        return Ok(false);
    }
    check_writable(locations)?;
    let _lock = lock(&path).await.map_err(|error| error.message)?;
    // The file can be gone between the check and the lock: then there is nothing to
    // remove either.
    let Some((_, mut document)) = load(locations)? else {
        return Ok(false);
    };
    if document.get(route_id).is_none() {
        return Ok(false);
    }
    if let Some(object) = document.as_object_mut() {
        object.remove(route_id);
    }
    write_atomic(&path, &encode(&document)).map_err(|error| error.message)?;
    Ok(true)
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
        let kind = match self.dialect {
            OauthDialect::ClaudeCode => CredentialKind::ClaudeCodeOauth,
            OauthDialect::Codex => CredentialKind::CodexOauth,
        };
        match read_entry(&self.locations, &self.route_id, kind)? {
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
        let _lock = lock(&path).await?;
        let Some((_, document)) = load(&self.locations).map_err(auth)? else {
            return Err(auth(format!(
                "the p1 store has no entry for route \"{}\"",
                self.route_id
            )));
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
            auth(format!(
                "the p1 store oauth entry for route \"{}\" records no refresh token and its \
                 access token is expired",
                self.route_id
            ))
        })?;
        let response = self.post_refresh(&refresh_token).await?;
        // `refresh` must never hand back the credential the caller rejected.
        if rejected == Some(response.access.as_str()) {
            return Err(auth(
                "the token refresh returned the rejected token; log in again for this route",
            ));
        }
        let updated = merge(&document, &self.route_id, &response);
        write_atomic(&path, &updated)?;
        Ok(Credential {
            bearer: response.access,
            account_id,
        })
    }

    fn kind(&self) -> CredentialKind {
        match self.dialect {
            OauthDialect::ClaudeCode => CredentialKind::ClaudeCodeOauth,
            OauthDialect::Codex => CredentialKind::CodexOauth,
        }
    }

    async fn post_refresh(&self, refresh_token: &str) -> Result<Refreshed, ProviderError> {
        let response = self
            .transport
            .post(self.dialect.request(refresh_token))
            .await
            .map_err(|_| auth("the p1 store token refresh request failed"))?;
        if !(200..300).contains(&response.status) {
            return Err(auth(format!(
                "the p1 store token refresh failed with status {}",
                response.status
            )));
        }
        let bytes = read_all(response.body).await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|_| auth("the p1 store token refresh response is malformed"))?;
        self.dialect.parse(&value)
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

/// The entry with the refreshed fields replaced, every other key and every other
/// entry left exactly as it was.
fn merge(document: &Value, route_id: &str, response: &Refreshed) -> String {
    let mut document = document.clone();
    if let Some(entry) = document.get_mut(route_id).and_then(Value::as_object_mut) {
        entry.insert("access".to_string(), json!(response.access));
        if let Some(refresh) = &response.refresh {
            entry.insert("refresh".to_string(), json!(refresh));
        }
        entry.insert(
            "expires".to_string(),
            json!(now_ms().saturating_add(response.expires_in_secs.saturating_mul(1000))),
        );
    }
    encode(&document)
}

/// Take the advisory lock on the sibling `.lock` file, never blocking the thread.
async fn lock(path: &Path) -> Result<std::fs::File, ProviderError> {
    let mut lock_path = path.as_os_str().to_os_string();
    lock_path.push(".lock");
    let lock_path = PathBuf::from(lock_path);
    if let Some(parent) = lock_path.parent() {
        create_store_dir(parent)
            .map_err(|_| auth("the p1 store directory could not be created"))?;
    }
    let file = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .open(&lock_path)
        .map_err(|_| auth("the p1 store lock could not be opened"))?;
    lock_exclusive(file, LOCK_PATIENCE)
        .await
        .map_err(|_| auth("the p1 store lock could not be acquired"))
}

/// Create the store's directory 0700 when it is missing. The store is refused when
/// anyone but the owner can enter it, so the directory this crate creates must never
/// be the reason a later read refuses: a directory that already exists is left alone.
fn create_store_dir(dir: &Path) -> std::io::Result<()> {
    let existed = dir.is_dir();
    std::fs::create_dir_all(dir)?;
    if !existed {
        // Linux-only: 0700 for the directory that holds the store (spec §3).
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    Ok(())
}

/// Atomic write: a unique temp file beside the store, created 0600, renamed into
/// place. On any failure the original file is untouched.
fn write_atomic(path: &Path, contents: &str) -> Result<(), ProviderError> {
    if let Some(parent) = path.parent() {
        create_store_dir(parent)
            .map_err(|_| auth("the p1 store directory could not be created"))?;
    }
    let temp = unique_tmp_path(path);
    let written = (|| -> std::io::Result<()> {
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create_new(true);
        // Linux-only: the store is 0600 from creation, never briefly world-readable.
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        use std::io::Write;
        let mut file = options.open(&temp)?;
        file.write_all(contents.as_bytes())
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(auth("the p1 store could not be written"));
    }
    if std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(&temp);
        return Err(auth("the p1 store could not be replaced"));
    }
    Ok(())
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    path.with_extension(format!(
        "tmp-{}-{:09}-{counter}",
        std::process::id(),
        now_ms()
    ))
}

async fn read_all(mut body: ByteStream) -> Result<Vec<u8>, ProviderError> {
    use futures_util::StreamExt;
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(_) => {
                return Err(auth(
                    "the p1 store token refresh response could not be read",
                ));
            }
        }
    }
    Ok(bytes)
}

fn now_ms() -> u64 {
    crate::claude_code::system_clock()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    /// The directory and file this crate writes are private from creation: the
    /// store's own read check would refuse anything else (spec §3).
    #[test]
    fn the_directory_and_file_it_writes_are_private() {
        let scratch = tempfile::tempdir().unwrap();
        let dir = scratch.path().join("p1");
        let path = dir.join("auth.json");

        create_store_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o700);
        write_atomic(&path, "{}\n").unwrap();
        assert_eq!(mode(&path), 0o600);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}\n");

        // An existing directory keeps the mode its owner chose.
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o750)).unwrap();
        create_store_dir(&dir).unwrap();
        assert_eq!(mode(&dir), 0o750);
    }
}
