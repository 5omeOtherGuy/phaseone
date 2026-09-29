//! The Codex CLI's own login as a credential source: the third source of the
//! `codex-oauth` chain (spec §2).
//!
//! p1 builds no login flow: it reuses the auth file [`crate::Locations`] points at,
//! exactly as the donor reused the CLI login. The refresh token rotates, so a
//! refresh is single-flight: the directory lock plus an advisory lock on a sibling
//! `.lock` file, a re-read under the lock, a staged 0600 write-back through the
//! checked, pinned directory ([`crate::credential_file`], issue #484) and an untouched
//! file when the refresh fails. A rotation that has sent its request runs to its
//! write-back even when the caller is cancelled, and never overwrites a login the
//! Codex CLI rotated meanwhile. `docs/design/providers.md` "Credentials" is the spec.
//! This source never reads the process environment.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use p1_contracts::{BoxFuture, ProviderError, ProviderErrorKind};
use p1_provider_http::{Credential, CredentialSource, HttpRequest, Transport};
use serde_json::Value;

use crate::api_key::usable_key;
use crate::credential_file::{CredentialDir, CredentialLock, DirKind, FileError, PublishError};
use crate::refresh_http::{self, RefreshIoError};
use crate::resolve::{Entry, Presence, SourceName};

pub(crate) const TOKEN_URL: &str = "https://auth.openai.com/oauth/token";
/// The Codex CLI's public OAuth client id (from its source; never a secret).
pub(crate) const CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";
const ACCOUNT_ID_CLAIM: &str = "https://api.openai.com/auth";
/// Refresh this long before the token actually expires.
const EXPIRY_MARGIN_SECS: u64 = 300;

/// Injected time source. The adapter formats `last_refresh` from this clock.
pub trait Clock: Send + Sync {
    fn now(&self) -> SystemTime;
}

struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> SystemTime {
        SystemTime::now()
    }
}

/// A [`CredentialSource`] over the Codex CLI auth file.
pub struct CodexCliCredentials {
    path: PathBuf,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
}

impl CodexCliCredentials {
    /// A source over an explicit auth file and transport (the chain supplies both).
    pub fn at(path: PathBuf, transport: Arc<dyn Transport>) -> Self {
        Self {
            path,
            transport,
            clock: Arc::new(SystemClock),
        }
    }

    /// Replace the clock (tests and deterministic hosts).
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = clock;
        self
    }

    fn name(&self) -> String {
        self.path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "auth.json".to_string())
    }

    /// The lock file older p1 processes take: `auth.json` → `auth.lock`.
    fn lock_name(&self) -> String {
        self.path
            .with_extension("lock")
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "auth.lock".to_string())
    }

    fn open_dir(&self) -> Result<CredentialDir, ProviderError> {
        open_login_dir(&self.path).map_err(|error| file_error(&self.path, error))
    }

    fn now(&self) -> u64 {
        epoch_seconds(self.clock.now())
    }

    /// Refresh under the cross-process lock. `stale` is the rejected bearer on
    /// the 401/403 path; `None` on the expiry path.
    async fn refresh_locked(&self, stale: Option<&str>) -> Result<Credential, ProviderError> {
        let dir = self.open_dir()?;
        let name = self.name();
        let lock = dir
            .lock(&self.lock_name())
            .await
            .map_err(|error| match error {
                FileError::Refused(reason) => refused(&reason),
                FileError::Missing | FileError::Io => lock_error(),
            })?;
        dir.recover(&name, |bytes| {
            serde_json::from_slice::<Value>(bytes)
                .is_ok_and(|document| tokens_from(&document).is_ok())
        });
        let raw = dir
            .read(&name)
            .map_err(|error| file_error(&self.path, error))?;
        let document = parse_document(&raw, &self.path)?;
        let tokens = tokens_from(&document)?;
        let expired = token_expired(&tokens.access_token, self.now())?;
        // A peer already rotated the rejected token: use its replacement when it is
        // still fresh, and never refresh the same rejected token twice.
        let reusable = !expired && stale.is_none_or(|stale| tokens.access_token != stale);
        if reusable {
            return Ok(credential_from(&tokens));
        }
        let refresh_token = tokens.refresh_token.clone().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::Authentication,
                "the Codex auth file has no refresh token; run `codex login`",
            )
        })?;
        let rotation = CodexRotation {
            dir,
            _lock: lock,
            name,
            path: self.path.clone(),
            transport: self.transport.clone(),
            clock: self.clock.clone(),
            reserve: raw.len(),
            refresh_token,
            stale: stale.map(str::to_string),
        };
        refresh_http::detached(rotation.run()).await
    }
}

