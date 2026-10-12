//! A credential module's login file as a source of the chain (ADR-0040, ADR-0212): another
//! tool's own login, read fresh on every access and, for an OAuth login, refreshed and written
//! back to the same file.
//!
//! Everything here is native and the same for every module: the file is read, locked and
//! replaced through its checked, pinned directory ([`crate::credential_file`], issue #484); the
//! refresh is single-flight under the login's lock, re-reads the file first (the tool shares it
//! and may have rotated it), is sent through the bounded exchange ([`crate::refresh_http`]) only
//! after the module's request passed the token-origin check, runs to its write-back even when the
//! caller is cancelled, keeps a rotated refresh token even when the rest of the response is
//! unusable, and never overwrites a login the tool rotated meanwhile. The module only reads and
//! writes the file's format ([`ModuleMethod`]). No value reaches an error message.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::{BoxFuture, ProviderError, ProviderErrorKind};
use p1_provider_http::{Credential, CredentialSource, HttpRequest, Transport};

use crate::credential_file::{CredentialDir, CredentialLock, DirKind, FileError, PublishError};
use crate::locations::Locations;
use crate::method::BorrowedLogin;
use crate::module::{LoginTokens, ModuleMethod};
use crate::refresh_http::{self, RefreshIoError, system_clock};
use crate::resolve::{Entry, Presence, SourceName};

/// Refresh this far ahead of expiry so an in-flight request never races a token going stale.
const REFRESH_MARGIN_MS: u64 = refresh_http::REFRESH_MARGIN_MS;

/// Milliseconds since the Unix epoch.
pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// The login file of a method's module, in the route's `login_dir` or the default directory.
pub(crate) struct BorrowedFile {
    method: Arc<ModuleMethod>,
    login_dir: Option<String>,
}

impl BorrowedFile {
    pub(crate) fn new(method: Arc<ModuleMethod>, login_dir: Option<String>) -> Self {
        Self { method, login_dir }
    }

    fn path(&self, locations: &Locations) -> Option<PathBuf> {
        self.method
            .login()
            .and_then(|login| login.path(locations, self.login_dir.as_deref()))
    }
}

impl BorrowedLogin for BorrowedFile {
    fn name(&self, _locations: &Locations) -> SourceName {
        source_name(&self.method)
    }

    fn presence(&self, locations: &Locations) -> Presence {
        match self.path(locations) {
            None => Presence::Absent,
            Some(path) => presence_at(&self.method, &path, None),
        }
    }

    fn entry(&self, locations: &Locations, transport: Arc<dyn Transport>) -> Box<dyn Entry> {
        match self.path(locations) {
            Some(path) => Box::new(LoginFile::at(self.method.clone(), path, transport)),
            None => missing(source_name(&self.method)),
        }
    }
}

/// An API key another tool keeps: the entry `key` of the keyed login of the credential module
/// installed as `store` (`borrow = ["<store>:<key>"]`).
pub(crate) struct BorrowedKey {
    store: String,
    key: String,
}

impl BorrowedKey {
    pub(crate) fn new(store: String, key: String) -> Self {
        Self { store, key }
    }

    fn module(&self, locations: &Locations) -> Result<Arc<ModuleMethod>, String> {
        locations.methods().keyed_login(&self.store)
    }
}

impl BorrowedLogin for BorrowedKey {
    fn name(&self, locations: &Locations) -> SourceName {
        match self.module(locations) {
            Ok(method) => source_name(&method),
            Err(_) => SourceName::Login {
                name: format!("{} login", self.store),
                guidance: format!("install the credential module \"{}\"", self.store),
            },
        }
    }

    fn presence(&self, locations: &Locations) -> Presence {
        let method = match self.module(locations) {
            Ok(method) => method,
            Err(reason) => return Presence::Unusable(reason),
        };
        match method.login().and_then(|login| login.path(locations, None)) {
            None => Presence::Absent,
            Some(path) => presence_at(&method, &path, Some(&self.key)),
        }
    }

    fn entry(&self, locations: &Locations, _transport: Arc<dyn Transport>) -> Box<dyn Entry> {
        let method = match self.module(locations) {
            Ok(method) => method,
            Err(reason) => {
                return Box::new(Refused {
                    name: self.name(locations),
                    reason,
                });
            }
        };
        match method.login().and_then(|login| login.path(locations, None)) {
            Some(path) => Box::new(KeyFile {
                method,
                path,
                key: self.key.clone(),
            }),
            None => missing(source_name(&method)),
        }
    }
}

