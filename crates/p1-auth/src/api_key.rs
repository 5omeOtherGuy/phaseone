//! Read-only API key reuse from a borrowed CLI login file (ADR-0040).
//!
//! One entry of another tool's credential file: OpenCode's `{"type":"api","key":…}`
//! and Pi's `{"type":"api_key","key":…}`. No shell commands, no refresh endpoint and
//! no credential writes — the file is re-read on every `access`, so a key another
//! program rotated is picked up without a restart. Pi permits command-backed keys
//! (`"!command"`); executing configuration is outside a credential source, so such
//! an entry is refused rather than run.

use std::path::{Path, PathBuf};

use p1_contracts::{BoxFuture, ProviderError};
use p1_provider_http::{Credential, CredentialSource};
use serde_json::Value;

use crate::resolve::{Entry, Presence, SourceName};
use crate::{BorrowStore, auth};

/// One borrowed CLI login's API key.
pub struct SubscriptionCredentials {
    path: PathBuf,
    /// The provider key inside that store's credential file.
    key: String,
    store: BorrowStore,
}

impl std::fmt::Debug for SubscriptionCredentials {
    /// Prints no path, no key and no entry name.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("SubscriptionCredentials { <redacted> }")
    }
}

impl SubscriptionCredentials {
    /// One borrow source of a route's `[credential]` table.
    pub(crate) fn at(path: PathBuf, key: &str, store: BorrowStore) -> Self {
        Self {
            path,
            key: key.to_string(),
            store,
        }
    }

    /// Explicit location for isolated tests; uses no process environment.
    pub fn from_file(path: PathBuf, provider: &str, opencode_format: bool) -> Self {
        let store = if opencode_format {
            BorrowStore::Opencode
        } else {
            BorrowStore::Pi
        };
        Self::at(path, provider, store)
    }

    /// The key this login holds, `None` when the file or the entry is absent, and
    /// the reason when the entry exists but cannot be used.
    fn load(&self) -> Result<Option<String>, String> {
        load_at(&self.path, &self.key, self.store)
    }

    fn store_name(&self) -> &'static str {
        store_name(self.store)
    }

    fn credential(&self, key: String) -> Credential {
        Credential {
            bearer: key,
            account_id: None,
        }
    }
}

/// Whether one borrowed login holds an entry for `key`, for [`crate::describe`].
pub(crate) fn presence_at(path: &Path, key: &str, store: BorrowStore) -> Presence {
    match load_at(path, key, store) {
        Ok(Some(_)) => Presence::Present,
        Ok(None) => Presence::Absent,
        Err(reason) => Presence::Unusable(reason),
    }
}

/// The key this login holds, `None` when the file or the entry is absent, and the
/// reason when the entry exists but cannot be used.
fn load_at(path: &Path, key: &str, store: BorrowStore) -> Result<Option<String>, String> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(_) => {
            return Err(format!(
                "cannot read the {} credential file",
                store_name(store)
            ));
        }
    };
    let data: Value = serde_json::from_slice(&bytes).map_err(|_| {
        format!(
            "the {} credential file is not valid JSON",
            store_name(store)
        )
    })?;
    let Some(entry) = data.get(key) else {
        return Ok(None);
    };
    if entry.get("type").and_then(Value::as_str) != Some(entry_type(store)) {
        return Err(format!(
            "the {} login holds no API key for \"{key}\"",
            store_name(store)
        ));
    }
    let value = entry
        .get("key")
        .and_then(Value::as_str)
        .ok_or_else(|| format!("the {} login entry has no key", store_name(store)))?;
    // Pi permits command-backed keys; executing configuration is outside this
    // credential source.
    if value.starts_with('!') {
        return Err(
            "command-backed keys are unsupported; set the documented key environment variable"
                .into(),
        );
    }
    if !usable_key(value) {
        return Err(format!(
            "the {} login entry for \"{key}\" holds no usable key",
            store_name(store)
        ));
    }
    Ok(Some(value.to_string()))
}

fn store_name(store: BorrowStore) -> &'static str {
    match store {
        BorrowStore::Opencode => "opencode",
        BorrowStore::Pi => "pi",
    }
}

impl Entry for SubscriptionCredentials {
    fn name(&self) -> SourceName {
        match self.store {
            BorrowStore::Opencode => SourceName::OpencodeLogin,
            BorrowStore::Pi => SourceName::PiLogin,
        }
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
                Ok(Some(key)) => Ok(self.credential(key)),
                Ok(None) => Err(auth(format!(
                    "no {} login entry for \"{}\"; set the documented key environment variable or \
                     log in with the {} CLI",
                    self.store_name(),
                    self.key,
                    self.store_name()
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
                Ok(Some(key)) if key != rejected.bearer => Ok(self.credential(key)),
                Ok(_) => Err(auth(format!(
                    "the {} login key was rejected; update the CLI login or the documented key \
                     environment variable",
                    self.store_name()
                ))),
                Err(reason) => Err(auth(reason)),
            }
        })
    }
}

impl CredentialSource for SubscriptionCredentials {
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

/// The entry type each store writes for an API key.
fn entry_type(store: BorrowStore) -> &'static str {
    match store {
        BorrowStore::Opencode => "api",
        BorrowStore::Pi => "api_key",
    }
}

/// A key is a token a header can carry: non-empty, printable ASCII, no spaces.
pub(crate) fn usable_key(value: &str) -> bool {
    !value.is_empty() && value.bytes().all(|b| (33..=126).contains(&b))
}