/// One started rotation of the Codex login. It owns the opened directory and the
/// held lock, so it runs to its write-back even when the caller is cancelled.
struct CodexRotation {
    dir: CredentialDir,
    /// Held until the rotation is written back.
    _lock: CredentialLock,
    name: String,
    path: PathBuf,
    transport: Arc<dyn Transport>,
    clock: Arc<dyn Clock>,
    reserve: usize,
    refresh_token: String,
    stale: Option<String>,
}

impl CodexRotation {
    async fn run(self) -> Result<Credential, ProviderError> {
        // Staged, with its space reserved, before the refresh token is spent.
        let mut staging = self
            .dir
            .stage(&self.name, self.reserve * 2 + 4096)
            .map_err(|_| write_error())?;
        let refreshed = request_refresh(self.transport.as_ref(), &self.refresh_token).await?;

        // The Codex CLI shares this file and does not take p1's lock: when it rotated
        // the login while this request was out, its file is left alone.
        let latest = self
            .dir
            .read(&self.name)
            .map_err(|error| file_error(&self.path, error))?;
        let mut document = parse_document(&latest, &self.path)?;
        let unchanged = tokens_from(&document)
            .is_ok_and(|tokens| tokens.refresh_token.as_deref() == Some(&self.refresh_token));
        let usable = match &refreshed.access_token {
            Ok(access) if self.stale.as_deref() == Some(access.as_str()) => {
                Err("the token refresh returned the rejected token")
            }
            Ok(access) => Ok(access.clone()),
            Err(problem) => Err(*problem),
        };
        if !unchanged {
            return match usable {
                // This rotation's access token is valid; the file keeps the other one.
                Ok(access) => Ok(Credential {
                    account_id: tokens_from(&document)
                        .ok()
                        .and_then(|tokens| tokens.account_id)
                        .or_else(|| jwt_account_id(&access)),
                    bearer: access,
                }),
                Err(_) => Err(ProviderError::new(
                    ProviderErrorKind::Authentication,
                    "the Codex auth file changed while it was being refreshed; it was left as \
                     it is — retry",
                )),
            };
        }
        let Some(refresh_token) = refreshed.refresh_token.clone() else {
            // Nothing was rotated: nothing is written, the file stays byte-identical.
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                usable
                    .err()
                    .unwrap_or("token refresh response had no refresh token"),
            ));
        };
        apply_refresh(
            &mut document,
            usable.as_deref().ok(),
            &refresh_token,
            refreshed.id_token.as_deref(),
            self.clock.now(),
        );
        let bytes = serde_json::to_vec_pretty(&document).map_err(|_| write_error())?;
        if let Err(error) = staging.publish(&bytes) {
            let kept = !matches!(error, PublishError::NotDurable) && staging.keep_for_recovery();
            let reason = match error {
                PublishError::NotPublished(Some(reason)) => {
                    format!("failed to write the Codex auth file: {reason}")
                }
                PublishError::NotPublished(None) => "failed to write the Codex auth file".into(),
                PublishError::NotDurable => {
                    "the Codex auth file was written but could not be flushed to disk".into()
                }
            };
            return Err(ProviderError::new(
                ProviderErrorKind::Authentication,
                if kept {
                    format!(
                        "{reason}; the refreshed login was kept beside it and is used by the \
                         next refresh"
                    )
                } else {
                    reason
                },
            ));
        }
        let access = usable
            .map_err(|problem| ProviderError::new(ProviderErrorKind::Authentication, problem))?;
        let tokens = tokens_from(&document)?;
        debug_assert_eq!(tokens.access_token, access);
        Ok(credential_from(&tokens))
    }
}