fn source_name(method: &ModuleMethod) -> SourceName {
    method.source_name().unwrap_or(SourceName::Login {
        name: format!("{} login", method.module_name()),
        guidance: format!("log in with {}", method.module_name()),
    })
}

/// Whether the login at `path` has an entry the chain can use. Reading the file is
/// unavoidable (a route's entry may be absent); no value is kept.
fn presence_at(method: &ModuleMethod, path: &Path, key: Option<&str>) -> Presence {
    let title = title(method);
    let raw = match open_login_dir(path).and_then(|dir| dir.read(&file_name(path))) {
        Ok(raw) => raw,
        Err(FileError::Missing) => return Presence::Absent,
        Err(FileError::Refused(reason)) => {
            return Presence::Unusable(format!("the {title} is not used: {reason}"));
        }
        Err(FileError::Io) => {
            return Presence::Unusable(format!(
                "the {title} at {} could not be read; {}",
                path.display(),
                guidance(method)
            ));
        }
    };
    match method.read(&raw, key) {
        Ok(Some(_)) => Presence::Present,
        // A keyed login without the key has no entry; any other login without a token is one
        // that cannot be used.
        Ok(None) if key.is_some() => Presence::Absent,
        Ok(None) => Presence::Unusable(unusable(method, path, "it holds no access token")),
        Err(reason) => Presence::Unusable(unusable(method, path, &reason)),
    }
}

/// A credential module's OAuth login file as a [`CredentialSource`]: read fresh on every
/// access, refreshed when the token is due (5 minute margin) or after a 401/403, and the
/// rotation written back to the same file.
pub struct LoginFile {
    method: Arc<ModuleMethod>,
    path: PathBuf,
    transport: Arc<dyn Transport>,
    clock: Clock,
}

impl LoginFile {
    /// The login of `method` at an explicit path, refreshed through an explicit transport (the
    /// chain supplies both). Construction touches no file.
    pub fn at(method: Arc<ModuleMethod>, path: PathBuf, transport: Arc<dyn Transport>) -> Self {
        Self {
            method,
            path,
            transport,
            clock: Arc::new(system_clock),
        }
    }

    /// Replace the clock (milliseconds since the Unix epoch): tests need no real time.
    pub fn with_clock(mut self, clock: impl Fn() -> u64 + Send + Sync + 'static) -> Self {
        self.clock = Arc::new(clock);
        self
    }

    fn name(&self) -> String {
        file_name(&self.path)
    }

    fn open_dir(&self) -> Result<CredentialDir, ProviderError> {
        open_login_dir(&self.path).map_err(|error| self.file_error(error))
    }

    /// The login as the file holds it now.
    fn read_tokens(&self, raw: &[u8]) -> Result<LoginTokens, ProviderError> {
        match self.method.read(raw, None) {
            Ok(Some(tokens)) => Ok(tokens),
            Ok(None) => Err(self.auth_error("it holds no access token")),
            Err(reason) => Err(self.auth_error(&reason)),
        }
    }

    fn read_stored(&self) -> Result<LoginTokens, ProviderError> {
        let raw = self
            .open_dir()?
            .read(&self.name())
            .map_err(|error| self.file_error(error))?;
        self.read_tokens(&raw)
    }

    /// Refresh under the login's lock. Re-reads first: the tool itself shares this file, and a
    /// token someone else already rotated is used instead of burning another rotation.
    async fn refresh_locked(&self, rejected: Option<&str>) -> Result<Credential, ProviderError> {
        let login = self.method.login().expect("a login file has a login");
        let dir = self.open_dir()?;
        let name = self.name();
        let lock = dir
            .lock(&login.lock_name())
            .await
            .map_err(|error| match error {
                FileError::Refused(reason) => self.refused(&reason),
                FileError::Missing | FileError::Io => {
                    self.auth_error("its lock could not be acquired")
                }
            })?;
        let method = self.method.clone();
        dir.recover(&name, |bytes| {
            method
                .read(bytes, None)
                .is_ok_and(|tokens| tokens.is_some())
        });
        let raw = dir.read(&name).map_err(|error| self.file_error(error))?;
        let stored = self.read_tokens(&raw)?;
        let now = (self.clock)();
        if fresh(&stored, now) && rejected.is_none_or(|rejected| stored.access_token() != rejected)
        {
            return Ok(credential(&stored));
        }
        let refresh_token = stored.refresh.clone().ok_or_else(|| {
            self.auth_error(if rejected.is_some() {
                "it records no refresh token, so the rejected access token cannot be replaced"
            } else {
                "it records no refresh token and the access token is expired"
            })
        })?;
        // Built, and its origin checked, before anything is staged or sent.
        let request = self
            .method
            .refresh_request(&refresh_token, Some(&raw))
            .map_err(|reason| self.auth_error(&reason))?;
        let rotation = Rotation {
            dir,
            _lock: lock,
            reserve: raw.len(),
            baseline: (raw, stored),
            started: tokio::time::Instant::now(),
            name,
            path: self.path.clone(),
            method: self.method.clone(),
            transport: self.transport.clone(),
            clock: self.clock.clone(),
            request,
            refresh_token,
            rejected: rejected.map(str::to_string),
        };
        refresh_http::detached(rotation.run()).await
    }

