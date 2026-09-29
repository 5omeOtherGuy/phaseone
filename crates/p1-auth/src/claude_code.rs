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
//!
//! The file is read, locked and replaced through its checked, pinned directory
//! ([`crate::credential_file`], issue #484): a symlinked, foreign or oversized file
//! is refused, and so is a login directory someone else can write to. A rotation
//! that has sent its request runs to its write-back even when the caller is
//! cancelled, keeps a rotated refresh token even when the rest of the response is
//! unusable, and never overwrites a login Claude Code rotated meanwhile.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::{BoxFuture, ProviderError, ProviderErrorKind};
use p1_provider_http::{Credential, CredentialSource, HttpRequest, Transport};
use serde_json::{Value, json};

use crate::api_key::usable_key;
use crate::credential_file::{CredentialDir, CredentialLock, DirKind, FileError, PublishError};
use crate::refresh_http::{self, RefreshIoError};
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
const REFRESH_MARGIN_MS: u64 = refresh_http::REFRESH_MARGIN_MS;

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

    /// The file's name inside its directory.
    fn name(&self) -> String {
        file_name(&self.path)
    }

    /// The login directory, checked and opened.
    fn open_dir(&self) -> Result<CredentialDir, ProviderError> {
        open_login_dir(&self.path).map_err(|error| file_error(&self.path, error))
    }

    /// Read the file and build the current credential. A missing or malformed
    /// file is an authentication error (log in with Claude Code).
    fn read_stored(&self) -> Result<StoredCredentials, ProviderError> {
        let dir = self.open_dir()?;
        let raw = dir
            .read(&self.name())
            .map_err(|error| file_error(&self.path, error))?;
        let document: Value = serde_json::from_slice(&raw)
            .map_err(|_| auth_error(&self.path, "Claude Code credentials are malformed"))?;
        parse_credentials(&document, &self.path)
    }

    /// A token with no recorded expiry is used as-is: an older flat credential
    /// shape omits `expiresAt`, and a rejected one still heals through the
    /// forced-refresh path.
    fn is_fresh(&self, stored: &StoredCredentials) -> bool {
        fresh(stored, (self.clock)())
    }

    /// Refresh under the advisory lock. Re-reads first: Claude Code itself shares
    /// this file, and a token someone else already rotated must be used instead
    /// of burning another rotation.
    async fn refresh_locked(&self, rejected: Option<&str>) -> Result<Credential, ProviderError> {
        let dir = self.open_dir()?;
        let name = self.name();
        let lock = dir
            .lock(&format!("{name}.lock"))
            .await
            .map_err(|error| match error {
                FileError::Refused(reason) => refused(&reason),
                FileError::Missing | FileError::Io => auth_error(
                    &self.path,
                    "the Claude Code credentials lock could not be acquired",
                ),
            })?;
        dir.recover(&name, |bytes| {
            serde_json::from_slice::<Value>(bytes)
                .is_ok_and(|document| parse_credentials(&document, &self.path).is_ok())
        });

        let raw = dir
            .read(&name)
            .map_err(|error| file_error(&self.path, error))?;
        let document: Value = serde_json::from_slice(&raw)
            .map_err(|_| auth_error(&self.path, "Claude Code credentials are malformed"))?;
        let stored = parse_credentials(&document, &self.path)?;

        if self.is_fresh(&stored) && rejected.is_none_or(|rejected| stored.access != rejected) {
            return Ok(bearer(stored.access));
        }

        let refresh_token = stored.refresh.clone().ok_or_else(|| {
            auth_error(
                &self.path,
                if rejected.is_some() {
                    "Claude Code credentials record no refreshToken, so the rejected access \
                     token cannot be replaced"
                } else {
                    "Claude Code credentials record no refreshToken and the access token is expired"
                },
            )
        })?;
        let rotation = ClaudeRotation {
            dir,
            _lock: lock,
            baseline: (raw.clone(), stored.clone()),
            name,
            path: self.path.clone(),
            transport: self.transport.clone(),
            clock: self.clock,
            reserve: raw.len(),
            scope: scopes_for(stored.scopes.as_ref()),
            refresh_token,
            rejected: rejected.map(str::to_string),
        };
        refresh_http::detached(rotation.run()).await
    }
}