async fn request_refresh(
    transport: &dyn Transport,
    refresh_token: &str,
) -> Result<RefreshedTokens, ProviderError> {
    let body = format!(
        "grant_type=refresh_token&refresh_token={}&client_id={}",
        percent_encode(refresh_token),
        percent_encode(CLIENT_ID),
    );
    let request = HttpRequest {
        url: TOKEN_URL.to_string(),
        headers: vec![
            (
                "Content-Type".to_string(),
                "application/x-www-form-urlencoded".to_string(),
            ),
            ("Accept".to_string(), "application/json".to_string()),
        ],
        body: body.into_bytes(),
    };
    // A failed status is reported at once; its body is never read.
    let body = refresh_http::exchange(transport, request)
        .await
        .map_err(|error| match error {
            RefreshIoError::TimedOut(error) => error,
            RefreshIoError::Status(status) => ProviderError::new(
                ProviderErrorKind::Authentication,
                format!("token refresh failed with http {status}"),
            ),
            RefreshIoError::TooLarge => ProviderError::new(
                ProviderErrorKind::Authentication,
                "token refresh response is too large to be one",
            ),
            RefreshIoError::Transport(error) => ProviderError::new(
                ProviderErrorKind::Authentication,
                format!("token refresh request failed: {}", error.0),
            ),
        })?;
    let value: Value = serde_json::from_slice(&body).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Authentication,
            "token refresh response was not valid JSON",
        )
    })?;
    let access_token = match non_empty_str(value.get("access_token")) {
        None => Err("token refresh response had no access token"),
        Some(access) if !usable_key(&access) => {
            Err("token refresh response had an access token that is not a header-safe token")
        }
        Some(access) => Ok(access),
    };
    Ok(RefreshedTokens {
        access_token,
        refresh_token: non_empty_str(value.get("refresh_token")),
        id_token: non_empty_str(value.get("id_token")),
    })
}

impl CredentialSource for CodexCliCredentials {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            // Read fresh every time: another program may have rotated the file.
            let document = read_document(&self.path)?;
            let tokens = tokens_from(&document)?;
            if token_expired(&tokens.access_token, self.now())? {
                return self.refresh_locked(None).await;
            }
            Ok(credential_from(&tokens))
        })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let document = read_document(&self.path)?;
            let tokens = tokens_from(&document)?;
            if tokens.access_token != rejected.bearer
                && !token_expired(&tokens.access_token, self.now())?
            {
                // Someone else refreshed between the rejection and this call.
                return Ok(credential_from(&tokens));
            }
            self.refresh_locked(Some(&rejected.bearer)).await
        })
    }
}

/// Whether the Codex auth file has an entry the chain can use. Reading the file is
/// unavoidable (a route's entry may be absent); no value is kept.
pub(crate) fn presence_at(path: &Path) -> Presence {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_string());
    let bytes = match open_login_dir(path).and_then(|dir| dir.read(&name)) {
        Ok(bytes) => bytes,
        Err(FileError::Missing) => return Presence::Absent,
        Err(FileError::Refused(reason)) => return Presence::Unusable(reason),
        Err(FileError::Io) => {
            return Presence::Unusable(format!(
                "the Codex auth file {} could not be read; run `codex login`",
                path.display()
            ));
        }
    };
    match serde_json::from_slice::<Value>(&bytes) {
        Err(_) => Presence::Unusable(format!(
            "the Codex auth file {} is not valid JSON; run `codex login`",
            path.display()
        )),
        Ok(document) => match tokens_from(&document)
            .and_then(|tokens| token_expired(&tokens.access_token, 0).map(|_| tokens))
        {
            Ok(_) => Presence::Present,
            Err(error) => Presence::Unusable(error.message),
        },
    }
}