    fn file_error(&self, error: FileError) -> ProviderError {
        file_error(&self.method, &self.path, error)
    }

    fn refused(&self, reason: &str) -> ProviderError {
        auth(format!("the {} is not used: {reason}", title(&self.method)))
    }

    fn auth_error(&self, reason: &str) -> ProviderError {
        auth(unusable(&self.method, &self.path, reason))
    }
}

impl LoginTokens {
    fn access_token(&self) -> &str {
        self.access.as_deref().unwrap_or_default()
    }
}

/// Due for refresh: a token with no recorded expiry is used as it is, and a rejected one still
/// heals through the forced-refresh path.
fn fresh(tokens: &LoginTokens, now_ms: u64) -> bool {
    tokens
        .expires_at_ms
        .is_none_or(|expires| expires > now_ms.saturating_add(REFRESH_MARGIN_MS))
}

fn credential(tokens: &LoginTokens) -> Credential {
    Credential {
        bearer: tokens.access_token().to_string(),
        account_id: tokens.account_id.clone(),
    }
}

/// One started rotation of a login file. It owns the opened directory and the held lock, so it
/// runs to its write-back even when the caller is cancelled.
struct Rotation {
    dir: CredentialDir,
    /// Held until the rotation is written back.
    _lock: CredentialLock,
    reserve: usize,
    /// The file and its login as this rotation started from them: where the rotated tokens are
    /// kept when the file cannot be read back after the request.
    baseline: (Vec<u8>, LoginTokens),
    /// When the refresh began: its time bounds count from here.
    started: tokio::time::Instant,
    name: String,
    path: PathBuf,
    method: Arc<ModuleMethod>,
    transport: Arc<dyn Transport>,
    clock: Clock,
    request: HttpRequest,
    refresh_token: String,
    rejected: Option<String>,
}

