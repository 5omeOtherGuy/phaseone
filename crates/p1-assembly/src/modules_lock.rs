//! `modules.lock`: which official package a module name resolves to.
//!
//! An environment names a module by the key of a `[[tools]]` entry (`module = "…"`); the
//! host resolves a key that no compiled-in tool claims through `modules.lock` to one
//! package of p1's release: its manifest name, the release version, the digest of its
//! component (the module's identity) and its ABI (WIT world and protocol major.minor).
//! This file is plain data and resolution only: comparing the lock with p1's release
//! manifest is the host's (`p1-host`, `catalog/modules.rs`), verifying the bytes against
//! the digest, checking the ABI and compiling are the module runtime's loader, and nothing
//! here names wasmtime.
//!
//! # Format (`p1-modules-lock/1`)
//!
//! ```toml
//! format = "p1-modules-lock/1"
//!
//! [modules.fixture]
//! package  = "p1/fixture"
//! version  = "0.0.1"
//! digest   = "sha256:<64 lowercase hex>"
//! world    = "p1:module/tool@1.0.0"
//! protocol = "1.0"
//! ```
//!
//! Unknown keys are refused. A module name is the catalog key an environment uses, so it
//! has the shape of one: lowercase ASCII letters, digits, `_` and `-`.
//!
//! # Override order
//!
//! A lock file lives next to each environments directory (`<dir>/../modules.lock`, as
//! profiles live in `<dir>/../profiles`), and the lock files are layered in the
//! environment search order: an entry of an earlier (higher-priority) directory's lock
//! replaces the entry of the same name in a later one. An override can only *select*: the
//! loader accepts nothing but packages named by p1's release manifest, whatever the lock
//! says.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

/// The format tag every `modules.lock` carries.
pub const MODULES_LOCK_FORMAT: &str = "p1-modules-lock/1";
/// Lock file location relative to an environments directory.
const MODULES_LOCK_PATH: &str = "../modules.lock";
/// The reserved namespace of official packages (docs/design/modules/package.md).
const OFFICIAL_NAMESPACE: &str = "p1";

/// One resolution: module name → package, version, digest and ABI.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LockedModule {
    /// The package manifest's `name`, `<namespace>/<name>`.
    pub package: String,
    /// The release version the package was built at. Recorded, not compared: the package
    /// manifest carries no version, so the digest is what pins the bytes.
    pub version: String,
    /// `sha256:<64 lowercase hex>`, the digest of the package's `.wasm`.
    pub digest: String,
    /// The WIT world the package implements, `p1:module/<kind>@<version>`.
    pub world: String,
    /// The value protocol the package speaks.
    pub protocol: LockedProtocol,
    /// The lock file this resolution came from, for error messages.
    pub source: PathBuf,
}

/// A protocol `major.minor` as a lock or package manifest writes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct LockedProtocol {
    pub major: u32,
    pub minor: u32,
}

impl LockedProtocol {
    /// Parse `major.minor` (two unsigned integers, nothing else).
    pub fn parse(text: &str) -> Option<Self> {
        let (major, minor) = text.split_once('.')?;
        let digits = |part: &str| !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit());
        if !digits(major) || !digits(minor) {
            return None;
        }
        Some(Self {
            major: major.parse().ok()?,
            minor: minor.parse().ok()?,
        })
    }
}

impl std::fmt::Display for LockedProtocol {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}.{}", self.major, self.minor)
    }
}

/// The effective resolutions, after layering.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ModulesLock {
    modules: BTreeMap<String, LockedModule>,
}

/// Why a `modules.lock` could not be read. One variant per failure.
#[derive(Debug, thiserror::Error)]
pub enum ModulesLockError {
    #[error("cannot read modules lock {}: {message}", path.display())]
    Read { path: PathBuf, message: String },
    #[error("invalid modules lock {}: {message}", path.display())]
    Parse { path: PathBuf, message: String },
    #[error("modules lock {} has format `{found}`; this host reads `{MODULES_LOCK_FORMAT}`", path.display())]
    UnsupportedFormat { path: PathBuf, found: String },
    #[error("modules lock {}: module `{module}`: {message}", path.display())]
    InvalidEntry {
        path: PathBuf,
        module: String,
        message: String,
    },
}