/// One started rotation of the Claude Code login. It owns the opened directory and
/// the held lock, so it runs to its write-back even when the caller is cancelled.
struct ClaudeRotation {
    dir: CredentialDir,
    /// Held until the rotation is written back.
    _lock: CredentialLock,
    /// The file and its credentials as this rotation started from them: where the
    /// rotated tokens are kept when the file cannot be read back after the request.
    baseline: (Vec<u8>, StoredCredentials),
    name: String,
    path: PathBuf,
    transport: Arc<dyn Transport>,
    clock: fn() -> u64,
    reserve: usize,
    scope: String,
    refresh_token: String,
    rejected: Option<String>,
}

impl ClaudeRotation {
    async fn run(self) -> Result<Credential, ProviderError> {
        // Staged, with its space reserved, before the refresh token is spent.
        let mut staging = self
            .dir
            .stage(&self.name, self.reserve * 2 + 4096)
            .map_err(|_| {
                auth_error(
                    &self.path,
                    "Claude Code credentials could not be prepared for the refreshed login",
                )
            })?;
        // The lifetime counts from when the token was minted, never from when a slow
        // response finally arrived.
        let sent_at = (self.clock)();
        let response =
            post_refresh(self.transport.as_ref(), &self.refresh_token, &self.scope).await?;
        let usable = match (&response.access, &response.lifetime_ms) {
            (Ok(access), Ok(_)) if self.rejected.as_deref() == Some(access.as_str()) => {
                Err("the token refresh returned the rejected token")
            }
            (Ok(access), Ok(lifetime)) => Ok((access.clone(), sent_at.saturating_add(*lifetime))),
            (Err(problem), _) | (_, Err(problem)) => Err(*problem),
        };

        // Claude Code shares this file and does not take p1's lock: when it rotated
        // the login while this request was out, its file is left alone.
        let read_back = self
            .dir
            .read_versioned(&self.name)
            .map_err(|error| file_error(&self.path, error));
        let (latest, version, latest_stored) = match read_back.and_then(|(latest, version)| {
            let stored = serde_json::from_slice::<Value>(&latest)
                .map_err(|_| auth_error(&self.path, "Claude Code credentials are malformed"))
                .and_then(|document| parse_credentials(&document, &self.path))?;
            Ok((latest, version, stored))
        }) {
            Ok(read) => read,
            // The file cannot be read back, but the server may have rotated the refresh
            // token already: the rotation is kept beside the file, computed from the
            // file it started from, and the next refresh adopts it.
            Err(reason) => return self.keep_unreadable(&mut staging, &response, usable, reason),
        };
        let latest_stored = Some(latest_stored);
        let Some(latest_stored) = latest_stored
            .filter(|latest| latest.refresh.as_deref() == Some(self.refresh_token.as_str()))
        else {
            return match usable {
                // This rotation's access token is valid; the file keeps the other one.
                Ok((access, _)) => Ok(bearer(access)),
                Err(_) => Err(auth_error(
                    &self.path,
                    "Claude Code credentials changed while they were being refreshed; they \
                     were left as they are — retry",
                )),
            };
        };
        if usable.is_err() && response.refresh.is_none() {
            // Nothing was rotated: nothing is written, the file stays byte-identical.
            return Err(auth_error(&self.path, usable.err().unwrap_or_default()));
        }

        let updated = merge_document(
            &latest,
            &self.path,
            &latest_stored,
            &response,
            usable
                .as_ref()
                .ok()
                .map(|(access, expires)| (access.as_str(), *expires)),
        )?;
        staging.expect(version);
        if let Err(error) = staging.publish(updated.as_bytes()) {
            let kept = error.keeps() && staging.keep_for_recovery();
            let reason = match error {
                PublishError::NotPublished(Some(reason)) | PublishError::Changed(reason) => reason,
                PublishError::NotPublished(None) => {
                    "Claude Code credentials could not be replaced".to_string()
                }
                PublishError::NotDurable => {
                    "Claude Code credentials were replaced but could not be flushed to disk"
                        .to_string()
                }
            };
            let reason = if kept {
                format!(
                    "{reason}; the refreshed login was kept beside the file and is used by the next refresh"
                )
            } else {
                reason
            };
            return Err(auth_error(&self.path, &reason));
        }
        let (access, _) = usable.map_err(|problem| auth_error(&self.path, problem))?;
        Ok(bearer(access))
    }
}

