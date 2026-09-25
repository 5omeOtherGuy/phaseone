//! The release manifest: the one source modules are loaded from (freeze item 6, "official
//! source only").
//!
//! p1's release archive ships one `manifest.json` (`scripts/release-manifest.py`, format
//! `p1-release-manifest/1`); its `components` list is the set of module packages p1 built
//! and signed off on. Each entry names a package by its manifest name, pins its bytes by
//! digest and locates them by a path relative to the manifest's directory, and carries the
//! package's frozen manifest fields (`docs/design/modules/package.md`) so the loader can
//! refuse a package the runtime does not speak before it compiles anything. The rest of the
//! release manifest (native binary, toolchain, WIT and schema digests) is the installer's.

use std::fmt;
use std::path::{Path, PathBuf};

use p1_contracts::serde_json::{self, Map, Value};
use thiserror::Error;

use crate::sha256;

/// The `format` of the release manifest this runtime reads.
pub const RELEASE_MANIFEST_FORMAT: &str = "p1-release-manifest/1";

/// Why a release manifest could not be read.
#[derive(Debug, Error)]
pub enum ManifestError {
    /// The manifest file could not be read.
    #[error("cannot read the release manifest {path}: {source}")]
    Read {
        /// The file.
        path: PathBuf,
        /// Why.
        source: std::io::Error,
    },
    /// The manifest is not JSON.
    #[error("the release manifest is not JSON: {0}")]
    Json(String),
    /// The manifest is JSON but not of the shape this runtime reads; the reason names the
    /// field.
    #[error("the release manifest is invalid: {0}")]
    Invalid(String),
}

/// A module's identity: the SHA-256 of its component bytes.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Digest([u8; 32]);

impl Digest {
    /// The digest of `bytes`.
    pub fn of(bytes: &[u8]) -> Self {
        Self(sha256::sha256(bytes))
    }

    /// Parses the manifest form `sha256:<64 lowercase hex>`.
    pub fn parse(text: &str) -> Option<Self> {
        let hex = text.strip_prefix("sha256:")?;
        if hex.len() != 64 {
            return None;
        }
        let mut bytes = [0_u8; 32];
        for (index, pair) in hex.as_bytes().chunks(2).enumerate() {
            let high = lower_hex_value(pair[0])?;
            let low = lower_hex_value(pair[1])?;
            bytes[index] = (high << 4) | low;
        }
        Some(Self(bytes))
    }
}

/// One lowercase hex digit; the manifest form is canonical, so uppercase is refused.
fn lower_hex_value(byte: u8) -> Option<u8> {
    match byte {
        b'0'..=b'9' => Some(byte - b'0'),
        b'a'..=b'f' => Some(byte - b'a' + 10),
        _ => None,
    }
}

impl fmt::Display for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "sha256:{}", sha256::hex(&self.0))
    }
}

impl fmt::Debug for Digest {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(self, f)
    }
}

/// One module package of the release, as the manifest states it. The package fields are
/// kept as written; the loader decides whether the runtime speaks them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ComponentEntry {
    /// The package's manifest name, `<namespace>/<name>`.
    pub name: String,
    /// The digest of the component bytes: the module's identity.
    pub digest: Digest,
    /// Where the component is, relative to the manifest's directory (POSIX, no `..`).
    pub path: String,
    /// The module class.
    pub kind: String,
    /// The WIT world the package implements.
    pub world: String,
    /// The major.minor of the value protocol the package speaks.
    pub protocol: String,
    /// What the package may be linked with.
    pub capabilities: Vec<String>,
    /// The model-facing variant of its `ToolIdentity`.
    pub variant: String,
}

/// The module packages of one p1 release.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReleaseManifest {
    components: Vec<ComponentEntry>,
}

impl ReleaseManifest {
    /// Reads the manifest file at `path`.
    pub fn read(path: &Path) -> Result<Self, ManifestError> {
        let text = std::fs::read_to_string(path).map_err(|source| ManifestError::Read {
            path: path.to_owned(),
            source,
        })?;
        Self::parse(&text)
    }

    /// Parses manifest text. Entries are closed: an unknown field is refused, because a
    /// field this runtime does not know may be one it should have enforced.
    pub fn parse(text: &str) -> Result<Self, ManifestError> {
        let value: Value =
            serde_json::from_str(text).map_err(|error| ManifestError::Json(error.to_string()))?;
        let Value::Object(top) = value else {
            return Err(invalid("the top level is not an object"));
        };
        match top.get("format") {
            Some(Value::String(format)) if format == RELEASE_MANIFEST_FORMAT => {}
            Some(other) => {
                return Err(invalid(format!(
                    "format is {other}, this runtime reads {RELEASE_MANIFEST_FORMAT}"
                )));
            }
            None => return Err(invalid("format is missing")),
        }
        let Some(Value::Array(entries)) = top.get("components") else {
            return Err(invalid("components is missing or not a list"));
        };
        let mut components: Vec<ComponentEntry> = Vec::with_capacity(entries.len());
        for (index, entry) in entries.iter().enumerate() {
            let entry = component_entry(entry)
                .map_err(|reason| invalid(format!("components[{index}]: {reason}")))?;
            if components.iter().any(|known| known.name == entry.name) {
                return Err(invalid(format!(
                    "components[{index}]: {} is listed twice",
                    entry.name
                )));
            }
            components.push(entry);
        }
        Ok(Self { components })
    }

