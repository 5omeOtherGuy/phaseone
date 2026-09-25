//! The verified package loader (ADR draft "Module identity and verified loading").
//!
//! A [`Release`] is p1's own installed module set: `<modules dir>/manifest.json` (the
//! release manifest of ADR-0079, format `p1-release-manifest/1`) and the package files
//! under `<modules dir>/packages/<package>/` (`<package>.wasm` and
//! `<package>.manifest.json`, the build outputs of docs/design/modules/package.md).
//!
//! **Official source.** A package is official when both of its files are named by the
//! release manifest's `packages` list, with the sha256 the manifest records, and its
//! manifest `name` is in the reserved `p1/` namespace. Anything else found on disk, and
//! any package a lock names that the release does not hold, is refused with an error that
//! names the source.
//!
//! **Verify, then compile the same bytes.** [`Release::load`] reads the component once
//! into memory, hashes that buffer, compares the digest with the release manifest, the
//! package manifest and the `modules.lock` resolution, and compiles exactly that buffer.
//! It never reads the file twice and never deserializes a compiled cache.
//!
//! **ABI.** The package's world must be one this host implements and its protocol major
//! must be `PROTOCOL_VERSION`'s, with a minor no newer than the host's
//! (docs/design/modules/protocol.md). The lock's world and protocol must be the package's.
//!
//! **Grants.** The manifest's capabilities must lie inside the class allocation of
//! docs/design/modules/wit.md, and every interface the compiled component imports must be
//! granted by the manifest (or grant nothing, like `types`).
//!
//! **Identity.** A module's identity is its digest; two packages that claim one name or
//! one digest make the release ambiguous and it is refused whole.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use p1_assembly::{LockedModule, LockedProtocol};
use p1_contracts::ToolIdentity;
use p1_module_protocol::PROTOCOL_VERSION;
use serde::Deserialize;
use wasmtime::Engine;
use wasmtime::component::Component;

/// The release manifest format this loader reads (ADR-0079).
pub const RELEASE_MANIFEST_FORMAT: &str = "p1-release-manifest/1";
/// The release manifest's file name inside the modules directory.
pub const RELEASE_MANIFEST_FILE: &str = "manifest.json";
/// The package tree inside the modules directory.
const PACKAGES_DIR: &str = "packages";
/// The reserved namespace of official packages.
const OFFICIAL_NAMESPACE: &str = "p1";
/// Suffix of a package manifest file.
const MANIFEST_SUFFIX: &str = ".manifest.json";

/// The worlds this host implements: `(kind, world)`.
// DRAFT(tag): the world list and its version come from the tag's `modules/wit/worlds.wit`
// (freeze item 1); a host constant exported by S0 (p1-module-runtime) should replace this
// copy so the loader and the linker cannot disagree.
const HOST_WORLDS: &[(&str, &str)] = &[
    ("tool", "p1:module/tool@1.0.0"),
    ("provider", "p1:module/provider@1.0.0"),
    ("context-policy", "p1:module/context-policy@1.0.0"),
    (
        "authorization-policy",
        "p1:module/authorization-policy@1.0.0",
    ),
    (
        "workflow-implementation",
        "p1:module/workflow-implementation@1.0.0",
    ),
    ("workflow-decision", "p1:module/workflow-decision@1.0.0"),
];

/// The package of p1's own WIT, whose interfaces are the only capabilities a module has.
const P1_INTERFACE_PREFIX: &str = "p1:module/";
/// Interfaces that carry types only and grant nothing (docs/design/modules/wit.md).
const GRANT_NOTHING: &[&str] = &["types", "worker-types"];

/// The per-class capability allocation of docs/design/modules/wit.md (freeze item 13).
// DRAFT(tag): S0.7 moves the allocation into frozen data; read it from there (or from a
// p1-module-runtime export) instead of this copy of the wit.md table.
fn allocation(kind: &str) -> &'static [&'static str] {
    match kind {
        "tool" => &[
            "control",
            "clock",
            "random",
            "notices",
            "workspace",
            "snapshot",
            "workspace-mutation",
            "process",
            "workers-start",
            "workers-observe",
            "workers-control",
            "workflows",
            "completion",
        ],
        "provider" => &[
            "control",
            "clock",
            "random",
            "notices",
            "http",
            "websocket",
            "credential-control",
        ],
        "context-policy" => &["control", "clock", "notices", "completion", "summary"],
        "authorization-policy" => &["control", "clock", "notices"],
        "workflow-implementation" => &[
            "control",
            "clock",
            "random",
            "notices",
            "workers-start",
            "workers-observe",
            "workers-control",
            "workflows",
        ],
        "workflow-decision" => &["control", "clock"],
        _ => &[],
    }
}