impl ClaudeRotation {
    /// The file could not be read back after the request: keep what the rotation would
    /// have written over the file it started from, and use a usable access token.
    fn keep_unreadable(
        &self,
        staging: &mut crate::credential_file::Staging<'_>,
        response: &RefreshResponse,
        usable: Result<(String, u64), &str>,
        reason: ProviderError,
    ) -> Result<Credential, ProviderError> {
        let (baseline, stored) = &self.baseline;
        let kept = response.refresh.is_some()
            && merge_document(
                baseline,
                &self.path,
                stored,
                response,
                usable
                    .as_ref()
                    .ok()
                    .map(|(access, expires)| (access.as_str(), *expires)),
            )
            .is_ok_and(|updated| staging.keep(updated.as_bytes()));
        match usable {
            Ok((access, _)) => Ok(bearer(access)),
            Err(_) if kept => Err(ProviderError::new(
                reason.kind,
                format!(
                    "the refreshed Claude Code login was kept beside {}; the next refresh adopts it unless \
                     the file is changed first; {}",
                    self.path.display(),
                    reason.message
                ),
            )),
            Err(_) => Err(reason),
        }
    }
}

/// POST the refresh and read its response; every error names no value.
async fn post_refresh(
    transport: &dyn Transport,
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
    let bytes = refresh_http::exchange(transport, request)
        .await
        .map_err(|error| match error {
            RefreshIoError::TimedOut(error) => error,
            RefreshIoError::Status(status) => ProviderError::new(
                ProviderErrorKind::Authentication,
                format!("the Claude Code token refresh failed with status {status}"),
            ),
            RefreshIoError::TooLarge => ProviderError::new(
                ProviderErrorKind::Authentication,
                "the Claude Code token refresh response is too large to be one",
            ),
            RefreshIoError::Transport(_) => ProviderError::new(
                ProviderErrorKind::Authentication,
                "the Claude Code token refresh request failed",
            ),
        })?;
    let value: Value = serde_json::from_slice(&bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Claude Code token refresh response is malformed",
        )
    })?;
    Ok(parse_refresh_response(&value))
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
    let unreadable = || {
        Presence::Unusable(format!(
            "Claude Code credentials at {} could not be read; run Claude Code login",
            path.display()
        ))
    };
    let raw = match open_login_dir(path).and_then(|dir| dir.read(&file_name(path))) {
        Ok(raw) => raw,
        Err(FileError::Missing) => return Presence::Absent,
        Err(FileError::Refused(reason)) => return Presence::Unusable(reason),
        Err(FileError::Io) => return unreadable(),
    };
    match serde_json::from_slice::<Value>(&raw) {
        Err(_) => Presence::Unusable(format!(
            "Claude Code credentials at {} are malformed; run Claude Code login",
            path.display()
        )),
        Ok(document) => match parse_credentials(&document, path) {
            Ok(_) => Presence::Present,
            Err(error) => Presence::Unusable(error.message),
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

#[derive(Clone)]
pub(crate) struct StoredCredentials {
    pub(crate) access: String,
    pub(crate) refresh: Option<String>,
    pub(crate) expires_ms: Option<u64>,
    /// The raw `scopes` value, preserved so the refresh requests the same scopes.
    scopes: Option<Value>,
}

/// A refresh response, each field checked on its own: a rotated refresh token is
/// kept even when the rest of the response cannot be used.
struct RefreshResponse {
    access: Result<String, &'static str>,
    refresh: Option<String>,
    lifetime_ms: Result<u64, &'static str>,
    scope: Option<String>,
}

fn bearer(access: String) -> Credential {
    Credential {
        bearer: access,
        account_id: None,
    }
}

fn fresh(stored: &StoredCredentials, now_ms: u64) -> bool {
    if stored.access.is_empty() {
        return false;
    }
    match stored.expires_ms {
        Some(expires) => expires > now_ms.saturating_add(REFRESH_MARGIN_MS),
        None => true,
    }
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| ".credentials.json".to_string())
}

/// The login directory of `path`, checked and opened.
fn open_login_dir(path: &Path) -> Result<CredentialDir, FileError> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    CredentialDir::open(dir, DirKind::Borrowed)
}