impl Entry for CodexCliCredentials {
    fn name(&self) -> SourceName {
        SourceName::CodexLogin
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

/// A parsed view of the parts of the auth file this adapter uses. Unknown fields
/// stay in the [`Value`] document and are written back untouched.
struct Tokens {
    access_token: String,
    refresh_token: Option<String>,
    account_id: Option<String>,
}

/// A refresh response, each field checked on its own.
struct RefreshedTokens {
    access_token: Result<String, &'static str>,
    refresh_token: Option<String>,
    id_token: Option<String>,
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
        FileError::Refused(reason) => refused(&reason),
        FileError::Missing | FileError::Io => ProviderError::new(
            ProviderErrorKind::Authentication,
            format!(
                "cannot read the Codex auth file {}; run `codex login`",
                path.display()
            ),
        ),
    }
}

fn refused(reason: &str) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        format!("the Codex auth file is not used: {reason}"),
    )
}

fn read_document(path: &Path) -> Result<Value, ProviderError> {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "auth.json".to_string());
    let bytes = open_login_dir(path)
        .and_then(|dir| dir.read(&name))
        .map_err(|error| file_error(path, error))?;
    parse_document(&bytes, path)
}

fn parse_document(bytes: &[u8], path: &Path) -> Result<Value, ProviderError> {
    serde_json::from_slice(bytes).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::Authentication,
            format!(
                "the Codex auth file {} is not valid JSON; run `codex login`",
                path.display()
            ),
        )
    })
}

fn tokens_from(value: &Value) -> Result<Tokens, ProviderError> {
    let tokens = value
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(login_error)?;
    let access_token = non_empty_str(tokens.get("access_token")).ok_or_else(login_error)?;
    // Both values go into headers: anything a header cannot carry is refused here.
    if !usable_key(&access_token) {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Codex auth file holds an access token that is not a header-safe token; run \
             `codex login`",
        ));
    }
    let account_id =
        non_empty_str(tokens.get("account_id")).or_else(|| jwt_account_id(&access_token));
    if account_id.as_deref().is_some_and(|id| !usable_key(id)) {
        return Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Codex auth file holds an account id that is not a header-safe token; run \
             `codex login`",
        ));
    }
    Ok(Tokens {
        access_token,
        refresh_token: non_empty_str(tokens.get("refresh_token")),
        account_id,
    })
}

fn login_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        "no ChatGPT tokens in the Codex auth file; run `codex login` with ChatGPT \
         (an API key alone is not valid for this route)",
    )
}

fn credential_from(tokens: &Tokens) -> Credential {
    Credential {
        bearer: tokens.access_token.clone(),
        account_id: tokens.account_id.clone(),
    }
}

/// A non-empty string after trimming; whitespace alone is no token.
fn non_empty_str(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_string)
}

/// Write the rotation into the document: the rotated refresh token always, the new
/// access (and id) token when the response had a usable one. Without one, the old
/// access token stays: it is expired or rejected, so the next access refreshes again
/// with the rotated refresh token instead of losing the login.
fn apply_refresh(
    document: &mut Value,
    access: Option<&str>,
    refresh: &str,
    id_token: Option<&str>,
    now: SystemTime,
) {
    if let Some(tokens) = document.get_mut("tokens").and_then(Value::as_object_mut) {
        if let Some(access) = access {
            tokens.insert(
                "access_token".to_string(),
                Value::String(access.to_string()),
            );
            if let Some(id_token) = id_token {
                tokens.insert("id_token".to_string(), Value::String(id_token.to_string()));
            }
        }
        tokens.insert(
            "refresh_token".to_string(),
            Value::String(refresh.to_string()),
        );
    }
    if let Some(object) = document.as_object_mut() {
        object.insert(
            "last_refresh".to_string(),
            Value::String(format_rfc3339_utc(now)),
        );
    }
}

fn write_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        "failed to write the Codex auth file",
    )
}

fn lock_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Authentication,
        "failed to lock the Codex auth file for refresh",
    )
}

/// Percent-encode a form value by hand: unreserved characters pass through, a
/// space becomes `+`, everything else is `%XX` uppercase.
pub(crate) fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for byte in value.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => {
                use std::fmt::Write as _;
                let _ = write!(out, "%{byte:02X}");
            }
        }
    }
    out
}

