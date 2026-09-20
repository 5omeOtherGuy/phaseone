//! The Claude Code login as a credential source: the third source of the
//! `claude-code-oauth` chain (spec §2).
//!
//! The credential file layout, expiry unit, refresh request, token URL, client
//! id and scopes are taken from the donor's `src/mimir/auth/anthropic.rs`
//! source, never from any real file. The file is read fresh on every `access`;
//! a refresh happens only when the token is expired (5 minute margin) or after a
//! 401/403. Refresh tokens rotate, so a successful refresh is written back to
//! the same file under an advisory lock, preserving every unknown field, with a
//! temp-file-plus-rename that is 0600 from creation.
//!
//! A missing, unreadable or malformed file is an [`ProviderErrorKind::Authentication`]
//! error telling the user to log in with Claude Code. File contents are never
//! printed, logged or copied into an error. Where the file IS comes from
//! [`crate::Locations`]; this source never reads the process environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use futures_util::StreamExt;
use p1_contracts::{BoxFuture, ProviderError, ProviderErrorKind};
use p1_provider_http::{
    ByteStream, Credential, CredentialSource, HttpRequest, LOCK_PATIENCE, Transport, lock_exclusive,
};
use serde_json::{Value, json};

use crate::resolve::{Entry, Presence, SourceName};

pub(crate) const TOKEN_URL: &str = "https://platform.claude.com/v1/oauth/token";
pub(crate) const OAUTH_BETA: &str = "oauth-2025-04-20";
pub(crate) const CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";

/// Scopes requested when a stored credential records none (the runtime scopes
/// Claude Code itself uses on refresh; the login-only `org:create_api_key` scope
/// is deliberately not part of a refresh).
pub(crate) const DEFAULT_SCOPES: &str =
    "user:profile user:inference user:sessions:claude_code user:mcp_servers user:file_upload";

/// Refresh this far ahead of expiry so an in-flight request never races a token
/// going stale.
const REFRESH_MARGIN_MS: u64 = 300_000;

/// The Claude Code credential file as a [`CredentialSource`].
pub struct ClaudeCodeCredentials {
    path: PathBuf,
    transport: Arc<dyn Transport>,
    /// Injected so tests need no real clock. Milliseconds since the Unix epoch.
    clock: fn() -> u64,
}

impl ClaudeCodeCredentials {
    /// Read and refresh credentials at an explicit path through an explicit
    /// transport (the chain supplies both). Construction touches no file.
    pub fn at(path: PathBuf, transport: Arc<dyn Transport>) -> Self {
        Self {
            path,
            transport,
            clock: system_clock,
        }
    }

    /// Replace the clock (milliseconds since the Unix epoch). Tests inject a
    /// fixed clock so expiry needs no real time.
    pub fn with_clock(mut self, clock: fn() -> u64) -> Self {
        self.clock = clock;
        self
    }

    /// Read the file and build the current credential. A missing or malformed
    /// file is an authentication error (log in with Claude Code).
    fn read_stored(&self) -> Result<StoredCredentials, ProviderError> {
        let raw = std::fs::read_to_string(&self.path)
            .map_err(|_| auth_error(&self.path, "Claude Code credentials could not be read"))?;
        let document: Value = serde_json::from_str(&raw)
            .map_err(|_| auth_error(&self.path, "Claude Code credentials are malformed"))?;
        parse_credentials(&document, &self.path)
    }

    /// A token with no recorded expiry is used as-is: an older flat credential
    /// shape omits `expiresAt`, and a rejected one still heals through the
    /// forced-refresh path.
    fn is_fresh(&self, stored: &StoredCredentials) -> bool {
        if stored.access.is_empty() {
            return false;
        }
        match stored.expires_ms {
            Some(expires) => expires > (self.clock)().saturating_add(REFRESH_MARGIN_MS),
            None => true,
        }
    }

    /// Refresh under the advisory lock. Re-reads first: Claude Code itself shares
    /// this file, and a token someone else already rotated must be used instead
    /// of burning another rotation.
    async fn refresh_locked(&self, rejected: Option<&str>) -> Result<Credential, ProviderError> {
        let _lock = self.lock().await?;

        let raw = std::fs::read_to_string(&self.path)
            .map_err(|_| auth_error(&self.path, "Claude Code credentials could not be read"))?;
        let document: Value = serde_json::from_str(&raw)
            .map_err(|_| auth_error(&self.path, "Claude Code credentials are malformed"))?;
        let stored = parse_credentials(&document, &self.path)?;

        if self.is_fresh(&stored) && rejected.is_none_or(|rejected| stored.access != rejected) {
            return Ok(bearer(stored.access));
        }

        let refresh_token = stored.refresh.clone().ok_or_else(|| {
            auth_error(
                &self.path,
                "Claude Code credentials record no refreshToken and the access token is expired",
            )
        })?;
        let scope = scopes_for(stored.scopes.as_ref());
        let response = self.post_refresh(&refresh_token, &scope).await?;

        // `refresh` must never hand back the credential the caller rejected.
        if rejected == Some(response.access.as_str()) {
            return Err(auth_error(
                &self.path,
                "the token refresh returned the rejected token",
            ));
        }

        let updated = merge_document(&raw, &self.path, &stored, &response, (self.clock)())?;
        self.write_atomic(&updated)?;
        Ok(bearer(response.access))
    }