impl Rotation {
    async fn run(self) -> Result<Credential, ProviderError> {
        let auth_error = |reason: &str| auth(unusable(&self.method, &self.path, reason));
        // Staged, with its space reserved, before the refresh token is spent.
        let mut staging = self
            .dir
            .stage(&self.name, self.reserve * 2 + 4096)
            .map_err(|_| auth_error("it could not be prepared for the refreshed login"))?;
        // The lifetime counts from when the token was minted, never from when a slow response
        // finally arrived.
        let sent_at = (self.clock)();
        let title = title(&self.method);
        let body =
            refresh_http::exchange(self.transport.as_ref(), self.request.clone(), self.started)
                .await
                .map_err(|error| match error {
                    RefreshIoError::TimedOut(error) => error,
                    RefreshIoError::Status(status) => auth(format!(
                        "the {title} token refresh failed with status {status}"
                    )),
                    RefreshIoError::TooLarge => auth(format!(
                        "the {title} token refresh response is too large to be one"
                    )),
                    RefreshIoError::Transport(_) => {
                        auth(format!("the {title} token refresh request failed"))
                    }
                })?;
        let granted = self
            .method
            .granted(&body, sent_at)
            .map_err(|reason| auth_error(&reason))?;
        let now = (self.clock)();
        let usable: Result<String, String> = match (&granted.access, &granted.expires_at_ms) {
            (crate::module::Checked::Ok(access), crate::module::Checked::Ok(_))
                if self.rejected.as_deref() == Some(access.as_str()) =>
            {
                Err("the token refresh returned the rejected token".into())
            }
            (crate::module::Checked::Ok(_), crate::module::Checked::Ok(Some(expires)))
                if *expires <= now =>
            {
                Err("the token refresh returned a token that is already expired".into())
            }
            (crate::module::Checked::Ok(access), crate::module::Checked::Ok(_)) => {
                Ok(access.clone())
            }
            (crate::module::Checked::Err(problem), _)
            | (_, crate::module::Checked::Err(problem)) => Err(problem.clone()),
        };
        // This rotation's access token with ITS account: the account of the login it started
        // from, else the one the new token names — never the account of a login written
        // meanwhile.
        let own = |access: String| Credential {
            bearer: access,
            account_id: self
                .baseline
                .1
                .account_id
                .clone()
                .or_else(|| granted.account_id.clone()),
        };

        // The tool shares this file and does not take p1's lock: when it rotated the login
        // while this request was out, its file is left alone.
        let read_back = self
            .dir
            .read_versioned(&self.name)
            .map_err(|error| file_error(&self.method, &self.path, error))
            .and_then(|(latest, version)| {
                let tokens = match self.method.read(&latest, None) {
                    Ok(Some(tokens)) => tokens,
                    Ok(None) => return Err(auth_error("it holds no access token")),
                    Err(reason) => return Err(auth_error(&reason)),
                };
                Ok((latest, version, tokens))
            });
        let (latest, version, latest_tokens) = match read_back {
            Ok(read) => read,
            // The file cannot be read back, but the server may have rotated the refresh token
            // already: the rotation is kept beside the file, computed from the file it started
            // from, and the next refresh adopts it.
            Err(reason) => {
                let kept = granted.refresh.is_some()
                    && self
                        .method
                        .merge(&self.baseline.0, &body, sent_at, now, usable.is_ok())
                        .is_ok_and(|updated| staging.keep(&updated));
                return match usable {
                    Ok(access) => Ok(own(access)),
                    Err(_) if kept => Err(ProviderError::new(
                        reason.kind,
                        format!(
                            "the refreshed {title} was kept beside {}; the next refresh adopts it \
                             unless the file is changed first; {}",
                            self.path.display(),
                            reason.message
                        ),
                    )),
                    Err(_) => Err(reason),
                };
            }
        };
        if latest_tokens.refresh.as_deref() != Some(self.refresh_token.as_str()) {
            return match usable {
                // This rotation's access token is valid; the file keeps the other one.
                Ok(access) => Ok(own(access)),
                Err(_) => Err(auth_error(
                    "it changed while it was being refreshed and was left as it is — retry",
                )),
            };
        }
        if usable.is_err() && granted.refresh.is_none() {
            // Nothing was rotated: nothing is written, the file stays byte-identical.
            return Err(auth_error(&usable.err().unwrap_or_default()));
        }

        let updated = self
            .method
            .merge(&latest, &body, sent_at, now, usable.is_ok())
            .map_err(|reason| auth_error(&reason))?;
        staging.expect(version);
        if let Err(error) = staging.publish(&updated) {
            let kept = error.keeps() && staging.keep_for_recovery();
            let reason = match error {
                PublishError::NotPublished(Some(reason)) | PublishError::Changed(reason) => {
                    format!("it could not be replaced: {reason}")
                }
                PublishError::NotPublished(None) => "it could not be replaced".to_string(),
                PublishError::NotDurable => {
                    "it was replaced but could not be flushed to disk".to_string()
                }
            };
            let reason = if kept {
                format!(
                    "{reason}; the refreshed login was kept beside it and is used by the next \
                     refresh"
                )
            } else {
                reason
            };
            return Err(auth_error(&reason));
        }
        let access = usable.map_err(|problem| auth_error(&problem))?;
        // The credential as the file now records it, account included.
        match self.method.read(&updated, None) {
            Ok(Some(tokens)) if tokens.access.as_deref() == Some(access.as_str()) => {
                Ok(credential(&tokens))
            }
            _ => Ok(own(access)),
        }
    }
}