/// Whether an import outside p1's own interfaces is tolerated at load.
// DRAFT(tag): wit.md says no world imports WASI and the host links no `wasmtime-wasi`, but
// the fixture built for `wasm32-wasip2` today imports `wasi:cli`, `wasi:io` and
// `wasi:clocks` interfaces (S0-Q9, the guest-target question, is open). Until the tag
// settles S0-Q9 the loader tolerates `wasi:` imports so the fixture loads; the tag must
// either deliver a fixture without them (then return `false` here) or name the WASI
// interfaces a guest may import.
fn tolerated_foreign_import(interface: &str) -> bool {
    interface.starts_with("wasi:")
}

/// The frozen package manifest (docs/design/modules/package.md), as the build writes it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PackageManifest {
    pub name: String,
    pub kind: String,
    pub world: String,
    pub protocol: String,
    pub capabilities: Vec<String>,
    pub variant: String,
    pub digest: String,
    pub size: u64,
}

/// A package that passed every check, compiled from exactly the verified bytes. Nothing
/// is instantiated yet: that happens only when an environment assembles the module.
#[derive(Clone)]
pub struct VerifiedModule {
    /// The module name the lock resolved (the catalog key).
    pub module: String,
    /// The package manifest the checks were made against.
    pub manifest: PackageManifest,
    /// The loader-built identity: implementation from the manifest `name`, variant from
    /// the manifest `variant`. A package never names its own identity.
    pub identity: ToolIdentity,
    /// `sha256:<hex>` of the component bytes that were compiled.
    pub digest: String,
    /// The `.wasm` file the bytes were read from.
    pub source: PathBuf,
    /// The compiled component.
    pub component: Component,
}

impl std::fmt::Debug for VerifiedModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VerifiedModule")
            .field("module", &self.module)
            .field("manifest", &self.manifest)
            .field("identity", &self.identity)
            .field("digest", &self.digest)
            .field("source", &self.source)
            .finish_non_exhaustive()
    }
}

/// Which record a digest was compared against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DigestRecord {
    /// The release manifest's `packages` entry for the file.
    ReleaseManifest,
    /// The package manifest's `digest`.
    PackageManifest,
    /// The `modules.lock` resolution.
    ModulesLock,
}

impl std::fmt::Display for DigestRecord {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ReleaseManifest => "the release manifest",
            Self::PackageManifest => "the package manifest",
            Self::ModulesLock => "modules.lock",
        })
    }
}

/// Every way loading can fail. One variant per refusal, so a caller (and the tests) can
/// tell them apart without parsing text.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    /// The release manifest is missing, unreadable, or not `p1-release-manifest/1`.
    #[error("module release manifest {}: {message}", path.display())]
    ReleaseManifest { path: PathBuf, message: String },
    /// A package file could not be read or its manifest could not be parsed.
    #[error("module package file {}: {message}", path.display())]
    PackageFile { path: PathBuf, message: String },
    /// The package is not from p1's release: not named by the release manifest, or
    /// outside the reserved `p1/` namespace. `origin` names where it came from.
    #[error("module package `{package}` from {origin} is not an official p1 package: {reason}")]
    NotOfficial {
        package: String,
        origin: String,
        reason: String,
    },
    /// No package of the release has the name a lock resolved to.
    #[error(
        "module `{module}` resolves to package `{package}`, which p1's release at {} does not ship",
        release.display()
    )]
    PackageNotFound {
        module: String,
        package: String,
        release: PathBuf,
    },
    /// The bytes read are not the bytes a record names.
    #[error(
        "module package `{package}` ({}) has digest {actual}, but {record} records {expected}",
        path.display()
    )]
    DigestMismatch {
        package: String,
        path: PathBuf,
        record: DigestRecord,
        expected: String,
        actual: String,
    },
    /// The package's world or protocol is one this host does not implement.
    #[error(
        "module package `{package}` needs world `{world}` protocol {protocol}; this host implements \
         {supported:?} at protocol {host_protocol}"
    )]
    UnsupportedAbi {
        package: String,
        world: String,
        protocol: String,
        supported: Vec<String>,
        host_protocol: String,
    },
    /// The lock and the package disagree on a field other than the digest.
    #[error(
        "module `{module}`: modules.lock ({}) says {field} `{lock}`, the package manifest says `{package}`",
        lock_source.display()
    )]
    LockMismatch {
        module: String,
        field: &'static str,
        lock: String,
        package: String,
        lock_source: PathBuf,
    },
    /// Two packages of one release claim one identity (a name or a digest).
    #[error(
        "two module packages claim the identity `{identity}`: {} and {}",
        first.display(),
        second.display()
    )]
    DuplicateIdentity {
        identity: String,
        first: PathBuf,
        second: PathBuf,
    },
    /// The manifest grants a capability outside the class allocation.
    #[error(
        "module package `{package}` ({kind}) declares capability `{capability}` outside its class allocation"
    )]
    CapabilityNotAllocated {
        package: String,
        kind: String,
        capability: String,
    },
    /// The compiled component imports an interface the manifest does not grant.
    #[error("module package `{package}` imports `{import}`, which its manifest does not grant")]
    ImportNotGranted { package: String, import: String },
    /// The runtime's engine could not be built, so nothing can be compiled.
    #[error("the module engine is unavailable: {message}")]
    Engine { message: String },
    /// wasmtime refused the verified bytes.
    #[error("module package `{package}` does not compile: {message}")]
    Compile { package: String, message: String },
}

