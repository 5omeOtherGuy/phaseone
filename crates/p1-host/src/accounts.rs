//! Account files (`accounts/<id>.toml`, ADR-0139): who is billed and how p1
//! authenticates, kept apart from the route that says how p1 talks to a provider. An
//! account holds a credential REFERENCE (`p1_auth::CredentialSpec`, never a value) and
//! the endpoint origins it means to send that credential to. Declaring an origin
//! approves nothing: ADR-0110's compiled and stored approvals still decide
//! (`crate::routes::check_credential_origin`).
//!
//! A route with an inline `[credential]` is a route plus an IMPLICIT account whose id
//! and store identity are the route's (ADR-0139 §6), so every existing route file,
//! store entry and variable keeps working. The lookup directory is the one routes use:
//! `<environments dir>/../accounts`.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use p1_auth::CredentialSpec;
use serde::Deserialize;

use crate::routes::endpoint_origin;

/// One account, from its own file or implicit in a route's inline `[credential]`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    /// The id an environment's `account` and a route's default `account` name.
    pub id: String,
    /// Endpoint origins (`scheme://authority`) this account's credential may be sent
    /// to, as the file declares them. A route whose origin is not listed is refused.
    pub origins: Vec<String>,
    pub credential: CredentialSpec,
    /// p1's store key for this account's entry and its origin approval (ADR-0040).
    pub store_id: String,
    /// `Some(route id)` for the implicit account of a route's inline `[credential]`.
    pub implicit_of: Option<String>,
    /// The file the account came from: its own file, or the route file.
    pub source: PathBuf,
    /// The name `p1 login --list` and `p1 usage` show; `None` derives one from the id.
    pub label: Option<String>,
    /// The compiled usage probe (`p1_usage`), by name; `None` keeps the shipped
    /// route-id table, which only an implicit account of a shipped route matches.
    pub usage: Option<String>,
    /// `[legacy_routes]` (ADR-0139 §6): an old route id → the route it now means with
    /// this account.
    pub legacy_routes: BTreeMap<String, String>,
    /// `[legacy_origins]` (ADR-0139 §7): a route id → the origin string sessions
    /// recorded for this account on it before accounts existed.
    pub legacy_origins: BTreeMap<String, String>,
}

impl Account {
    /// Whether this account declares the origin of `endpoint`.
    pub fn covers(&self, endpoint: &str) -> bool {
        self.origins.contains(&endpoint_origin(endpoint))
    }
}

/// The TOML surface of `accounts/<id>.toml`.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct AccountToml {
    id: String,
    #[serde(default)]
    label: Option<String>,
    origins: Vec<String>,
    #[serde(default)]
    usage: Option<String>,
    #[serde(default)]
    store_id: Option<String>,
    credential: CredentialSpec,
    #[serde(default)]
    legacy_routes: BTreeMap<String, String>,
    #[serde(default)]
    legacy_origins: BTreeMap<String, String>,
}

pub use p1_assembly::is_account_id;

/// `<dir>/../accounts`, for each environments directory, highest priority first.
pub fn accounts_dirs(environment_dirs: &[PathBuf]) -> Vec<PathBuf> {
    environment_dirs
        .iter()
        .map(|dir| dir.join("../accounts"))
        .collect()
}

/// Every `*.toml` in `dir`, sorted by file name, parsed and validated. A directory
/// that does not exist holds no accounts.
pub fn load_accounts(dir: &Path) -> Result<Vec<Account>, String> {
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => {
            return Err(format!(
                "cannot read the account directory {}: {error}",
                dir.display()
            ));
        }
    };
    let mut paths: Vec<PathBuf> = entries
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .collect();
    paths.sort();
    paths.iter().map(|path| load_account(path)).collect()
}

/// Parse and validate one account file. Every error names the file.
pub fn load_account(path: &Path) -> Result<Account, String> {
    let name = |message: String| format!("{}: {message}", path.display());
    let stem = path
        .file_stem()
        .and_then(OsStr::to_str)
        .ok_or_else(|| name("the account file name is not UTF-8".into()))?;
    let text = std::fs::read_to_string(path).map_err(|error| name(error.to_string()))?;
    let parsed: AccountToml = toml::from_str(&text).map_err(|error| name(error.to_string()))?;
    validate(&parsed, stem).map_err(name)?;
    Ok(Account {
        store_id: parsed.store_id.unwrap_or_else(|| parsed.id.clone()),
        id: parsed.id,
        origins: parsed.origins,
        credential: parsed.credential,
        implicit_of: None,
        source: path.to_path_buf(),
        label: parsed.label,
        usage: parsed.usage,
        legacy_routes: parsed.legacy_routes,
        legacy_origins: parsed.legacy_origins,
    })
}