    async fn post_refresh(
        &self,
        refresh_token: &str,
        scope: &str,
    ) -> Result<RefreshResponse, ProviderError> {
        let payload = json!({
            "grant_type": "refresh_token",
            "refresh_token": refresh_token,
            "client_id": CLIENT_ID,
            "scope": scope,
        });
        let body = serde_json::to_vec(&payload).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "the Claude Code token refresh request could not be encoded",
            )
        })?;
        let request = HttpRequest {
            url: TOKEN_URL.to_string(),
            headers: vec![
                ("content-type".to_string(), "application/json".to_string()),
                ("anthropic-beta".to_string(), OAUTH_BETA.to_string()),
            ],
            body,
        };

        let response = self.transport.post(request).await.map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "the Claude Code token refresh request failed",
            )
        })?;
        if !(200..300).contains(&response.status) {
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                format!(
                    "the Claude Code token refresh failed with status {}",
                    response.status
                ),
            ));
        }

        let bytes = read_all(response.body).await?;
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "the Claude Code token refresh response is malformed",
            )
        })?;
        parse_refresh_response(&value)
    }

    /// Atomic write: a unique temp file in the same directory, created 0600 and
    /// renamed into place. On any failure the original file is untouched.
    fn write_atomic(&self, contents: &str) -> Result<(), ProviderError> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent).map_err(|_| {
                auth_error(
                    &self.path,
                    "the Claude Code credentials directory could not be created",
                )
            })?;
        }

        let temp = unique_tmp_path(&self.path);
        let written = (|| -> std::io::Result<()> {
            let mut options = std::fs::OpenOptions::new();
            options.write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            use std::io::Write;
            let mut file = options.open(&temp)?;
            file.write_all(contents.as_bytes())
        })();
        if written.is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(auth_error(
                &self.path,
                "Claude Code credentials could not be written",
            ));
        }
        if std::fs::rename(&temp, &self.path).is_err() {
            let _ = std::fs::remove_file(&temp);
            return Err(auth_error(
                &self.path,
                "Claude Code credentials could not be replaced",
            ));
        }
        Ok(())
    }

    /// Take the advisory lock on the sibling `.lock` file. The lock is released
    /// when the returned handle drops.
    async fn lock(&self) -> Result<std::fs::File, ProviderError> {
        let path = self.lock_path();
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .truncate(false)
            .open(&path)
            .map_err(|_| {
                auth_error(
                    &path,
                    "the Claude Code credentials lock could not be opened",
                )
            })?;
        // Never a blocking lock: the holder keeps it across its refresh request,
        // and on a current-thread runtime a blocked thread would stop that request
        // (and cancellation) for good.
        lock_exclusive(file, LOCK_PATIENCE).await.map_err(|_| {
            auth_error(
                &path,
                "the Claude Code credentials lock could not be acquired",
            )
        })
    }

    fn lock_path(&self) -> PathBuf {
        let name = self
            .path
            .file_name()
            .map(|name| name.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from(".credentials.json"));
        let mut name = name;
        name.push(".lock");
        self.path.with_file_name(name)
    }
}

impl CredentialSource for ClaudeCodeCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let stored = self.read_stored()?;
            if self.is_fresh(&stored) {
                return Ok(bearer(stored.access));
            }
            self.refresh_locked(None).await
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.refresh_locked(Some(&rejected.bearer)).await })
    }
}

/// Whether Claude Code's login file has an entry the chain can use. Reading the
/// file is unavoidable (a route's entry may be absent); no value is kept.
pub(crate) fn presence_at(path: &Path) -> Presence {
    match std::fs::read_to_string(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Presence::Absent,
        Err(_) => Presence::Unusable(format!(
            "Claude Code credentials at {} could not be read; run Claude Code login",
            path.display()
        )),
        Ok(raw) => match serde_json::from_str::<Value>(&raw) {
            Err(_) => Presence::Unusable(format!(
                "Claude Code credentials at {} are malformed; run Claude Code login",
                path.display()
            )),
            Ok(document) => match parse_credentials(&document, path) {
                Ok(_) => Presence::Present,
                Err(error) => Presence::Unusable(error.message),
            },
        },
    }
}

impl Entry for ClaudeCodeCredentials {
    fn name(&self) -> SourceName {
        SourceName::ClaudeCodeLogin
    }

    fn presence(&self) -> Presence {
        presence_at(&self.path)
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        CredentialSource::access(self)
    }

    fn rotated<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        CredentialSource::refresh(self, rejected)
    }
}

struct StoredCredentials {
    access: String,
    refresh: Option<String>,
    expires_ms: Option<u64>,
    /// The raw `scopes` value, preserved so the refresh requests the same scopes.
    scopes: Option<Value>,
}