/// The subset of the release manifest the loader reads. Other fields belong to the
/// installer (ADR-0079) and are ignored here.
#[derive(Debug, Deserialize)]
struct ReleaseManifestFile {
    format: String,
    #[serde(default)]
    packages: Vec<ReleaseEntry>,
}

#[derive(Debug, Deserialize)]
struct ReleaseEntry {
    path: String,
    sha256: String,
}

/// One official package of the release: its manifest and where its component is.
#[derive(Debug, Clone)]
struct OfficialPackage {
    manifest: PackageManifest,
    manifest_path: PathBuf,
    wasm_path: PathBuf,
    /// The sha256 (bare hex) the release manifest records for the `.wasm`.
    wasm_sha256: String,
}

/// p1's installed module set, indexed by package name.
#[derive(Debug)]
pub struct Release {
    dir: PathBuf,
    official: BTreeMap<String, OfficialPackage>,
    /// Packages found on disk that the release manifest does not name, by the name their
    /// (untrusted) manifest claims: kept only to name the source in a refusal.
    unofficial: BTreeMap<String, PathBuf>,
}

impl Release {
    /// Read `<modules_dir>/manifest.json` and index the package tree. Every package
    /// manifest the release names is verified against its recorded sha256 here; the
    /// components are verified when [`Release::load`] reads them.
    pub fn open(modules_dir: &Path) -> Result<Self, LoadError> {
        let manifest_path = modules_dir.join(RELEASE_MANIFEST_FILE);
        let release_error = |message: String| LoadError::ReleaseManifest {
            path: manifest_path.clone(),
            message,
        };
        let text = std::fs::read_to_string(&manifest_path)
            .map_err(|error| release_error(error.to_string()))?;
        let release: ReleaseManifestFile =
            serde_json::from_str(&text).map_err(|error| release_error(error.to_string()))?;
        if release.format != RELEASE_MANIFEST_FORMAT {
            return Err(release_error(format!(
                "format `{}`, this host reads `{RELEASE_MANIFEST_FORMAT}`",
                release.format
            )));
        }
        let mut listed: BTreeMap<String, String> = BTreeMap::new();
        for entry in release.packages {
            if listed.insert(entry.path.clone(), entry.sha256).is_some() {
                return Err(release_error(format!("`{}` is listed twice", entry.path)));
            }
        }

        let mut official: BTreeMap<String, OfficialPackage> = BTreeMap::new();
        let mut by_digest: BTreeMap<String, PathBuf> = BTreeMap::new();
        let mut unofficial = BTreeMap::new();
        for (relative, path) in package_manifests(modules_dir)? {
            let bytes = std::fs::read(&path).map_err(|error| LoadError::PackageFile {
                path: path.clone(),
                message: error.to_string(),
            })?;
            let manifest: PackageManifest =
                serde_json::from_slice(&bytes).map_err(|error| LoadError::PackageFile {
                    path: path.clone(),
                    message: error.to_string(),
                })?;
            let wasm_relative = relative
                .strip_suffix(MANIFEST_SUFFIX)
                .map(|stem| format!("{stem}.wasm"))
                .unwrap_or_default();
            let (Some(manifest_sha), Some(wasm_sha)) =
                (listed.get(&relative), listed.get(&wasm_relative))
            else {
                unofficial.entry(manifest.name.clone()).or_insert(path);
                continue;
            };
            let actual = p1_usage::sha256_hex(&bytes);
            if &actual != manifest_sha {
                return Err(LoadError::DigestMismatch {
                    package: manifest.name.clone(),
                    path,
                    record: DigestRecord::ReleaseManifest,
                    expected: format!("sha256:{manifest_sha}"),
                    actual: format!("sha256:{actual}"),
                });
            }
            if let Some(first) = official.get(&manifest.name) {
                return Err(LoadError::DuplicateIdentity {
                    identity: manifest.name.clone(),
                    first: first.manifest_path.clone(),
                    second: path,
                });
            }
            if let Some(first) = by_digest.get(&manifest.digest) {
                return Err(LoadError::DuplicateIdentity {
                    identity: manifest.digest.clone(),
                    first: first.clone(),
                    second: path,
                });
            }
            by_digest.insert(manifest.digest.clone(), path.clone());
            official.insert(
                manifest.name.clone(),
                OfficialPackage {
                    wasm_path: modules_dir.join(&wasm_relative),
                    wasm_sha256: wasm_sha.clone(),
                    manifest,
                    manifest_path: path,
                },
            );
        }
        Ok(Self {
            dir: modules_dir.to_path_buf(),
            official,
            unofficial,
        })
    }