fn file_error(path: &Path, error: FileError) -> ProviderError {
    match error {
        FileError::Missing | FileError::Io => {
            auth_error(path, "Claude Code credentials could not be read")
        }
        FileError::Refused(reason) => refused(&reason),
    }
}

fn refused(reason: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        format!("Claude Code credentials are not used: {reason}"),
    )
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
pub(crate) fn parse_credentials(
    document: &Value,
    path: &Path,
) -> Result<StoredCredentials, ProviderError> {
    let oauth = document.get("claudeAiOauth").unwrap_or(document);
    let access = oauth
        .get("accessToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .ok_or_else(|| auth_error(path, "Claude Code credentials are missing accessToken"))?;
    // The token goes into a header: anything a header cannot carry is refused here.
    if !usable_key(access) {
        return Err(auth_error(
            path,
            "Claude Code credentials hold an accessToken that is not a header-safe token",
        ));
    }
    let refresh = oauth
        .get("refreshToken")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .map(str::to_string);
    // Absent or null is "no expiry" (an older flat shape); anything but a millisecond
    // timestamp is an error, never a silently fresh token.
    let expires_ms = match oauth.get("expiresAt") {
        None | Some(Value::Null) => None,
        Some(value) => Some(value.as_u64().ok_or_else(|| {
            auth_error(
                path,
                "Claude Code credentials have an invalid expiresAt (not a millisecond timestamp)",
            )
        })?),
    };
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

/// Parse the token-refresh response, each field on its own. Field-shape errors name
/// no values, let alone the token.
fn parse_refresh_response(value: &Value) -> RefreshResponse {
    let text = |field: &str| {
        value
            .get(field)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|text| !text.is_empty())
            .map(str::to_string)
    };
    let access = match text("access_token") {
        None => Err("the Claude Code token refresh response is missing required fields"),
        Some(access) if !usable_key(&access) => Err(
            "the Claude Code token refresh response holds an access token that is not a \
             header-safe token",
        ),
        Some(access) => Ok(access),
    };
    let lifetime_ms = match value.get("expires_in").and_then(Value::as_u64) {
        None => Err("the Claude Code token refresh response is missing required fields"),
        Some(seconds) => refresh_http::lifetime_ms(seconds).ok_or(
            "the Claude Code token refresh response granted a token lifetime too short to use",
        ),
    };
    RefreshResponse {
        access,
        refresh: text("refresh_token"),
        lifetime_ms,
        scope: text("scope"),
    }
}

/// Update the credential fields in place, preserving every other key and the
/// document's existing shape. `usable` is the new access token and its expiry; when
/// the response had none worth keeping, the old access token stays and is marked
/// expired so the next access refreshes with the rotated refresh token.
fn merge_document(
    existing: &[u8],
    path: &Path,
    stored: &StoredCredentials,
    response: &RefreshResponse,
    usable: Option<(&str, u64)>,
) -> Result<String, ProviderError> {
    let mut document = match serde_json::from_slice::<Value>(existing) {
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

    match usable {
        Some((access, expires_ms)) => {
            target.insert("accessToken".to_string(), json!(access));
            target.insert("expiresAt".to_string(), json!(expires_ms));
        }
        None => {
            target.insert("expiresAt".to_string(), json!(0));
        }
    }
    // Keep the prior refresh token when the response rotates none (older
    // servers), and keep the existing scopes unless the response names new ones.
    let refresh = response.refresh.clone().or_else(|| stored.refresh.clone());
    if let Some(refresh) = refresh {
        target.insert("refreshToken".to_string(), json!(refresh));
    }
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

/// The default clock: milliseconds since the Unix epoch.
pub(crate) fn system_clock() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}
