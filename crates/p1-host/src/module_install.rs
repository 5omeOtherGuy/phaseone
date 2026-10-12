//! `p1 modules install PACKAGE` (ADR-0209): the one explicit path by which a module package
//! from outside p1's release becomes installed.
//!
//! A package archive (`<package>.p1pkg`, `docs/design/modules/package.md`) is an uncompressed
//! ustar archive of exactly two regular files: `manifest.json`, the package manifest in format
//! `p1-module-package/1`, and `component.wasm`, the component. Installing it
//!
//! 1. reads the archive (a regular file, bounded) and refuses any other member;
//! 2. refuses a package in the reserved `p1/` namespace, which only p1's release supplies;
//! 3. verifies the manifest's digest and size against the component bytes;
//! 4. loads the package through the runtime loader from the user's set exactly as a session
//!    would, so a component the loader refuses (its class, world, protocol, grants, or an
//!    import beyond its class's allocation) is never recorded;
//! 5. records it in the user's set (`<catalog dir>/modules/manifest.json`, the component
//!    content-addressed under `packages/`) and pins it in the user's lock
//!    (`<catalog dir>/modules.lock`) under the module key the manifest's name declares: the
//!    name without its namespace.
//!
//! A credential method (ADR-0212) declares the https origins its token refreshes go to. They
//! are installed only when the owner confirmed them (`--confirm-origins`): on its first
//! install and whenever they differ from the ones the lock pins. The lock pins them beside the
//! digest, and a session refuses a package whose manifest declares others.
//!
//! Nothing here fetches anything: the user obtains the file. Nothing a model, an environment
//! or a route says can install a package.

use std::io::Read;
use std::path::{Path, PathBuf};

use p1_assembly::{LockedModule, LockedProtocol, ModulesLock};
use p1_contracts::serde_json::{self, Value, json};
use p1_module_runtime::loader::{OFFICIAL_NAMESPACE, check_component_header, is_official};
use p1_module_runtime::manifest::{ComponentEntry, Digest, RELEASE_MANIFEST_FORMAT};
use p1_module_runtime::{Loader, ReleaseManifest};
use serde::Deserialize;

/// The format of a package archive's `manifest.json`.
pub const PACKAGE_FORMAT: &str = "p1-module-package/1";
/// The archive member holding the package manifest.
const MANIFEST_MEMBER: &str = "manifest.json";
/// The archive member holding the component.
const COMPONENT_MEMBER: &str = "component.wasm";
/// The largest archive read: far above any module p1 builds, small enough to read at once.
const MAX_ARCHIVE_BYTES: u64 = 256 * 1024 * 1024;
/// The largest package manifest accepted.
const MAX_MANIFEST_BYTES: usize = 64 * 1024;
/// A ustar block.
const BLOCK: usize = 512;

/// The package manifest inside an archive: the build's `<package>.manifest.json` fields
/// (`scripts/build-modules.sh`) plus the format and the version the package was published at.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct PackageManifest {
    format: String,
    name: String,
    version: String,
    kind: String,
    world: String,
    protocol: String,
    capabilities: Vec<String>,
    variant: String,
    digest: String,
    size: u64,
    /// The https origins a credential method's refresh may go to (ADR-0212).
    #[serde(default)]
    token_origins: Option<Vec<String>>,
}

/// What an install recorded.
#[derive(Debug)]
pub struct Installed {
    /// The package's manifest name, `<namespace>/<name>`.
    pub name: String,
    /// The module key the user's lock pins it under.
    pub key: String,
    /// The digest pinned.
    pub digest: Digest,
    /// The user's set it was copied into.
    pub set: PathBuf,
    /// The user's lock that pins it.
    pub lock: PathBuf,
    /// The package and digest the key resolved to before, when the lock had another.
    pub replaced: Option<(String, String)>,
    /// A credential method's token origins, as pinned.
    pub token_origins: Option<Vec<String>>,
}

