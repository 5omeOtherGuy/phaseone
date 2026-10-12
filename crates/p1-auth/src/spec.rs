//! The `[credential]` table of a route file (spec §2): a REFERENCE to where a key
//! is read from, never the key itself. The file format is unchanged from the host's
//! former `CredentialRef`; only the owning crate moved.

use serde::Deserialize;

use crate::method::CredentialMethod;

/// A route's credential method (ADR-0211): a handle on one of the [`methods`] this build has,
/// parsed from the `kind` (or `method`) a route or account file spells. What a method means —
/// its checks, its store entry, the logins it borrows — is the method's own
/// ([`CredentialMethod`]); this handle only names it.
///
/// [`methods`]: crate::methods
#[derive(Clone, Copy)]
pub struct CredentialKind(&'static dyn CredentialMethod);

impl CredentialKind {
    /// The `api-key` method.
    pub const API_KEY: CredentialKind = CredentialKind(&crate::method::ApiKeyMethod);
    /// The `none` method.
    pub const NONE: CredentialKind = CredentialKind(&crate::method::NoneMethod);

    /// The handle of `method`.
    pub const fn of(method: &'static dyn CredentialMethod) -> Self {
        Self(method)
    }

    /// The method itself.
    pub fn method(self) -> &'static dyn CredentialMethod {
        self.0
    }

    /// The spelling a route file uses for this kind.
    pub fn name(self) -> &'static str {
        self.0.name()
    }

    /// The kind as `p1 login --list` shows it: the route-file spelling, plus, for a
    /// route that sends no credential, what that means for the operator.
    pub fn label(self) -> &'static str {
        self.0.label()
    }
}

impl PartialEq for CredentialKind {
    fn eq(&self, other: &Self) -> bool {
        self.name() == other.name()
    }
}

impl Eq for CredentialKind {}

impl std::fmt::Debug for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "CredentialKind({})", self.name())
    }
}

impl<'de> Deserialize<'de> for CredentialKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        match crate::method::method(&name) {
            Some(method) => Ok(Self(method)),
            None => {
                let known: Vec<&str> = crate::methods()
                    .iter()
                    .map(|method| method.name())
                    .collect();
                Err(serde::de::Error::custom(format!(
                    "credential method \"{name}\" is not one this p1 has ({}); a method an \
                     optional part implements is available only in a build with that part",
                    known.join(", ")
                )))
            }
        }
    }
}

/// The credential files a route may borrow from (ADR-0040: read-only reuse of an
/// existing CLI login). A build without the `subscriptions` feature has none, so its
/// routes borrow nothing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BorrowStore {
    /// The OpenCode CLI's shared data directory.
    #[cfg(feature = "subscriptions")]
    Opencode,
    /// The Pi CLI's agent directory.
    #[cfg(feature = "subscriptions")]
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

#[cfg(not(feature = "subscriptions"))]
impl<'de> Deserialize<'de> for BorrowSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Err(serde::de::Error::custom(format!(
            "borrow source \"{text}\": borrowed CLI logins are {}",
            crate::WITHOUT_SUBSCRIPTIONS
        )))
    }
}

#[cfg(feature = "subscriptions")]
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
    /// The credential method. An account file spells it `method` (ADR-0139); `kind`
    /// stays the route file's legacy spelling of the same field.
    #[serde(alias = "method")]
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
    /// The Claude Code config directory whose login this route borrows (ADR-0074),
    /// as the file writes it: an absolute path or a directory below the home (`~/<dir>`,
    /// never `~` itself), expanded against the home directory when it is used. Only a
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
        if self.login_dir.is_some() && !self.kind.method().takes_login_dir() {
            let takers: Vec<String> = crate::methods()
                .iter()
                .filter(|method| method.takes_login_dir())
                .map(|method| format!("\"{}\"", method.name()))
                .collect();
            if takers.is_empty() {
                return Err(format!(
                    "`[credential]` names a `login_dir`; borrowed login directories are {}",
                    crate::WITHOUT_SUBSCRIPTIONS
                ));
            }
            return Err(format!(
                "`[credential]` kind \"{}\" also names a `login_dir`; only a {} route borrows a \
                 login directory — delete the `login_dir` line",
                self.kind.name(),
                takers.join(" or ")
            ));
        }
        if self.store_only && !self.borrow.is_empty() {
            return Err(
                "`[credential]` sets `store_only` and also lists `borrow` entries; a \
                 store-only route reads only its environment variable and p1's store"
                    .into(),
            );
        }
        self.kind.method().validate(self)
    }
}

#[cfg(feature = "subscriptions")]
fn known_stores() -> &'static str {
    "opencode, pi"
}

fn is_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}