    /// The official package names, sorted.
    pub fn package_names(&self) -> Vec<String> {
        self.official.keys().cloned().collect()
    }

    /// Verify and compile the package `locked` resolves `module` to.
    pub fn load(
        &self,
        engine: &Engine,
        module: &str,
        locked: &LockedModule,
    ) -> Result<VerifiedModule, LoadError> {
        let namespace = locked
            .package
            .split_once('/')
            .map(|(namespace, _)| namespace);
        if namespace != Some(OFFICIAL_NAMESPACE) {
            return Err(LoadError::NotOfficial {
                package: locked.package.clone(),
                origin: locked.source.display().to_string(),
                reason: format!("only the reserved `{OFFICIAL_NAMESPACE}/` namespace is official"),
            });
        }
        let Some(package) = self.official.get(&locked.package) else {
            if let Some(source) = self.unofficial.get(&locked.package) {
                return Err(LoadError::NotOfficial {
                    package: locked.package.clone(),
                    origin: source.display().to_string(),
                    reason: format!(
                        "the release manifest {} does not name it",
                        self.dir.join(RELEASE_MANIFEST_FILE).display()
                    ),
                });
            }
            return Err(LoadError::PackageNotFound {
                module: module.to_string(),
                package: locked.package.clone(),
                release: self.dir.clone(),
            });
        };
        let manifest = &package.manifest;
        check_abi(manifest)?;
        let lock_mismatch =
            |field: &'static str, lock: String, package: String| LoadError::LockMismatch {
                module: module.to_string(),
                field,
                lock,
                package,
                lock_source: locked.source.clone(),
            };
        if locked.world != manifest.world {
            return Err(lock_mismatch(
                "world",
                locked.world.clone(),
                manifest.world.clone(),
            ));
        }
        if LockedProtocol::parse(&manifest.protocol) != Some(locked.protocol) {
            return Err(lock_mismatch(
                "protocol",
                locked.protocol.to_string(),
                manifest.protocol.clone(),
            ));
        }
        let allocated = allocation(&manifest.kind);
        for capability in &manifest.capabilities {
            if !allocated.contains(&capability.as_str()) {
                return Err(LoadError::CapabilityNotAllocated {
                    package: manifest.name.clone(),
                    kind: manifest.kind.clone(),
                    capability: capability.clone(),
                });
            }
        }