/// Installs the package archive `archive` into the user's catalog directory `catalog`. A
/// credential method's token origins must be `confirmed` when the lock does not pin them yet.
pub fn install(archive: &Path, catalog: &Path, confirmed: bool) -> Result<Installed, String> {
    let bytes = read_archive(archive)?;
    let named = |message: String| format!("{}: {message}", archive.display());
    let members = ustar_members(&bytes).map_err(named)?;
    let member = |wanted: &str| {
        members
            .iter()
            .find(|(name, _)| name == wanted)
            .map(|(_, data)| *data)
            .ok_or_else(|| named(format!("the archive has no {wanted}")))
    };
    let manifest_bytes = member(MANIFEST_MEMBER)?;
    let component = member(COMPONENT_MEMBER)?;
    if manifest_bytes.len() > MAX_MANIFEST_BYTES {
        return Err(named(format!("{MANIFEST_MEMBER} is too large")));
    }
    let package: PackageManifest = serde_json::from_slice(manifest_bytes)
        .map_err(|error| named(format!("{MANIFEST_MEMBER} is invalid: {error}")))?;
    if package.format != PACKAGE_FORMAT {
        return Err(named(format!(
            "{MANIFEST_MEMBER} has format `{}`; this p1 installs `{PACKAGE_FORMAT}`",
            package.format
        )));
    }
    let key = module_key(&package.name).map_err(named)?;
    if package.version.trim().is_empty() {
        return Err(named("the package version is empty".to_owned()));
    }
    let Some(protocol) = LockedProtocol::parse(&package.protocol) else {
        return Err(named(format!(
            "protocol `{}` is not `major.minor`",
            package.protocol
        )));
    };
    let pinned = Digest::parse(&package.digest).ok_or_else(|| {
        named(format!(
            "digest `{}` is not `sha256:<64 lowercase hex>`",
            package.digest
        ))
    })?;
    let digest = Digest::of(component);
    if digest != pinned {
        return Err(named(format!(
            "the manifest pins {pinned}, the component is {digest}; nothing installed"
        )));
    }
    if package.size != component.len() as u64 {
        return Err(named(format!(
            "the manifest says {} bytes, the component has {}; nothing installed",
            package.size,
            component.len()
        )));
    }
    check_component_header(component).map_err(named)?;

    let lock_path = catalog.join("modules.lock");
    let lock = match std::fs::read_to_string(&lock_path) {
        Ok(text) => ModulesLock::parse(&lock_path, &text).map_err(|error| error.to_string())?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => ModulesLock::default(),
        Err(error) => return Err(format!("{}: {error}", lock_path.display())),
    };
    let credential = package.kind == "credential-method";
    let token_origins = package.token_origins.clone().unwrap_or_default();
    if credential && package.token_origins.is_none() {
        return Err(named(
            "a credential method names its token origins (`token_origins`); nothing installed"
                .to_owned(),
        ));
    }
    let pinned_origins = lock
        .resolve(&key)
        .filter(|old| old.world.starts_with("p1:module/credential-method@"))
        .map(|old| old.token_origins.clone());
    if credential && pinned_origins.as_ref() != Some(&token_origins) && !confirmed {
        let was = match &pinned_origins {
            Some(origins) => format!(
                "; the lock pins {} today",
                crate::modules_cli::origins_text(origins)
            ),
            None => String::new(),
        };
        return Err(named(format!(
            "{} is a credential method: it reads its own login file and sends its token \
             refreshes to {}{was}. Run `p1 modules install {} --confirm-origins` to install it; \
             nothing installed",
            package.name,
            crate::modules_cli::origins_text(&token_origins),
            archive.display()
        )));
    }

    let set = catalog.join("modules");
    create_owned_dir(catalog)?;
    create_owned_dir(&set)?;
    create_owned_dir(&set.join("packages"))?;
    p1_auth::check_owned_directory(&set).map_err(|reason| format!("refused: {reason}"))?;
    let manifest_path = set.join("manifest.json");
    let mut components = read_components(&manifest_path)?;
    if let Some(entry) = components.iter().find(|entry| is_official(&entry.name)) {
        return Err(format!(
            "{}: lists {}, but the {OFFICIAL_NAMESPACE}/ namespace is reserved for p1's release; \
             nothing installed",
            manifest_path.display(),
            entry.name
        ));
    }

    // Content-addressed, so a file of the same name always holds the same bytes.
    let relative = format!("packages/{}.wasm", hex_of(&digest));
    let component_path = set.join(&relative);
    let existed = component_path.is_file();
    write_atomically(&component_path, component)?;
    let entry = ComponentEntry {
        name: package.name.clone(),
        digest,
        path: relative,
        kind: package.kind,
        world: package.world,
        protocol: package.protocol,
        capabilities: package.capabilities,
        variant: package.variant,
        precompiled: None,
        token_origins: package.token_origins.clone().unwrap_or_default(),
    };
    // The loader is the one verifier: install records nothing a session would refuse.
    let staged = ReleaseManifest::parse(&manifest_text(std::slice::from_ref(&entry)))
        .map_err(|error| error.to_string())?;
    let verified = Loader::new(staged, &set)
        .map_err(|error| error.to_string())
        .and_then(|loader| loader.load(&entry.name).map_err(|error| error.to_string()));
    if let Err(reason) = verified {
        if !existed && !components.iter().any(|known| known.digest == digest) {
            let _ = std::fs::remove_file(&component_path);
        }
        return Err(format!(
            "{}: {reason}; nothing installed",
            archive.display()
        ));
    }

    let previous_path = components
        .iter()
        .position(|known| known.name == entry.name)
        .map(|index| components.remove(index).path);
    components.push(entry.clone());
    components.sort_by(|a, b| a.name.cmp(&b.name));
    write_atomically(&manifest_path, manifest_text(&components).as_bytes())?;
    if let Some(previous) = previous_path
        && !components.iter().any(|known| known.path == previous)
    {
        let _ = std::fs::remove_file(set.join(previous));
    }

    let mut lock = lock;
    let replaced = lock
        .resolve(&key)
        .filter(|old| old.package != entry.name || old.digest != entry.digest.to_string())
        .map(|old| (old.package.clone(), old.digest.clone()));
    lock.insert(
        &key,
        LockedModule {
            package: entry.name.clone(),
            version: package.version,
            digest: entry.digest.to_string(),
            world: entry.world.clone(),
            protocol,
            token_origins: entry.token_origins.clone(),
            source: lock_path.clone(),
        },
    );
    write_atomically(&lock_path, lock.to_toml().as_bytes())?;
    Ok(Installed {
        name: entry.name,
        key,
        digest,
        set,
        lock: lock_path,
        replaced,
        token_origins: credential.then_some(token_origins),
    })
}