struct RefreshResponse {
    access: String,
    refresh: Option<String>,
    expires_in_secs: u64,
    scope: Option<String>,
}

fn bearer(access: String) -> Credential {
    Credential {
        bearer: access,
        account_id: None,
    }
}

fn auth_error(path: &Path, reason: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        format!(
            "{reason}. Run Claude Code login, then ensure Claude Code credentials exist at {}",
            path.display()
        ),
    )
}

/// Both the nested (`{"claudeAiOauth":{…}}`) and the older flat shape are
/// accepted. Errors are redacted: the raw JSON never appears in a message.
fn parse_credentials(document: &Value, path: &Path) -> Result<StoredCredentials, ProviderError> {
    let oauth = document.get("claudeAiOauth").unwrap_or(document);
    let access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| auth_error(path, "Claude Code credentials are missing accessToken"))?;
    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let expires_ms = oauth.get("expiresAt").and_then(Value::as_u64);
    let scopes = oauth.get("scopes").cloned();
    Ok(StoredCredentials {
        access: access.to_string(),
        refresh,
        expires_ms,
        scopes,
    })
}

/// Space-joined scopes, falling back to the Claude Code defaults when the
/// credential records none (or only blanks).
fn scopes_for(scopes: Option<&Value>) -> String {
    match scopes {
        Some(Value::Array(items)) => {
            let scopes: Vec<&str> = items
                .iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|scope| !scope.is_empty())
                .collect();
            if scopes.is_empty() {
                DEFAULT_SCOPES.to_string()
            } else {
                scopes.join(" ")
            }
        }
        Some(Value::String(scopes)) if !scopes.trim().is_empty() => scopes.trim().to_string(),
        _ => DEFAULT_SCOPES.to_string(),
    }
}

/// Parse the token-refresh response. Field-shape errors name no values, let
/// alone the token.
fn parse_refresh_response(value: &Value) -> Result<RefreshResponse, ProviderError> {
    let access = value
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty());
    let expires_in = value.get("expires_in").and_then(Value::as_u64);
    let (Some(access), Some(expires_in_secs)) = (access, expires_in) else {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Claude Code token refresh response is missing required fields",
        ));
    };
    let refresh = value
        .get("refresh_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    let scope = value
        .get("scope")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|scope| !scope.is_empty())
        .map(str::to_string);
    Ok(RefreshResponse {
        access: access.to_string(),
        refresh,
        expires_in_secs,
        scope,
    })
}

/// Update the credential fields in place, preserving every other key and the
/// document's existing shape. A missing/non-object document becomes a fresh
/// nested envelope.
fn merge_document(
    existing: &str,
    path: &Path,
    stored: &StoredCredentials,
    response: &RefreshResponse,
    now_ms: u64,
) -> Result<String, ProviderError> {
    let mut document = match serde_json::from_str::<Value>(existing) {
        Ok(value) if value.is_object() => value,
        _ => json!({ "claudeAiOauth": {} }),
    };
    let root = document
        .as_object_mut()
        .expect("document is an object by construction");
    let target = if root.contains_key("claudeAiOauth") {
        root.get_mut("claudeAiOauth")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| {
                auth_error(path, "the Claude Code claudeAiOauth field is not an object")
            })?
    } else {
        root
    };

    target.insert("accessToken".to_string(), json!(response.access));
    // Keep the prior refresh token when the response rotates none (older
    // servers), and keep the existing scopes unless the response names new ones.
    let refresh = response.refresh.clone().or_else(|| stored.refresh.clone());
    if let Some(refresh) = refresh {
        target.insert("refreshToken".to_string(), json!(refresh));
    }
    target.insert(
        "expiresAt".to_string(),
        json!(now_ms.saturating_add(response.expires_in_secs.saturating_mul(1000))),
    );
    if let Some(scope) = &response.scope {
        let scopes: Vec<Value> = scope.split_whitespace().map(|scope| json!(scope)).collect();
        if !scopes.is_empty() {
            target.insert("scopes".to_string(), Value::Array(scopes));
        }
    }

    let mut encoded = serde_json::to_string_pretty(&document)
        .map_err(|_| auth_error(path, "Claude Code credentials could not be encoded"))?;
    encoded.push('\n');
    Ok(encoded)
}

async fn read_all(mut body: ByteStream) -> Result<Vec<u8>, ProviderError> {
    let mut bytes = Vec::new();
    while let Some(chunk) = body.next().await {
        match chunk {
            Ok(chunk) => bytes.extend_from_slice(&chunk),
            Err(_) => {
                return Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "the Claude Code token refresh response could not be read",
                ));
            }
        }
    }
    Ok(bytes)
}

fn unique_tmp_path(path: &Path) -> PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.subsec_nanos())
        .unwrap_or(0);
    path.with_extension(format!("tmp-{}-{nanos:09}-{counter}", std::process::id()))
}

/// The default clock: milliseconds since the Unix epoch.
pub(crate) fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