impl CredentialSource for LoginFile {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            let stored = self.read_stored()?;
            if fresh(&stored, (self.clock)()) {
                return Ok(credential(&stored));
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

impl Entry for LoginFile {
    fn name(&self) -> SourceName {
        source_name(&self.method)
    }

    fn presence(&self) -> Presence {
        presence_at(&self.method, &self.path, None)
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

/// One entry of a keyed login: an API key another tool keeps, read fresh on every access,
/// never refreshed and never written.
pub struct KeyFile {
    method: Arc<ModuleMethod>,
    path: PathBuf,
    key: String,
}

impl std::fmt::Debug for KeyFile {
    /// Prints no path, no key and no entry name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("KeyFile { <redacted> }")
    }
}

impl KeyFile {
    /// The entry `key` of the keyed login of `method` at `path`.
    pub fn at(method: Arc<ModuleMethod>, path: PathBuf, key: &str) -> Self {
        Self {
            method,
            path,
            key: key.to_string(),
        }
    }

    /// The key, `None` when the file or the entry is absent, the reason when the entry exists
    /// but cannot be used.
    fn load(&self) -> Result<Option<String>, String> {
        let raw = match open_login_dir(&self.path).and_then(|dir| dir.read(&file_name(&self.path)))
        {
            Ok(raw) => raw,
            Err(FileError::Missing) => return Ok(None),
            Err(FileError::Refused(reason)) => {
                return Err(format!("the {} is not used: {reason}", title(&self.method)));
            }
            Err(FileError::Io) => {
                return Err(format!("the {} could not be read", title(&self.method)));
            }
        };
        match self.method.read(&raw, Some(&self.key)) {
            Ok(tokens) => Ok(tokens.and_then(|tokens| tokens.access)),
            Err(reason) => Err(format!("the {}: {reason}", title(&self.method))),
        }
    }

    fn credential(key: String) -> Credential {
        Credential {
            bearer: key,
            account_id: None,
        }
    }
}

impl Entry for KeyFile {
    fn name(&self) -> SourceName {
        source_name(&self.method)
    }

    fn presence(&self) -> Presence {
        match self.load() {
            Ok(Some(_)) => Presence::Present,
            Ok(None) => Presence::Absent,
            Err(reason) => Presence::Unusable(reason),
        }
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            match self.load() {
                Ok(Some(key)) => Ok(Self::credential(key)),
                Ok(None) => Err(auth(format!(
                    "no {} entry for \"{}\"; set the documented key environment variable or {}",
                    title(&self.method),
                    self.key,
                    guidance(&self.method)
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
            match self.load() {
                Ok(Some(key)) if key != rejected.bearer => Ok(Self::credential(key)),
                Ok(_) => Err(auth(format!(
                    "the {} key was rejected; update the CLI login or the documented key \
                     environment variable",
                    title(&self.method)
                ))),
                Err(reason) => Err(auth(reason)),
            }
        })
    }
}

impl CredentialSource for KeyFile {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Entry::current(self)
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Entry::rotated(self, rejected)
    }
}

/// The login's title as a person reads it (`Claude Code login`).
fn title(method: &ModuleMethod) -> String {
    method.login().map_or_else(
        || format!("{} login", method.module_name()),
        |login| login.title.clone(),
    )
}

fn guidance(method: &ModuleMethod) -> String {
    method
        .login()
        .map_or_else(String::new, |login| login.guidance.clone())
}

/// Why the login at `path` cannot be used, and what to do about it.
fn unusable(method: &ModuleMethod, path: &Path, reason: &str) -> String {
    format!(
        "the {} at {} cannot be used: {reason}; {}",
        title(method),
        path.display(),
        guidance(method)
    )
}

fn file_name(path: &Path) -> String {
    path.file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default()
}

/// The login directory of `path`, checked and opened.
fn open_login_dir(path: &Path) -> Result<CredentialDir, FileError> {
    let dir = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    CredentialDir::open(dir, DirKind::Borrowed)
}

fn file_error(method: &ModuleMethod, path: &Path, error: FileError) -> ProviderError {
    match error {
        FileError::Missing | FileError::Io => auth(unusable(method, path, "it could not be read")),
        FileError::Refused(reason) => auth(format!("the {} is not used: {reason}", title(method))),
    }
}

fn auth(message: String) -> ProviderError {
    ProviderError::new(ProviderErrorKind::Authentication, message)
}

/// The entry of a borrowed source the host has no location for.
fn missing(name: SourceName) -> Box<dyn Entry> {
    Box::new(Missing { name })
}

/// A source the host has no location for (no home, no such directory): it can never have an
/// entry, so it is skipped rather than guessed at.
struct Missing {
    name: SourceName,
}

impl Entry for Missing {
    fn name(&self) -> SourceName {
        self.name.clone()
    }

    fn presence(&self) -> Presence {
        Presence::Absent
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move {
            Err(auth(format!(
                "{} has no location on this system; set HOME or the variable that names it",
                self.name
            )))
        })
    }

    fn rotated<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        self.current()
    }
}

/// A borrowed source whose credential module cannot be used: never an entry, always the reason.
struct Refused {
    name: SourceName,
    reason: String,
}

impl Entry for Refused {
    fn name(&self) -> SourceName {
        self.name.clone()
    }

    fn presence(&self) -> Presence {
        Presence::Unusable(self.reason.clone())
    }

    fn current<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { Err(auth(self.reason.clone())) })
    }

    fn rotated<'a>(
        &'a self,
        _rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        self.current()
    }
}
