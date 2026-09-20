//! The `[credential]` table of a route file (spec §2): a REFERENCE to where a key
//! is read from, never the key itself. The file format is unchanged from the host's
//! former `CredentialRef`; only the owning crate moved.

use serde::Deserialize;

/// The credential kinds a route file may name. Each kind selects the sources the
/// chain tries, in the order of the table in spec §2.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CredentialKind {
    /// An API key: the environment variable, then p1's store, then the borrowed
    /// CLI logins the file lists.
    ApiKey,
    /// The Claude Code CLI login: the environment variable (a bearer token), then
    /// p1's store, then Claude Code's own credentials file.
    ClaudeCodeOauth,
    /// The Codex CLI login: the environment variable (a bearer token), then p1's
    /// store, then the Codex CLI's own auth file.
    CodexOauth,
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

/// A credential file a route borrows a key from, in the order the file lists them.
/// Parsed from `"<store>:<key>"`; the paths behind each store live in
/// [`crate::Locations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowSource {
    pub store: BorrowStore,
    /// The provider key inside that store's credential file.
    pub key: String,
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
pub struct CredentialSpec {
    pub kind: CredentialKind,
    /// The environment variable that precedes every other source.
    #[serde(default)]
    pub env: Option<String>,
    /// Borrowed store entries, tried in this order after the store.
    #[serde(default)]
    pub borrow: Vec<BorrowSource>,
}

impl CredentialSpec {
    /// Check the reference itself. An API key is documented by the environment
    /// variable that holds it, so `api-key` without one is a load error; the two
    /// OAuth kinds take an optional one (spec §2).
    pub fn validate(&self) -> Result<(), String> {
        if let Some(env) = &self.env
            && !is_env_var_name(env)
        {
            return Err(format!(
                "`[credential]` env \"{env}\" is not an environment variable name"
            ));
        }
        if self.kind == CredentialKind::ApiKey && self.env.is_none() {
            return Err(
                "`[credential]` kind \"api-key\" names no environment variable; write \
                 `env = \"…\"` with the documented variable for this route"
                    .into(),
            );
        }
        Ok(())
    }
}

fn known_stores() -> &'static str {
    "opencode, pi"
}

fn is_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}
