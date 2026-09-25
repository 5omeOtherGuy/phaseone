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
    /// NO credential: an egress proxy injects the provider's credential after the
    /// request leaves the process (issue #134). Nothing is loaded or required, and
    /// every adapter sends no authentication header for such a route.
    None,
}

impl CredentialKind {
    /// The spelling a route file uses for this kind.
    pub fn name(self) -> &'static str {
        match self {
            CredentialKind::ApiKey => "api-key",
            CredentialKind::ClaudeCodeOauth => "claude-code-oauth",
            CredentialKind::CodexOauth => "codex-oauth",
            CredentialKind::None => "none",
        }
    }

    /// The kind as `p1 login --list` shows it: the route-file spelling, plus, for a
    /// route that sends no credential, what that means for the operator.
    pub fn label(self) -> &'static str {
        match self {
            CredentialKind::None => "none (proxy-injected)",
            kind => kind.name(),
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
    /// Self-contained credential policy (ADR-0061): the chain is the documented
    /// environment variable and p1's own store ONLY. No other tool's login file is
    /// opened, so a Claude/Codex route cannot silently fall back to the CLI that
    /// happens to be installed. Absent (the default) keeps the legacy chain, where
    /// the two OAuth kinds add the CLI's own login after p1's store.
    #[serde(default)]
    pub store_only: bool,
    /// The Claude Code config directory whose login this route borrows (ADR-0075),
    /// as the file writes it: an absolute path or one that starts with `~/`, which is
    /// expanded against the home directory when it is used. Only a
    /// `claude-code-oauth` route may name one. Absent keeps the default directory
    /// (`$CLAUDE_CONFIG_DIR`, else `~/.claude`). It is also the directory
    /// `p1 login <route> --from-claude-code` imports from when no directory is given.
    #[serde(default)]
    pub login_dir: Option<String>,
}

impl CredentialSpec {
    /// Check the reference itself. An API key is documented by the environment
    /// variable that holds it, so `api-key` without one is a load error; the two
    /// OAuth kinds take an optional one (spec §2). A `none` route names NO source at
    /// all — it reads nothing — so naming one is a contradiction and a load error.
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
        if self.kind == CredentialKind::None {
            if self.env.is_some() {
                return Err(
                    "`[credential]` kind \"none\" also names an environment variable; a route \
                     that sends no credential reads no variable — delete the `env` line"
                        .into(),
                );
            }
            if !self.borrow.is_empty() {
                return Err(
                    "`[credential]` kind \"none\" also lists `borrow` entries; a route that \
                     sends no credential reads no borrowed login"
                        .into(),
                );
            }
            if self.store_only {
                return Err(
                    "`[credential]` kind \"none\" also sets `store_only`; a route that sends no \
                     credential reads no store — delete the `store_only` line"
                        .into(),
                );
            }
        }
        if let Some(dir) = &self.login_dir {
            if self.kind != CredentialKind::ClaudeCodeOauth {
                return Err(format!(
                    "`[credential]` kind \"{}\" also names a `login_dir`; only a \
                     \"claude-code-oauth\" route borrows a Claude Code login directory — delete \
                     the `login_dir` line",
                    self.kind.name()
                ));
            }
            if !(dir == "~" || dir.starts_with("~/") || dir.starts_with('/')) {
                return Err(format!(
                    "`[credential]` login_dir \"{dir}\" is neither an absolute path nor one that \
                     starts with `~/`"
                ));
            }
        }
        if self.store_only && !self.borrow.is_empty() {
            return Err(
                "`[credential]` sets `store_only` and also lists `borrow` entries; a \
                 store-only route reads only its environment variable and p1's store"
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
