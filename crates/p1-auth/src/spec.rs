//! The `[credential]` table of a route file (spec §2): a REFERENCE to where a key
//! is read from, never the key itself. The file format is unchanged from the host's
//! former `CredentialRef`; only the owning crate moved.

use std::sync::Arc;

use serde::Deserialize;

use crate::method::{ApiKeyMethod, CredentialMethod, NoneMethod};

/// A route's credential method (ADR-0211, ADR-0212), parsed from the `kind` (or `method`) a
/// route or account file spells: one of the core's two, or the name of a credential module,
/// which [`crate::Methods`] resolves against what is installed. What a method means — its
/// checks, its store entry, the logins it borrows — is the method's own ([`CredentialMethod`]);
/// this handle only names it.
#[derive(Clone)]
pub enum CredentialKind {
    /// A method of p1's core.
    Core(CoreMethod),
    /// A credential module's method, by the name routes give it.
    Module(Arc<str>),
}

/// The methods of p1's core.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreMethod {
    /// [`ApiKeyMethod`].
    ApiKey,
    /// [`NoneMethod`].
    None,
}

impl CredentialKind {
    /// The `api-key` method.
    pub const API_KEY: CredentialKind = CredentialKind::Core(CoreMethod::ApiKey);
    /// The `none` method.
    pub const NONE: CredentialKind = CredentialKind::Core(CoreMethod::None);

    /// The method a credential module provides under `name`.
    pub fn module(name: &str) -> Self {
        Self::Module(Arc::from(name))
    }

    /// The core method, when this is one.
    pub fn core(&self) -> Option<Arc<dyn CredentialMethod>> {
        match self {
            Self::Core(CoreMethod::ApiKey) => Some(Arc::new(ApiKeyMethod)),
            Self::Core(CoreMethod::None) => Some(Arc::new(NoneMethod)),
            Self::Module(_) => None,
        }
    }

    /// The spelling a route file uses for this kind.
    pub fn name(&self) -> &str {
        match self {
            Self::Core(CoreMethod::ApiKey) => "api-key",
            Self::Core(CoreMethod::None) => "none",
            Self::Module(name) => name,
        }
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

impl std::fmt::Display for CredentialKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.name())
    }
}

impl<'de> Deserialize<'de> for CredentialKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        match name.as_str() {
            "api-key" => Ok(Self::API_KEY),
            "none" => Ok(Self::NONE),
            // Any other method is a credential module's, resolved against what is installed
            // when the route is used (ADR-0212); here only its spelling is checked.
            other if is_module_name(other) => Ok(Self::module(other)),
            other => Err(serde::de::Error::custom(format!(
                "credential method \"{other}\" is not a method name (lowercase letters, digits \
                 and `-`)"
            ))),
        }
    }
}

/// A name a credential module is installed under: lowercase ASCII letters, digits and `-`,
/// starting with a letter.
pub fn is_module_name(name: &str) -> bool {
    name.bytes().next().is_some_and(|b| b.is_ascii_lowercase())
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
}

/// A keyed login a route borrows an API key from (ADR-0040: read-only reuse of an existing CLI
/// login), in the order the file lists them. Parsed from `"<store>:<key>"`: the store is the
/// name of the credential module that reads that login file (ADR-0212), the key the entry
/// inside it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BorrowSource {
    /// The credential module whose keyed login holds the key.
    pub store: String,
    /// The provider key inside that store's credential file.
    pub key: String,
}

impl<'de> Deserialize<'de> for BorrowSource {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::Error;
        let text = String::deserialize(deserializer)?;
        let (store, key) = text.split_once(':').ok_or_else(|| {
            D::Error::custom(format!(
                "borrow source \"{text}\" is not `<store>:<key>`, the store a credential module \
                 that reads a keyed login"
            ))
        })?;
        if !is_module_name(store) {
            return Err(D::Error::custom(format!(
                "borrow source \"{text}\" names the store \"{store}\", which is not a credential \
                 module name"
            )));
        }
        if key.is_empty() {
            return Err(D::Error::custom(format!(
                "borrow source \"{text}\" names no key"
            )));
        }
        Ok(Self {
            store: store.to_string(),
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
    /// a method with a login adds the CLI's own login after p1's store.
    #[serde(default)]
    pub store_only: bool,
    /// The directory whose login this route borrows (ADR-0074), as the file writes it: an
    /// absolute path or a directory below the home (`~/<dir>`, never `~` itself), expanded
    /// against the home directory when it is used. Only a route whose method's login allows
    /// it may name one (`claude-code-oauth`). Absent keeps the login's default directory. It is
    /// also the directory `p1 login <route> --import` imports from when no directory is given.
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
        if let Some(dir) = &self.login_dir {
            check_login_dir(dir)?;
        }
        if self.store_only && !self.borrow.is_empty() {
            return Err(
                "`[credential]` sets `store_only` and also lists `borrow` entries; a \
                 store-only route reads only its environment variable and p1's store"
                    .into(),
            );
        }
        // A core method checks its table now; a module's method once it is resolved
        // ([`CredentialSpec::validate_with`]), since what it accepts is the module's.
        match self.kind.core() {
            Some(method) => self.validate_with(method.as_ref()),
            None => Ok(()),
        }
    }

    /// The checks of `method`, the one this table's `kind` resolved to.
    pub fn validate_with(&self, method: &dyn CredentialMethod) -> Result<(), String> {
        if self.login_dir.is_some() && !method.takes_login_dir() {
            return Err(format!(
                "`[credential]` kind \"{}\" also names a `login_dir`; a {} route borrows no \
                 login directory — delete the `login_dir` line",
                self.kind.name(),
                self.kind.name()
            ));
        }
        method.validate(self)
    }
}

/// A `login_dir` is spelled plainly (ADR-0074): an absolute directory or one below the home,
/// never the home or the root itself, with no `.` or `..` component.
pub(crate) fn check_login_dir(dir: &str) -> Result<(), String> {
    // The home directory itself (`~`, `~/`) is not a login directory.
    let Some(rest) = dir.strip_prefix("~/").or_else(|| dir.strip_prefix('/')) else {
        return Err(format!(
            "`[credential]` login_dir \"{dir}\" is neither an absolute path nor a directory \
             below the home (`~/<dir>`)"
        ));
    };
    let components: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
    if components.iter().any(|part| *part == "." || *part == "..") {
        return Err(format!(
            "`[credential]` login_dir \"{dir}\" contains a `.` or `..` component; write the \
             directory plainly"
        ));
    }
    if components.is_empty() {
        return Err(format!(
            "`[credential]` login_dir \"{dir}\" names the home or the root directory; write an \
             absolute login directory or one below the home"
        ));
    }
    Ok(())
}

fn is_env_var_name(name: &str) -> bool {
    !name.is_empty()
        && name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
        && !name.as_bytes()[0].is_ascii_digit()
}