/// The module key a package's name declares: the name without its namespace. Both parts are
/// lowercase ASCII letters, digits, `_` and `-`, so the key is a valid lock and environment
/// key and the name is the same in every file that spells it.
fn module_key(name: &str) -> Result<String, String> {
    let part = |text: &str| {
        !text.is_empty()
            && text
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_' || b == b'-')
    };
    match name.split_once('/') {
        Some((namespace, key)) if part(namespace) && part(key) => {
            if namespace == OFFICIAL_NAMESPACE {
                Err(format!(
                    "{name} is in the {OFFICIAL_NAMESPACE}/ namespace, which is reserved for \
                     p1's release; nothing installed"
                ))
            } else {
                Ok(key.to_owned())
            }
        }
        _ => Err(format!(
            "package name `{name}` is not `<namespace>/<name>` of lowercase ASCII letters, \
             digits, `_` and `-`"
        )),
    }
}

/// The archive's bytes, from a regular file of bounded size.
fn read_archive(path: &Path) -> Result<Vec<u8>, String> {
    let named = |error: std::io::Error| format!("cannot read {}: {error}", path.display());
    let metadata = std::fs::symlink_metadata(path).map_err(named)?;
    if !metadata.file_type().is_file() {
        return Err(format!("{} is not a regular file", path.display()));
    }
    if metadata.len() > MAX_ARCHIVE_BYTES {
        return Err(format!(
            "{} is larger than {MAX_ARCHIVE_BYTES} bytes",
            path.display()
        ));
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .and_then(|file| file.take(MAX_ARCHIVE_BYTES + 1).read_to_end(&mut bytes))
        .map_err(named)?;
    Ok(bytes)
}

/// The regular-file members of a ustar archive, by name. Anything else (a directory, a link,
/// an extended header, a name with a directory part, a member named twice, a bad checksum) is
/// refused rather than skipped: a package archive holds two plain files.
fn ustar_members(bytes: &[u8]) -> Result<Vec<(String, &[u8])>, String> {
    let mut members: Vec<(String, &[u8])> = Vec::new();
    let mut at = 0;
    while at + BLOCK <= bytes.len() {
        let header = &bytes[at..at + BLOCK];
        if header.iter().all(|byte| *byte == 0) {
            return Ok(members);
        }
        if &header[257..262] != b"ustar" {
            return Err("not a ustar archive".to_owned());
        }
        let recorded = octal(&header[148..156]).ok_or("a header has no checksum")?;
        let sum: u64 = header
            .iter()
            .enumerate()
            .map(|(index, byte)| {
                if (148..156).contains(&index) {
                    u64::from(b' ')
                } else {
                    u64::from(*byte)
                }
            })
            .sum();
        if sum != recorded {
            return Err("a header checksum does not match".to_owned());
        }
        let name = text_field(&header[0..100])?;
        let prefix = text_field(&header[345..500])?;
        if !prefix.is_empty() || name.contains('/') || name.is_empty() {
            return Err(format!("member `{prefix}{name}` is not a plain file name"));
        }
        if !matches!(header[156], b'0' | 0) {
            return Err(format!("member `{name}` is not a regular file"));
        }
        let size = usize::try_from(octal(&header[124..136]).ok_or("a header has no size")?)
            .map_err(|_| "a member is too large".to_owned())?;
        let start = at + BLOCK;
        let end = start
            .checked_add(size)
            .filter(|end| *end <= bytes.len())
            .ok_or_else(|| format!("member `{name}` runs past the end of the archive"))?;
        if members.iter().any(|(known, _)| *known == name) {
            return Err(format!("member `{name}` appears twice"));
        }
        if name != MANIFEST_MEMBER && name != COMPONENT_MEMBER {
            return Err(format!(
                "member `{name}` is not part of a package; an archive holds {MANIFEST_MEMBER} \
                 and {COMPONENT_MEMBER}"
            ));
        }
        members.push((name, &bytes[start..end]));
        at = start + size.div_ceil(BLOCK) * BLOCK;
    }
    Err("the archive has no end-of-archive marker".to_owned())
}

/// A NUL- or space-terminated octal header field.
fn octal(field: &[u8]) -> Option<u64> {
    let digits: Vec<u8> = field
        .iter()
        .copied()
        .take_while(|byte| *byte != 0)
        .filter(|byte| *byte != b' ')
        .collect();
    if digits.is_empty() || !digits.iter().all(|byte| (b'0'..=b'7').contains(byte)) {
        return None;
    }
    u64::from_str_radix(std::str::from_utf8(&digits).ok()?, 8).ok()
}

/// A NUL-terminated text header field.
fn text_field(field: &[u8]) -> Result<String, String> {
    let end = field
        .iter()
        .position(|byte| *byte == 0)
        .unwrap_or(field.len());
    String::from_utf8(field[..end].to_vec()).map_err(|_| "a member name is not UTF-8".to_owned())
}

/// The components a set's manifest lists, or none when it has no manifest yet.
fn read_components(path: &Path) -> Result<Vec<ComponentEntry>, String> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    ReleaseManifest::read(path)
        .map(|manifest| manifest.components().to_vec())
        .map_err(|error| format!("{}: {error}", path.display()))
}