fn validate(account: &AccountToml, stem: &str) -> Result<(), String> {
    if account.id != stem {
        return Err(format!(
            "account id \"{}\" must equal the file stem \"{stem}\"",
            account.id
        ));
    }
    if !is_account_id(&account.id) {
        return Err(format!(
            "account id \"{}\" may use only letters, digits, `-`, `_` and `.`",
            account.id
        ));
    }
    if account.origins.is_empty() {
        return Err(
            "`origins` is empty; list the endpoint origins (`https://host`) this account's \
             credential may be sent to"
                .into(),
        );
    }
    for (index, origin) in account.origins.iter().enumerate() {
        if !origin.contains("://") || endpoint_origin(origin) != *origin {
            return Err(format!(
                "`origins` entry \"{origin}\" is not a lowercase `scheme://authority` origin \
                 without a path"
            ));
        }
        if account.origins[..index].contains(origin) {
            return Err(format!("`origins` lists \"{origin}\" twice"));
        }
    }
    if let Some(store_id) = &account.store_id
        && !is_account_id(store_id)
    {
        return Err(format!(
            "`store_id` \"{store_id}\" may use only letters, digits, `-`, `_` and `.`"
        ));
    }
    // Route ids use the account id rule: no `@`, which separates the two in a key.
    for (old, route) in &account.legacy_routes {
        if !is_account_id(old) || !is_account_id(route) || old == route {
            return Err(format!(
                "`[legacy_routes]` entry \"{old}\" = \"{route}\" must map an old route id to \
                 another route id"
            ));
        }
    }
    for (route, origin) in &account.legacy_origins {
        if !is_account_id(route) || origin.trim().is_empty() {
            return Err(format!(
                "`[legacy_origins]` entry \"{route}\" = \"{origin}\" must map a route id to \
                 the origin string its sessions recorded"
            ));
        }
    }
    if let Some(usage) = &account.usage
        && !p1_usage::is_probe(usage)
    {
        return Err(format!(
            "`usage` \"{usage}\" is not a usage probe p1 knows (claude, codex, opencode-go, \
             kimi, glm)"
        ));
    }
    account.credential.validate()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(dir: &Path, stem: &str, text: &str) -> PathBuf {
        std::fs::create_dir_all(dir).unwrap();
        let path = dir.join(format!("{stem}.toml"));
        std::fs::write(&path, text).unwrap();
        path
    }

    #[test]
    fn an_account_file_loads_with_method_spelling_and_its_id_as_store_identity() {
        let dir = tempfile::tempdir().unwrap();
        let path = write(
            dir.path(),
            "work",
            "id = \"work\"\norigins = [\"https://api.example.test\"]\n\
             [credential]\nmethod = \"api-key\"\nenv = \"WORK_KEY\"\nstore_only = true\n",
        );
        let account = load_account(&path).unwrap();
        assert_eq!(account.id, "work");
        assert_eq!(account.store_id, "work");
        assert_eq!(account.credential.kind, p1_auth::CredentialKind::ApiKey);
        assert!(account.covers("https://api.example.test/v1"));
        assert!(!account.covers("https://other.example.test/v1"));
        assert_eq!(account.implicit_of, None);
    }

    #[test]
    fn malformed_account_files_are_load_errors_naming_the_file() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (
                "stem",
                "id = \"other\"\norigins = [\"https://a.test\"]\n[credential]\nmethod = \"none\"\n",
                "file stem",
            ),
            (
                "bad@id",
                "id = \"bad@id\"\norigins = [\"https://a.test\"]\n[credential]\nmethod = \"none\"\n",
                "may use only",
            ),
            (
                "empty",
                "id = \"empty\"\norigins = []\n[credential]\nmethod = \"none\"\n",
                "`origins` is empty",
            ),
            (
                "path",
                "id = \"path\"\norigins = [\"https://a.test/v1\"]\n[credential]\nmethod = \"none\"\n",
                "not a lowercase",
            ),
            (
                "upper",
                "id = \"upper\"\norigins = [\"https://A.test\"]\n[credential]\nmethod = \"none\"\n",
                "not a lowercase",
            ),
            (
                "nokey",
                "id = \"nokey\"\norigins = [\"https://a.test\"]\n[credential]\nmethod = \"api-key\"\n",
                "names no environment variable",
            ),
            (
                "extra",
                "id = \"extra\"\norigins = [\"https://a.test\"]\nsecret = \"x\"\n[credential]\nmethod = \"none\"\n",
                "unknown field",
            ),
            (
                "twice",
                "id = \"twice\"\norigins = [\"https://a.test\", \"https://a.test\"]\n[credential]\nmethod = \"none\"\n",
                "twice",
            ),
            (
                "probe",
                "id = \"probe\"\norigins = [\"https://a.test\"]\nusage = \"glmm\"\n[credential]\nmethod = \"none\"\n",
                "not a usage probe",
            ),
        ];
        for (stem, text, expected) in cases {
            let path = write(dir.path(), stem, text);
            let error = load_account(&path).unwrap_err();
            assert!(error.contains(expected), "{stem}: {error}");
            assert!(
                error.contains(&path.display().to_string()),
                "{stem}: {error}"
            );
        }
    }
}