impl ModulesLock {
    /// Parse one lock file's text. `path` is recorded in every entry and every error.
    pub fn parse(path: &Path, text: &str) -> Result<Self, ModulesLockError> {
        let parsed: LockToml = toml::from_str(text).map_err(|error| ModulesLockError::Parse {
            path: path.to_path_buf(),
            message: error.to_string(),
        })?;
        if parsed.format != MODULES_LOCK_FORMAT {
            return Err(ModulesLockError::UnsupportedFormat {
                path: path.to_path_buf(),
                found: parsed.format,
            });
        }
        let mut modules = BTreeMap::new();
        for (module, entry) in parsed.modules {
            let invalid = |message: String| ModulesLockError::InvalidEntry {
                path: path.to_path_buf(),
                module: module.clone(),
                message,
            };
            if !valid_module_name(&module) {
                return Err(invalid(
                    "a module name is lowercase ASCII letters, digits, `_` and `-`".to_string(),
                ));
            }
            let Some((namespace, name)) = entry.package.split_once('/') else {
                return Err(invalid(format!(
                    "package `{}` is not `<namespace>/<name>`",
                    entry.package
                )));
            };
            if name.is_empty() || name.contains('/') {
                return Err(invalid(format!(
                    "package `{}` is not `<namespace>/<name>`",
                    entry.package
                )));
            }
            if namespace != OFFICIAL_NAMESPACE {
                return Err(invalid(format!(
                    "package `{}` is outside the reserved `{OFFICIAL_NAMESPACE}/` namespace; \
                     only official packages can be selected",
                    entry.package
                )));
            }
            if entry.version.trim().is_empty() {
                return Err(invalid("version is empty".to_string()));
            }
            if !valid_digest(&entry.digest) {
                return Err(invalid(format!(
                    "digest `{}` is not `sha256:<64 lowercase hex>`",
                    entry.digest
                )));
            }
            if !entry.world.starts_with("p1:module/") || !entry.world.contains('@') {
                return Err(invalid(format!(
                    "world `{}` is not `p1:module/<kind>@<version>`",
                    entry.world
                )));
            }
            let Some(protocol) = LockedProtocol::parse(&entry.protocol) else {
                return Err(invalid(format!(
                    "protocol `{}` is not `major.minor`",
                    entry.protocol
                )));
            };
            modules.insert(
                module,
                LockedModule {
                    package: entry.package,
                    version: entry.version,
                    digest: entry.digest,
                    world: entry.world,
                    protocol,
                    source: path.to_path_buf(),
                },
            );
        }
        Ok(Self { modules })
    }

    /// Layer `lower` under `self`: every name `self` resolves keeps its resolution, the
    /// others come from `lower`.
    pub fn over(mut self, lower: ModulesLock) -> Self {
        for (name, entry) in lower.modules {
            self.modules.entry(name).or_insert(entry);
        }
        self
    }

    /// The resolution of `module`, or `None` when no lock names it.
    pub fn resolve(&self, module: &str) -> Option<&LockedModule> {
        self.modules.get(module)
    }

    /// Every resolution, sorted by module name.
    pub fn iter(&self) -> impl Iterator<Item = (&str, &LockedModule)> {
        self.modules
            .iter()
            .map(|(name, entry)| (name.as_str(), entry))
    }

    pub fn is_empty(&self) -> bool {
        self.modules.is_empty()
    }
}

/// Read and layer the lock files next to `search_dirs`, highest priority first (the
/// environment search order, see [`crate::load_environment`]). A directory without a
/// lock file contributes nothing; no lock at all is the empty lock.
pub fn load_modules_lock(search_dirs: &[PathBuf]) -> Result<ModulesLock, ModulesLockError> {
    let mut effective = ModulesLock::default();
    let mut seen: Vec<PathBuf> = Vec::new();
    for base in search_dirs {
        let path = base.join(MODULES_LOCK_PATH);
        // Two environments directories may share a parent; read that lock once.
        let key = std::fs::canonicalize(&path).unwrap_or_else(|_| path.clone());
        if seen.contains(&key) {
            continue;
        }
        let text = match std::fs::read_to_string(&path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(ModulesLockError::Read {
                    path,
                    message: error.to_string(),
                });
            }
        };
        seen.push(key);
        effective = effective.over(ModulesLock::parse(&path, &text)?);
    }
    Ok(effective)
}

fn valid_module_name(name: &str) -> bool {
    !name.is_empty()
        && name
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
}

fn valid_digest(digest: &str) -> bool {
    digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockToml {
    format: String,
    #[serde(default)]
    modules: BTreeMap<String, LockEntryToml>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct LockEntryToml {
    package: String,
    version: String,
    digest: String,
    world: String,
    protocol: String,
}