/// A set manifest listing `components`, in the release-manifest format the loader reads.
fn manifest_text(components: &[ComponentEntry]) -> String {
    let components: Vec<Value> = components
        .iter()
        .map(|entry| {
            let mut value = json!({
                "name": entry.name,
                "digest": entry.digest.to_string(),
                "path": entry.path,
                "kind": entry.kind,
                "world": entry.world,
                "protocol": entry.protocol,
                "capabilities": entry.capabilities,
                "variant": entry.variant,
            });
            // A credential method's set entry carries the origins its lock entry pins.
            if entry.kind == "credential-method" {
                value["token_origins"] = json!(entry.token_origins);
            }
            value
        })
        .collect();
    let mut text = serde_json::to_string_pretty(&json!({
        "format": RELEASE_MANIFEST_FORMAT,
        "components": components,
    }))
    .expect("a manifest serializes");
    text.push('\n');
    text
}

/// The hex digits of a digest, its `sha256:` prefix removed.
fn hex_of(digest: &Digest) -> String {
    digest.to_string().trim_start_matches("sha256:").to_owned()
}

/// Creates `path` and its missing parents writable by their owner only; an existing directory
/// is left as it is and checked by the caller.
fn create_owned_dir(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::DirBuilderExt;
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o755)
        .create(path)
        .map_err(|error| format!("cannot create {}: {error}", path.display()))
}