fn epoch_seconds(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// Whether the token's `exp` claim is past (with the margin). An opaque token — no
/// decodable JWT payload, or a payload without `exp` — is treated as not expired;
/// the 401 path is the backstop. A payload whose `exp` is not a timestamp is an
/// error, never a silently fresh token.
fn token_expired(token: &str, now: u64) -> Result<bool, ProviderError> {
    match jwt_exp(token) {
        Ok(Some(exp)) => Ok(exp <= now.saturating_add(EXPIRY_MARGIN_SECS)),
        Ok(None) => Ok(false),
        Err(()) => Err(ProviderError::new(
            ProviderErrorKind::Authentication,
            "the Codex access token has an invalid expiry claim; run `codex login`",
        )),
    }
}

fn jwt_payload(token: &str) -> Option<Value> {
    let payload = token.split('.').nth(1)?;
    let bytes = base64url_decode(payload)?;
    serde_json::from_slice(&bytes).ok()
}

fn jwt_exp(token: &str) -> Result<Option<u64>, ()> {
    let Some(payload) = jwt_payload(token) else {
        return Ok(None);
    };
    match payload.get("exp") {
        None | Some(Value::Null) => Ok(None),
        Some(exp) => exp.as_u64().map(Some).ok_or(()),
    }
}

fn jwt_account_id(token: &str) -> Option<String> {
    jwt_payload(token)?
        .get(ACCOUNT_ID_CLAIM)?
        .get("chatgpt_account_id")?
        .as_str()
        .map(str::to_string)
}

/// Minimal base64url (no padding) decoder. A signature is never verified; the
/// payload is only read for the `exp` and account-id claims.
fn base64url_decode(input: &str) -> Option<Vec<u8>> {
    fn sextet(byte: u8) -> Option<u32> {
        match byte {
            b'A'..=b'Z' => Some(u32::from(byte - b'A')),
            b'a'..=b'z' => Some(u32::from(byte - b'a') + 26),
            b'0'..=b'9' => Some(u32::from(byte - b'0') + 52),
            b'-' => Some(62),
            b'_' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::new();
    let mut accumulator = 0u32;
    let mut bits = 0u32;
    for byte in input.bytes() {
        if byte == b'=' {
            break;
        }
        let value = sextet(byte)?;
        accumulator = (accumulator << 6) | value;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((accumulator >> bits) as u8);
        }
    }
    Some(out)
}

/// Format a `SystemTime` as RFC 3339 UTC without pulling in a date crate.
fn format_rfc3339_utc(time: SystemTime) -> String {
    let seconds = time
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .unwrap_or(0);
    let days = seconds.div_euclid(86_400);
    let remainder = seconds.rem_euclid(86_400);
    let (year, month, day) = civil_from_days(days);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        remainder / 3600,
        (remainder % 3600) / 60,
        remainder % 60
    )
}

/// Howard Hinnant's `civil_from_days`: days since the Unix epoch to a civil date.
fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = (z - era * 146_097) as u64;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let year = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let month = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::time::Duration;

    use p1_provider_http::testing::{BodyEnd, ScriptedResponse, ScriptedTransport};

    use super::*;

    struct FixedClock(SystemTime);

    impl Clock for FixedClock {
        fn now(&self) -> SystemTime {
            self.0
        }
    }

    fn base64url_encode(bytes: &[u8]) -> String {
        const ALPHABET: &[u8; 64] =
            b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
        let mut out = String::new();
        for chunk in bytes.chunks(3) {
            let b0 = chunk[0] as u32;
            let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
            let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
            let triple = (b0 << 16) | (b1 << 8) | b2;
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

    fn jwt_with_payload(payload: &Value) -> String {
        let header = base64url_encode(br#"{"alg":"none"}"#);
        let body = base64url_encode(payload.to_string().as_bytes());
        format!("{header}.{body}.signature")
    }

    fn jwt_expiring_in(seconds: i64) -> String {
        let exp = epoch_seconds(SystemTime::now()) as i64 + seconds;
        jwt_exp(exp)
    }

    fn jwt_exp(exp: i64) -> String {
        jwt_with_payload(&serde_json::json!({ "exp": exp }))
    }

    fn auth_json(access: &str, refresh: &str) -> Value {
        serde_json::json!({
            "OPENAI_API_KEY": null,
            "tokens": {
                "id_token": "old-id",
                "access_token": access,
                "refresh_token": refresh,
                "account_id": "acct_1",
            },
            "last_refresh": "2000-01-01T00:00:00Z",
            "unknown_field": { "keep": true },
        })
    }

    fn write_auth_file(path: &Path, value: &Value) {
        fs::write(path, serde_json::to_vec_pretty(value).unwrap()).unwrap();
    }

    fn refresh_response(value: &Value) -> ScriptedResponse {
        ScriptedResponse {
            status: 200,
            headers: vec![("content-type".to_string(), "application/json".to_string())],
            chunks: vec![serde_json::to_vec(value).unwrap()],
            end: BodyEnd::Eof,
        }
    }

    fn credentials(
        path: &Path,
        transport: &ScriptedTransport,
        clock: SystemTime,
    ) -> CodexCliCredentials {
        CodexCliCredentials::at(path.to_path_buf(), Arc::new(transport.clone()))
            .with_clock(Arc::new(FixedClock(clock)))
    }

    #[test]
    fn decodes_base64url_payloads() {
        assert_eq!(base64url_decode("aGk").unwrap(), b"hi".to_vec());
        assert_eq!(base64url_decode("aGk=").unwrap(), b"hi".to_vec());
        assert!(base64url_decode("!!!").is_none());
    }

    #[test]
    fn reads_the_account_id_from_the_jwt_claim() {
        let token = jwt_with_payload(&serde_json::json!({
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_jwt" }
        }));
        assert_eq!(jwt_account_id(&token).as_deref(), Some("acc_jwt"));
        assert_eq!(jwt_account_id("not-a-jwt"), None);
    }

    #[test]
    fn formats_rfc3339_utc_by_hand() {
        assert_eq!(
            format_rfc3339_utc(UNIX_EPOCH),
            "1970-01-01T00:00:00Z".to_string()
        );
        // 2024-02-29T12:34:56Z
        let leap = UNIX_EPOCH + Duration::from_secs(1_709_210_096);
        assert_eq!(format_rfc3339_utc(leap), "2024-02-29T12:34:56Z".to_string());
    }

    #[test]
    fn percent_encodes_form_values() {
        assert_eq!(percent_encode("abc-._~123"), "abc-._~123");
        assert_eq!(percent_encode("a b"), "a+b");
        assert_eq!(percent_encode("a/b?c&d"), "a%2Fb%3Fc%26d");
    }

    #[test]
    fn missing_tokens_tell_the_user_to_log_in() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_file(&path, &serde_json::json!({ "OPENAI_API_KEY": "sk-test" }));
        let credentials =
            CodexCliCredentials::at(path, Arc::new(ScriptedTransport::new(Vec::new())));
        let error = block_on(credentials.access()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert!(error.message.contains("codex login"));
    }

    #[test]
    fn account_id_falls_back_to_the_jwt_claim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let access = jwt_with_payload(&serde_json::json!({
            "exp": epoch_seconds(SystemTime::now()) + 3600,
            "https://api.openai.com/auth": { "chatgpt_account_id": "acc_jwt" }
        }));
        let mut value = auth_json(&access, "refresh-1");
        value["tokens"]["account_id"] = Value::Null;
        write_auth_file(&path, &value);
        let credentials =
            CodexCliCredentials::at(path, Arc::new(ScriptedTransport::new(Vec::new())));
        let credential = block_on(credentials.access()).unwrap();
        assert_eq!(credential.account_id.as_deref(), Some("acc_jwt"));
    }

    #[test]
    fn valid_token_is_used_without_a_refresh() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        let access = jwt_expiring_in(3600);
        write_auth_file(&path, &auth_json(&access, "refresh-1"));
        let transport = ScriptedTransport::new(Vec::new());
        let credentials = credentials(&path, &transport, SystemTime::now());
        let credential = block_on(credentials.access()).unwrap();
        assert_eq!(credential.bearer, access);
        assert_eq!(credential.account_id.as_deref(), Some("acct_1"));
        assert!(transport.requests().is_empty(), "no refresh was needed");
    }

    #[test]
    fn unparsable_expiry_is_treated_as_valid() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_file(&path, &auth_json("not-a-jwt", "refresh-1"));
        let transport = ScriptedTransport::new(Vec::new());
        let credentials = credentials(&path, &transport, SystemTime::now());
        assert!(block_on(credentials.access()).is_ok());
        assert!(transport.requests().is_empty());
    }

    #[test]
    fn expired_token_refreshes_and_preserves_unknown_fields() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        // The fixed clock below is 1_700_000_000, so the token is expired by it.
        write_auth_file(
            &path,
            &auth_json(&jwt_exp(1_700_000_000 - 10), "refresh-old"),
        );
        // A world-readable original is tightened by the atomic 0600 write-back.
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).unwrap();

        let transport = ScriptedTransport::new(vec![refresh_response(&serde_json::json!({
            "access_token": "new-access",
            "refresh_token": "refresh-new",
            "id_token": "new-id",
        }))]);
        let clock = UNIX_EPOCH + Duration::from_secs(1_700_000_000);
        let credentials = credentials(&path, &transport, clock);

        let credential = block_on(credentials.access()).unwrap();
        assert_eq!(credential.bearer, "new-access");
        assert_eq!(credential.account_id.as_deref(), Some("acct_1"));

        let requests = transport.requests();
        assert_eq!(requests.len(), 1);
        assert!(requests[0].body.starts_with(b"grant_type=refresh_token"));
        assert!(
            requests[0]
                .headers
                .iter()
                .any(|(name, value)| name == "Content-Type"
                    && value == "application/x-www-form-urlencoded")
        );

        let written: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(written["tokens"]["access_token"], "new-access");
        assert_eq!(written["tokens"]["refresh_token"], "refresh-new");
        assert_eq!(written["tokens"]["id_token"], "new-id");
        assert_eq!(written["unknown_field"]["keep"], true);
        assert_eq!(written["last_refresh"], "2023-11-14T22:13:20Z");
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            dir.path().join("auth.lock").exists(),
            "the advisory lock file is created for refresh"
        );
    }

    #[test]
    fn refresh_failure_leaves_the_file_byte_identical() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_file(&path, &auth_json(&jwt_expiring_in(-10), "refresh-old"));
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600)).unwrap();
        let before = fs::read(&path).unwrap();

        let transport = ScriptedTransport::new(vec![ScriptedResponse {
            status: 500,
            headers: Vec::new(),
            chunks: vec![b"upstream failure".to_vec()],
            end: BodyEnd::Eof,
        }]);
        let credentials = credentials(&path, &transport, SystemTime::now());
        let error = block_on(credentials.access()).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert_eq!(fs::read(&path).unwrap(), before);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn refresh_does_not_return_the_rejected_token() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_file(&path, &auth_json("rejected-token", "refresh-old"));

        // The server rotates the refresh token but refuses to mint a new access
        // token: the adapter must not fall back to the rejected bearer.
        let transport = ScriptedTransport::new(vec![refresh_response(&serde_json::json!({
            "access_token": "rejected-token",
            "refresh_token": "refresh-new",
        }))]);
        let credentials = credentials(&path, &transport, SystemTime::now());
        let rejected = Credential {
            bearer: "rejected-token".to_string(),
            account_id: Some("acct_1".to_string()),
        };
        let error = block_on(credentials.refresh(&rejected)).unwrap_err();
        assert_eq!(error.kind, ProviderErrorKind::Authentication);
        assert!(!error.message.contains("rejected-token"));
    }

    #[test]
    fn refresh_reuses_a_peers_rotated_token_without_a_request() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("auth.json");
        write_auth_file(&path, &auth_json(&jwt_expiring_in(3600), "refresh-new"));
        let transport = ScriptedTransport::new(Vec::new());
        let credentials = credentials(&path, &transport, SystemTime::now());
        let rejected = Credential {
            bearer: "old-rejected".to_string(),
            account_id: Some("acct_1".to_string()),
        };
        let credential = block_on(credentials.refresh(&rejected)).unwrap();
        assert_ne!(credential.bearer, "old-rejected");
        assert!(transport.requests().is_empty());
    }

    /// A tiny executor for the synchronous tests above; the async-path tests use
    /// `#[tokio::test]` through the driver instead.
    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
            .block_on(future)
    }
}