        // Read once, hash that buffer, compile that buffer.
        let bytes = std::fs::read(&package.wasm_path).map_err(|error| LoadError::PackageFile {
            path: package.wasm_path.clone(),
            message: error.to_string(),
        })?;
        let digest = format!("sha256:{}", p1_usage::sha256_hex(&bytes));
        for (record, expected) in [
            (
                DigestRecord::ReleaseManifest,
                format!("sha256:{}", package.wasm_sha256),
            ),
            (DigestRecord::PackageManifest, manifest.digest.clone()),
            (DigestRecord::ModulesLock, locked.digest.clone()),
        ] {
            if expected != digest {
                return Err(LoadError::DigestMismatch {
                    package: manifest.name.clone(),
                    path: package.wasm_path.clone(),
                    record,
                    expected,
                    actual: digest,
                });
            }
        }
        let component = Component::new(engine, &bytes).map_err(|error| LoadError::Compile {
            package: manifest.name.clone(),
            message: format!("{error:#}"),
        })?;
        check_imports(engine, &component, manifest)?;

        Ok(VerifiedModule {
            module: module.to_string(),
            identity: ToolIdentity {
                implementation: manifest.name.clone(),
                variant: manifest.variant.clone(),
            },
            manifest: manifest.clone(),
            digest,
            source: package.wasm_path.clone(),
            component,
        })
    }
}

/// Every `packages/<dir>/<file>.manifest.json` under the modules directory, as
/// (path relative to the modules directory, absolute path), sorted.
fn package_manifests(modules_dir: &Path) -> Result<Vec<(String, PathBuf)>, LoadError> {
    let root = modules_dir.join(PACKAGES_DIR);
    let read_dir = |dir: &Path| {
        std::fs::read_dir(dir).map_err(|error| LoadError::PackageFile {
            path: dir.to_path_buf(),
            message: error.to_string(),
        })
    };
    let mut found = Vec::new();
    if !root.is_dir() {
        return Ok(found);
    }
    for package in read_dir(&root)? {
        let package = package.map_err(|error| LoadError::PackageFile {
            path: root.clone(),
            message: error.to_string(),
        })?;
        let package_dir = package.path();
        if !package_dir.is_dir() {
            continue;
        }
        for file in read_dir(&package_dir)? {
            let file = file.map_err(|error| LoadError::PackageFile {
                path: package_dir.clone(),
                message: error.to_string(),
            })?;
            let name = file.file_name().to_string_lossy().into_owned();
            if !name.ends_with(MANIFEST_SUFFIX) {
                continue;
            }
            let relative = format!(
                "{PACKAGES_DIR}/{}/{name}",
                package.file_name().to_string_lossy()
            );
            found.push((relative, file.path()));
        }
    }
    found.sort();
    Ok(found)
}

/// World and protocol: the ABI check of docs/design/modules/protocol.md.
fn check_abi(manifest: &PackageManifest) -> Result<(), LoadError> {
    let world_ok = HOST_WORLDS
        .iter()
        .any(|(kind, world)| *kind == manifest.kind && *world == manifest.world);
    let protocol_ok = LockedProtocol::parse(&manifest.protocol).is_some_and(|protocol| {
        // Same major, and no minor newer than the host's.
        protocol.major == PROTOCOL_VERSION.major
            && (protocol.major, protocol.minor) <= (PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor)
    });
    if world_ok && protocol_ok {
        return Ok(());
    }
    Err(LoadError::UnsupportedAbi {
        package: manifest.name.clone(),
        world: manifest.world.clone(),
        protocol: manifest.protocol.clone(),
        supported: HOST_WORLDS
            .iter()
            .map(|(_, world)| world.to_string())
            .collect(),
        host_protocol: format!("{}.{}", PROTOCOL_VERSION.major, PROTOCOL_VERSION.minor),
    })
}

/// Every interface the component imports must be granted by the manifest.
fn check_imports(
    engine: &Engine,
    component: &Component,
    manifest: &PackageManifest,
) -> Result<(), LoadError> {
    let world_version = manifest.world.rsplit_once('@').map(|(_, version)| version);
    for (import, _) in component.component_type().imports(engine) {
        let granted = match import.strip_prefix(P1_INTERFACE_PREFIX) {
            Some(rest) => {
                let (interface, version) = match rest.split_once('@') {
                    Some((interface, version)) => (interface, Some(version)),
                    None => (rest, None),
                };
                version == world_version
                    && (GRANT_NOTHING.contains(&interface)
                        || manifest.capabilities.iter().any(|cap| cap == interface))
            }
            None => tolerated_foreign_import(import),
        };
        if !granted {
            return Err(LoadError::ImportNotGranted {
                package: manifest.name.clone(),
                import: import.to_string(),
            });
        }
    }
    Ok(())
}