/// Writes `bytes` to a fresh file beside `path`, then renames it over `path`, so a reader
/// sees the old file or the new one, never a part.
fn write_atomically(path: &Path, bytes: &[u8]) -> Result<(), String> {
    use std::io::Write;
    let named = |error: std::io::Error| format!("cannot write {}: {error}", path.display());
    let file_name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let staging = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    let result = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staging)
        .and_then(|mut file| {
            file.write_all(bytes)?;
            file.sync_all()
        })
        .and_then(|()| std::fs::rename(&staging, path));
    if result.is_err() {
        let _ = std::fs::remove_file(&staging);
    }
    result.map_err(named)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A ustar archive of `members`, as `tar --format=ustar` writes one.
    fn archive(members: &[(&str, &[u8], u8)]) -> Vec<u8> {
        let mut bytes = Vec::new();
        for (name, data, kind) in members {
            let mut header = [0_u8; BLOCK];
            header[..name.len()].copy_from_slice(name.as_bytes());
            header[100..108].copy_from_slice(b"0000644\0");
            header[124..136].copy_from_slice(format!("{:011o}\0", data.len()).as_bytes());
            header[136..148].copy_from_slice(b"00000000000\0");
            header[156] = *kind;
            header[257..263].copy_from_slice(b"ustar\0");
            header[263..265].copy_from_slice(b"00");
            header[148..156].copy_from_slice(b"        ");
            let sum: u64 = header.iter().map(|byte| u64::from(*byte)).sum();
            header[148..156].copy_from_slice(format!("{sum:06o}\0 ").as_bytes());
            bytes.extend_from_slice(&header);
            bytes.extend_from_slice(data);
            bytes.resize(bytes.len().div_ceil(BLOCK) * BLOCK, 0);
        }
        bytes.resize(bytes.len() + 2 * BLOCK, 0);
        bytes
    }

    #[test]
    fn an_archive_yields_its_two_members() {
        let bytes = archive(&[
            (MANIFEST_MEMBER, b"{}", b'0'),
            (COMPONENT_MEMBER, b"wasm", b'0'),
        ]);
        let members = ustar_members(&bytes).expect("a package archive");
        assert_eq!(
            members,
            vec![
                (MANIFEST_MEMBER.to_owned(), &b"{}"[..]),
                (COMPONENT_MEMBER.to_owned(), &b"wasm"[..])
            ]
        );
    }

    #[test]
    fn anything_but_two_plain_named_files_is_refused() {
        for (members, why) in [
            (
                vec![(MANIFEST_MEMBER, &b"{}"[..], b'2')],
                "not a regular file",
            ),
            (
                vec![("dir/manifest.json", &b"{}"[..], b'0')],
                "not a plain file name",
            ),
            (
                vec![("extra.txt", &b"x"[..], b'0')],
                "not part of a package",
            ),
            (
                vec![
                    (MANIFEST_MEMBER, &b"{}"[..], b'0'),
                    (MANIFEST_MEMBER, &b"{}"[..], b'0'),
                ],
                "appears twice",
            ),
        ] {
            let error = ustar_members(&archive(&members)).unwrap_err();
            assert!(error.contains(why), "{why}: {error}");
        }
        let mut corrupt = archive(&[(MANIFEST_MEMBER, b"{}", b'0')]);
        corrupt[0] = b'X';
        assert!(ustar_members(&corrupt).unwrap_err().contains("checksum"));
        let truncated = archive(&[(COMPONENT_MEMBER, &[1; 600], b'0')]);
        assert!(
            ustar_members(&truncated[..BLOCK + 100])
                .unwrap_err()
                .contains("past the end")
        );
    }

    #[test]
    fn a_key_is_the_name_without_its_publisher_and_p1_is_reserved() {
        assert_eq!(module_key("acme/fixture").unwrap(), "fixture");
        assert!(module_key("p1/fixture").unwrap_err().contains("reserved"));
        for bad in [
            "fixture",
            "acme/",
            "/fixture",
            "Acme/fixture",
            "acme/a/b",
            "acme/a.b",
        ] {
            assert!(module_key(bad).is_err(), "{bad}");
        }
    }
}