    /// The entry for manifest name `name`, if the release has one.
    pub fn entry(&self, name: &str) -> Option<&ComponentEntry> {
        self.components.iter().find(|entry| entry.name == name)
    }

    /// Every entry, in manifest order.
    pub fn components(&self) -> &[ComponentEntry] {
        &self.components
    }
}

fn invalid(reason: impl Into<String>) -> ManifestError {
    ManifestError::Invalid(reason.into())
}

const ENTRY_FIELDS: [&str; 8] = [
    "name",
    "digest",
    "path",
    "kind",
    "world",
    "protocol",
    "capabilities",
    "variant",
];

fn component_entry(value: &Value) -> Result<ComponentEntry, String> {
    let Value::Object(fields) = value else {
        return Err("not an object".to_owned());
    };
    if let Some(unknown) = fields
        .keys()
        .find(|key| !ENTRY_FIELDS.contains(&key.as_str()))
    {
        return Err(format!("unknown field {unknown}"));
    }
    let digest_text = string_field(fields, "digest")?;
    let digest = Digest::parse(&digest_text)
        .ok_or_else(|| format!("digest {digest_text:?} is not sha256:<64 lowercase hex>"))?;
    let path = string_field(fields, "path")?;
    check_relative_path(&path)?;
    let capabilities = match fields.get("capabilities") {
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| match item {
                Value::String(name) => Ok(name.clone()),
                other => Err(format!(
                    "capabilities holds {other}, a list of names is required"
                )),
            })
            .collect::<Result<Vec<_>, _>>()?,
        Some(other) => return Err(format!("capabilities is {other}, a list is required")),
        None => return Err("capabilities is missing".to_owned()),
    };
    Ok(ComponentEntry {
        name: string_field(fields, "name")?,
        digest,
        path,
        kind: string_field(fields, "kind")?,
        world: string_field(fields, "world")?,
        protocol: string_field(fields, "protocol")?,
        capabilities,
        variant: string_field(fields, "variant")?,
    })
}

fn string_field(fields: &Map<String, Value>, key: &str) -> Result<String, String> {
    match fields.get(key) {
        Some(Value::String(text)) => Ok(text.clone()),
        Some(other) => Err(format!("{key} is {other}, a string is required")),
        None => Err(format!("{key} is missing")),
    }
}

/// The manifest names paths below its own directory, never places: an absolute path, a
/// backslash or an empty, `.` or `..` component would reach outside the release (the same
/// rule as `scripts/release-manifest.py`'s `check_relpath`).
fn check_relative_path(path: &str) -> Result<(), String> {
    if path.is_empty() || path.starts_with('/') || path.contains('\\') {
        return Err(format!("path {path:?} is not a POSIX relative path"));
    }
    if path
        .split('/')
        .any(|part| part.is_empty() || part == "." || part == "..")
    {
        return Err(format!("path {path:?} has an empty, '.' or '..' component"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const DIGEST: &str = "sha256:e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

    fn manifest(entry: &str) -> String {
        format!(
            "{{\"format\":\"p1-release-manifest/1\",\"commit\":\"x\",\"components\":[{entry}]}}"
        )
    }

    fn entry_with(path: &str) -> String {
        format!(
            "{{\"name\":\"p1/fixture\",\"digest\":\"{DIGEST}\",\"path\":\"{path}\",\"kind\":\"tool\",\
             \"world\":\"p1:module/tool@1.0.0\",\"protocol\":\"1.0\",\"capabilities\":[\"clock\"],\
             \"variant\":\"default\"}}"
        )
    }

    #[test]
    fn reads_an_entry_and_its_digest() {
        let manifest = ReleaseManifest::parse(&manifest(&entry_with("packages/f/f.wasm"))).unwrap();
        let entry = manifest.entry("p1/fixture").expect("entry");
        assert_eq!(entry.digest, Digest::of(b""));
        assert_eq!(entry.digest.to_string(), DIGEST);
        assert_eq!(entry.capabilities, vec!["clock".to_owned()]);
        assert!(manifest.entry("p1/other").is_none());
    }

    #[test]
    fn refuses_paths_outside_the_release() {
        for path in [
            "/abs/f.wasm",
            "../f.wasm",
            "a/./f.wasm",
            "a//f.wasm",
            "a\\\\f.wasm",
        ] {
            let error = ReleaseManifest::parse(&manifest(&entry_with(path))).unwrap_err();
            assert!(
                matches!(error, ManifestError::Invalid(_)),
                "{path}: {error}"
            );
        }
    }

    #[test]
    fn refuses_unknown_fields_bad_digests_and_other_formats() {
        let unknown = entry_with("f.wasm").replacen('{', "{\"extra\":1,", 1);
        assert!(ReleaseManifest::parse(&manifest(&unknown)).is_err());
        let upper = entry_with("f.wasm").replace("e3b0", "E3B0");
        assert!(ReleaseManifest::parse(&manifest(&upper)).is_err());
        let other = manifest(&entry_with("f.wasm")).replace("manifest/1", "manifest/2");
        assert!(ReleaseManifest::parse(&other).is_err());
        let twice = manifest(&format!(
            "{},{}",
            entry_with("a.wasm"),
            entry_with("b.wasm")
        ));
        assert!(ReleaseManifest::parse(&twice).is_err());
    }
}
