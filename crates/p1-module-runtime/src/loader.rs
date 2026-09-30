//! The loader (freeze item 6): a module comes from p1's release manifest, by name, and
//! nowhere else. There is no API that loads a path or bytes the caller chose.
//!
//! `load` reads the component bytes once, computes their SHA-256, compares it with the
//! manifest digest and only then compiles THOSE bytes, from memory, with
//! [`Component::from_binary`]: nothing is read twice (a file swapped between the check and
//! the compile cannot be the one compiled), no text format is accepted, and nothing is ever
//! deserialized from a compiled cache (wasmtime's `cache` feature is not built, and
//! `Component::deserialize*` is never called), so the digest check is the whole trust
//! decision.

use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use p1_contracts::ToolIdentity;
use p1_module_protocol::PROTOCOL_VERSION;
use thiserror::Error;
use tokio::sync::watch;
use wasmtime::Engine;
use wasmtime::component::Component;

use crate::manifest::{Digest, ReleaseManifest};
use crate::{RuntimeError, engine};

/// The namespace of the packages p1 builds and ships (`docs/design/modules/package.md`).
pub const OFFICIAL_NAMESPACE: &str = "p1";

/// The WIT package every world and capability interface of this runtime belongs to.
const WIT_PACKAGE: &str = "p1:module";
/// The WIT package version this runtime links.
const WIT_VERSION: &str = "1.0.0";

/// How often the engine's epoch advances: the unit of every per-call wall-clock deadline.
/// Short enough that a deadline or a cancellation lands promptly, long enough that the
/// ticker thread costs nothing measurable.
pub const EPOCH_TICK: Duration = Duration::from_millis(10);

/// The capabilities this runtime can link, by the interface name the manifest uses. The
/// other capability interfaces belong to other native crates and streams; a manifest that
/// grants one of them is refused until the runtime can provide it. `summary` is linked to
/// the caller's [`SummaryService`](crate::context_policy::SummaryService) (S5, GO S5-B7);
/// `completion` to the caller's [`CompletionService`](crate::completion::CompletionService)
/// (S3.7, D067); `http`, `websocket` and `credential-control` to the broker and credential
/// services a provider component's `stream` uses (S4.7); the worker and workflow interfaces
/// to the caller's services in [`crate::delegation`] (S6, B-S6-8); `workspace` (its read side)
/// and `snapshot` to the caller's [`WorkspaceService`](crate::capabilities::WorkspaceService)
/// and [`SnapshotService`](crate::capabilities::SnapshotService) (S1); `workspace-mutation` to
/// the caller's [`MutationService`](crate::capabilities::MutationService) (S2, beside S1's);
/// `tool-outputs` to the caller's [`ToolOutputsService`](crate::outputs::ToolOutputsService)
/// (ADR-0109).
///
/// Public, and re-exported from the crate root, because it is the ONE list of what this
/// runtime links: the host's `p1 modules verify` checks a manifest against it instead of
/// keeping a copy that could drift (S1.5.1). A new capability is added here alone.
pub const LINKABLE_CAPABILITIES: [&str; 17] = [
    "control",
    "clock",
    "random",
    "process",
    "summary",
    "completion",
    "http",
    "websocket",
    "credential-control",
    "workers-start",
    "workers-observe",
    "workers-control",
    "workflows",
    "workspace",
    "snapshot",
    "workspace-mutation",
    "tool-outputs",
];

/// The interface every world imports for its types; it grants nothing.
const TYPES_INTERFACE: &str = "types";
/// The type-only interface the worker capabilities take their records from
/// (`modules/capabilities.toml`, `type-only`): it grants nothing, so a manifest need not
/// declare it.
const WORKER_TYPES_INTERFACE: &str = crate::delegation::WORKER_TYPES_INTERFACE;

/// Why a module could not be loaded. Every refusal names the module.
#[derive(Debug, Error)]
pub enum LoadError {
    /// The runtime could not be set up.
    #[error(transparent)]
    Runtime(#[from] RuntimeError),
    /// The name is not in the reserved `p1/` namespace: only packages p1 builds are loaded.
    #[error(
        "module {name} is refused: only official packages in the reserved {OFFICIAL_NAMESPACE}/ namespace are loaded"
    )]
    NotOfficial {
        /// The name asked for.
        name: String,
    },
    /// The release manifest has no package of that name.
    #[error("module {name} is not in the release manifest; modules load only from it")]
    NotInManifest {
        /// The name asked for.
        name: String,
    },
    /// The component file named by the manifest could not be read as a regular file.
    #[error("module {name}: cannot read {path}: {reason}")]
    Read {
        /// The module.
        name: String,
        /// The file.
        path: PathBuf,
        /// Why.
        reason: String,
    },
    /// The bytes are not the ones the release manifest pins.
    #[error(
        "module {name} failed verification: the manifest digest is {expected}, the bytes are {actual}"
    )]
    DigestMismatch {
        /// The module.
        name: String,
        /// The digest the manifest pins.
        expected: Digest,
        /// The digest of the bytes read.
        actual: Digest,
    },
    /// The manifest names a module class this runtime does not know.
    #[error("module {name}: kind {kind} is not a module class this runtime speaks")]
    UnknownKind {
        /// The module.
        name: String,
        /// The kind as written.
        kind: String,
    },
    /// The manifest's world is not the world of its kind this runtime links.
    #[error("module {name}: world {world} is not {expected}, the {kind} world this runtime speaks")]
    WorldMismatch {
        /// The module.
        name: String,
        /// The module class.
        kind: String,
        /// The world as written.
        world: String,
        /// The world this runtime speaks for the kind.
        expected: String,
    },
    /// The manifest's protocol major is not the runtime's.
    #[error(
        "module {name}: protocol {protocol} is refused, this runtime speaks protocol major {major}"
    )]
    ProtocolMismatch {
        /// The module.
        name: String,
        /// The protocol as written.
        protocol: String,
        /// The major this runtime speaks.
        major: u32,
    },
    /// The manifest grants a capability this runtime cannot link.
    #[error("module {name}: capability {capability} cannot be linked by this runtime")]
    UnsupportedCapability {
        /// The module.
        name: String,
        /// The capability as written.
        capability: String,
    },
    /// The component imports something its manifest does not grant; any `wasi:` import is
    /// one (decision D-XO-4: a guest has no WASI surface).
    #[error("module {name} imports {import}, which its manifest does not grant")]
    UndeclaredImport {
        /// The module.
        name: String,
        /// The import as the component names it.
        import: String,
    },
    /// wasmtime refused the verified bytes.
    #[error("module {name}: the component does not compile: {reason}")]
    Compile {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
}

/// A module class and the world this runtime links it as.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ModuleKind {
    /// `p1_contracts::Tool`.
    Tool,
    /// `p1_contracts::Provider`.
    Provider,
    /// `p1_contracts::ContextPolicy`.
    ContextPolicy,
    /// `p1_contracts::AuthorizationPolicy`.
    AuthorizationPolicy,
    /// A workflow implemented as a module.
    WorkflowImplementation,
    /// A workflow's step decisions (`p1_workflow::Decisions`), its state kept native.
    WorkflowDecision,
}

impl ModuleKind {
    /// Every class this runtime speaks, in the order a manifest may name them. Public
    /// because the host's `p1 modules verify` reads a manifest's `kind` without the loader
    /// (it may not compile) and must accept exactly the classes the loader does (S1.5.1).
    pub const ALL: [Self; 6] = [
        Self::Tool,
        Self::Provider,
        Self::ContextPolicy,
        Self::AuthorizationPolicy,
        Self::WorkflowImplementation,
        Self::WorkflowDecision,
    ];

    /// The manifest name of the class.
    pub fn name(self) -> &'static str {
        match self {
            Self::Tool => "tool",
            Self::Provider => "provider",
            Self::ContextPolicy => "context-policy",
            Self::AuthorizationPolicy => "authorization-policy",
            Self::WorkflowImplementation => "workflow-implementation",
            Self::WorkflowDecision => "workflow-decision",
        }
    }

    /// The world of the class, e.g. `p1:module/tool@1.0.0`.
    pub fn world(self) -> String {
        format!("{WIT_PACKAGE}/{}@{WIT_VERSION}", self.name())
    }

    fn parse(name: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|kind| kind.name() == name)
    }
}

/// The import name of capability interface `interface`, e.g. `p1:module/clock@1.0.0`.
pub(crate) fn interface_import(interface: &str) -> String {
    format!("{WIT_PACKAGE}/{interface}@{WIT_VERSION}")
}

/// The epoch clock of one engine: how many [`EPOCH_TICK`]s have passed, as the per-call
/// deadlines count them.
///
/// The count lives beside the engine's own epoch because the engine's epoch also advances
/// for another reason: a cancelled call bumps it ([`Epochs::interrupt`]) so that a guest in
/// a CPU loop reaches its epoch callback at once. Execute deadlines read only this count, so
/// an interrupt never brings another call's deadline closer. The restricted backstop of
/// [`crate::restricted`] is the exception: it is the engine's own epoch, so each interrupt
/// spends one of its [`RESTRICTED_DEADLINE_TICKS`](crate::restricted::RESTRICTED_DEADLINE_TICKS)
/// ticks and may end an inspection early, which is why the fuel bound rather than the
/// backstop is what normally stops a runaway inspection.
pub(crate) struct Epochs {
    engine: Engine,
    ticks: watch::Sender<u64>,
}

impl Epochs {
    fn new(engine: Engine) -> Arc<Self> {
        Arc::new(Self {
            engine,
            ticks: watch::Sender::new(0),
        })
    }

    /// Starts the production ticker: one thread per engine advancing the clock every
    /// [`EPOCH_TICK`], so deadlines hold whatever the caller's Tokio flavour and however busy
    /// its threads are. The thread holds only a weak reference and ends within one tick of
    /// the last owner dropping the clock.
    fn start_ticker(self: &Arc<Self>) -> Result<(), RuntimeError> {
        let epochs = Arc::downgrade(self);
        thread::Builder::new()
            .name("p1-module-epoch".to_owned())
            .spawn(move || {
                loop {
                    thread::sleep(EPOCH_TICK);
                    match epochs.upgrade() {
                        Some(epochs) => epochs.advance(1),
                        None => break,
                    }
                }
            })
            .map_err(RuntimeError::Ticker)?;
        Ok(())
    }

    /// Advances the clock by `ticks`, then the engine's epoch, so an epoch callback that
    /// runs because of this advance already reads the new count.
    pub(crate) fn advance(&self, ticks: u64) {
        self.ticks
            .send_modify(|now| *now = now.saturating_add(ticks));
        for _ in 0..ticks {
            self.engine.increment_epoch();
        }
    }

    /// Makes every running guest of this engine reach its epoch callback at its next check,
    /// without advancing the clock.
    pub(crate) fn interrupt(&self) {
        self.engine.increment_epoch();
    }

    /// A receiver of the clock, for reading it and waiting on it.
    pub(crate) fn subscribe(&self) -> watch::Receiver<u64> {
        self.ticks.subscribe()
    }
}

/// The epochs of a loader built with [`Loader::with_manual_epochs`]: nothing advances them
/// but [`ManualEpochs::advance`], so a test drives deadlines explicitly instead of sleeping.
#[derive(Clone)]
pub struct ManualEpochs {
    epochs: Arc<Epochs>,
}

impl ManualEpochs {
    /// Advances the epoch clock by `ticks` [`EPOCH_TICK`]s.
    pub fn advance(&self, ticks: u64) {
        self.epochs.advance(ticks);
    }
}

/// Loads modules by name from one release manifest and the directory it describes.
pub struct Loader {
    manifest: ReleaseManifest,
    root: PathBuf,
    engine: Engine,
    epochs: Arc<Epochs>,
}

fn unsupported_kind_capability(kind: ModuleKind, capabilities: &[String]) -> Option<&'static str> {
    if kind == ModuleKind::ContextPolicy && capabilities.iter().any(|name| name == "completion") {
        Some("completion")
    } else {
        None
    }
}

impl Loader {
    /// A loader over `manifest`, whose entry paths are relative to `root` (the directory the
    /// manifest file is in). Its epochs advance on the production ticker.
    pub fn new(manifest: ReleaseManifest, root: impl Into<PathBuf>) -> Result<Self, LoadError> {
        let loader = Self::unticked(manifest, root.into())?;
        loader.epochs.start_ticker()?;
        Ok(loader)
    }

    /// A loader whose epochs advance only through the returned [`ManualEpochs`]: the test
    /// hook for deadlines.
    pub fn with_manual_epochs(
        manifest: ReleaseManifest,
        root: impl Into<PathBuf>,
    ) -> Result<(Self, ManualEpochs), LoadError> {
        let loader = Self::unticked(manifest, root.into())?;
        let epochs = ManualEpochs {
            epochs: loader.epochs.clone(),
        };
        Ok((loader, epochs))
    }

    fn unticked(manifest: ReleaseManifest, root: PathBuf) -> Result<Self, LoadError> {
        let engine = engine()?;
        let epochs = Epochs::new(engine.clone());
        Ok(Self {
            manifest,
            root,
            engine,
            epochs,
        })
    }

    /// Verifies and compiles the package `name` of the release manifest.
    pub fn load(&self, name: &str) -> Result<LoadedModule, LoadError> {
        check_official_name(name)?;
        let entry = self
            .manifest
            .entry(name)
            .ok_or_else(|| LoadError::NotInManifest {
                name: name.to_owned(),
            })?;

        let kind = check_manifest_fields(entry)?;
        // The frozen class allocation permits completion, but this context adapter has
        // no session completion record: refuse it before component construction rather
        // than leave a granted import without a service at assembly time.
        if unsupported_kind_capability(kind, &entry.capabilities) == Some("completion") {
            return Err(LoadError::UnsupportedCapability {
                name: name.to_owned(),
                capability: "completion".to_owned(),
            });
        }
        if let Some(capability) = entry
            .capabilities
            .iter()
            .find(|capability| !LINKABLE_CAPABILITIES.contains(&capability.as_str()))
        {
            return Err(LoadError::UnsupportedCapability {
                name: name.to_owned(),
                capability: capability.clone(),
            });
        }

        let path = self.root.join(&entry.path);
        let read_error = |reason: String| LoadError::Read {
            name: name.to_owned(),
            path: path.clone(),
            reason,
        };
        // A symlink could name a file outside the release; only a regular file is its own.
        let metadata =
            std::fs::symlink_metadata(&path).map_err(|error| read_error(error.to_string()))?;
        if !metadata.file_type().is_file() {
            return Err(read_error("not a regular file".to_owned()));
        }
        let bytes = std::fs::read(&path).map_err(|error| read_error(error.to_string()))?;

        let actual = Digest::of(&bytes);
        if actual != entry.digest {
            return Err(LoadError::DigestMismatch {
                name: name.to_owned(),
                expected: entry.digest,
                actual,
            });
        }
        check_component_header(&bytes).map_err(|reason| LoadError::Compile {
            name: name.to_owned(),
            reason,
        })?;
        // The verified bytes, not the file: see the module documentation.
        let component =
            Component::from_binary(&self.engine, &bytes).map_err(|error| LoadError::Compile {
                name: name.to_owned(),
                reason: format!("{error:#}"),
            })?;

        let allowed: Vec<String> = entry
            .capabilities
            .iter()
            .map(|capability| interface_import(capability))
            .chain([
                interface_import(TYPES_INTERFACE),
                interface_import(WORKER_TYPES_INTERFACE),
            ])
            .collect();
        let component_type = component.component_type();
        if let Some((import, _)) = component_type
            .imports(&self.engine)
            .find(|(import, _)| !allowed.iter().any(|allowed| allowed == import))
        {
            return Err(LoadError::UndeclaredImport {
                name: name.to_owned(),
                import: import.to_owned(),
            });
        }

        Ok(LoadedModule {
            name: entry.name.clone(),
            digest: actual,
            kind,
            abi: format!("{}+{}", entry.world, entry.protocol),
            capabilities: entry.capabilities.clone(),
            identity: ToolIdentity {
                implementation: entry.name.clone(),
                variant: entry.variant.clone(),
            },
            component,
            engine: self.engine.clone(),
            epochs: self.epochs.clone(),
        })
    }
}

/// Check the loader's reserved package namespace without reading a component.
pub fn check_official_name(name: &str) -> Result<(), LoadError> {
    if name.split_once('/').map(|(namespace, _)| namespace) != Some(OFFICIAL_NAMESPACE) {
        return Err(LoadError::NotOfficial {
            name: name.to_owned(),
        });
    }
    Ok(())
}

/// Check the manifest fields that do not require reading or compiling a component.
pub fn check_manifest_fields(
    entry: &crate::manifest::ComponentEntry,
) -> Result<ModuleKind, LoadError> {
    if let Some(error) = manifest_field_errors(entry).into_iter().next() {
        return Err(error);
    }
    Ok(ModuleKind::parse(&entry.kind).expect("a valid manifest has a known kind"))
}

/// All metadata-only field errors, in loader refusal order. The loader takes the first;
/// installer verification reports every independent problem in a staged entry.
pub fn manifest_field_errors(entry: &crate::manifest::ComponentEntry) -> Vec<LoadError> {
    let mut errors = Vec::new();
    if let Err(error) = check_official_name(&entry.name) {
        errors.push(error);
    }
    let Some(kind) = ModuleKind::parse(&entry.kind) else {
        errors.push(LoadError::UnknownKind {
            name: entry.name.clone(),
            kind: entry.kind.clone(),
        });
        return errors;
    };
    if entry.world != kind.world() {
        errors.push(LoadError::WorldMismatch {
            name: entry.name.clone(),
            kind: entry.kind.clone(),
            world: entry.world.clone(),
            expected: kind.world(),
        });
    }
    if protocol_major(&entry.protocol) != Some(PROTOCOL_VERSION.major) {
        errors.push(LoadError::ProtocolMismatch {
            name: entry.name.clone(),
            protocol: entry.protocol.clone(),
            major: PROTOCOL_VERSION.major,
        });
    }
    errors
}

/// Reject headers that the component compiler cannot accept, without compiling bytes.
/// Wasm components have the magic, component version 13 and layer 1.
pub fn check_component_header(bytes: &[u8]) -> Result<(), String> {
    if bytes.len() < 8 || &bytes[..4] != b"\0asm" {
        return Err("the file is not a WebAssembly module: it has no wasm header".into());
    }
    let version = u16::from_le_bytes([bytes[4], bytes[5]]);
    if version != 13 {
        return Err(format!(
            "the file is not a component: its wasm version is {version}, the component version is 13"
        ));
    }
    let layer = u16::from_le_bytes([bytes[6], bytes[7]]);
    if layer != 1 {
        return Err(format!(
            "the file is not a component: its wasm layer is {layer}, the component layer is 1"
        ));
    }
    Ok(())
}

/// The major of a `major.minor` protocol version, or `None` when it is not one.
fn protocol_major(protocol: &str) -> Option<u32> {
    let (major, minor) = protocol.split_once('.')?;
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|byte| byte.is_ascii_digit());
    if !digits(major) || !digits(minor) {
        return None;
    }
    major.parse().ok()
}

/// A verified, compiled module: what a module adapter is built from.
pub struct LoadedModule {
    name: String,
    digest: Digest,
    kind: ModuleKind,
    abi: String,
    capabilities: Vec<String>,
    identity: ToolIdentity,
    pub(crate) component: Component,
    pub(crate) engine: Engine,
    pub(crate) epochs: Arc<Epochs>,
}

impl LoadedModule {
    /// The manifest name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The module's identity: the digest of the bytes that were verified and compiled.
    pub fn digest(&self) -> Digest {
        self.digest
    }

    /// The verified manifest's world and protocol.
    pub fn abi(&self) -> &str {
        &self.abi
    }

    /// The module class.
    pub fn kind(&self) -> ModuleKind {
        self.kind
    }

    /// The capabilities the manifest grants, as written.
    pub fn capabilities(&self) -> &[String] {
        &self.capabilities
    }

    /// The loader-built identity: implementation from the manifest name, variant from the
    /// manifest. A module never reports its own.
    pub fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_protocol_is_major_dot_minor() {
        assert_eq!(protocol_major("1.0"), Some(1));
        assert_eq!(protocol_major("1.7"), Some(1));
        assert_eq!(protocol_major("2.0"), Some(2));
        for bad in ["1", "1.", ".0", "1.0.0", "a.0", "+1.0", ""] {
            assert_eq!(protocol_major(bad), None, "{bad}");
        }
    }

    #[test]
    fn context_completion_is_refused_before_assembly() {
        assert_eq!(
            unsupported_kind_capability(ModuleKind::ContextPolicy, &["completion".to_owned()]),
            Some("completion")
        );
        assert_eq!(
            unsupported_kind_capability(ModuleKind::Tool, &["completion".to_owned()]),
            None
        );
    }

    #[test]
    fn each_kind_has_its_world() {
        assert_eq!(ModuleKind::Tool.world(), "p1:module/tool@1.0.0");
        assert_eq!(
            ModuleKind::parse("context-policy"),
            Some(ModuleKind::ContextPolicy)
        );
        assert_eq!(ModuleKind::parse("plugin"), None);
    }

    #[test]
    fn the_linkable_capabilities_are_the_old_four_plus_every_linked_service() {
        assert_eq!(
            LINKABLE_CAPABILITIES,
            [
                "control",
                "clock",
                "random",
                "process",
                "summary",
                "completion",
                "http",
                "websocket",
                "credential-control",
                "workers-start",
                "workers-observe",
                "workers-control",
                "workflows",
                "workspace",
                "snapshot",
                "workspace-mutation",
                "tool-outputs",
            ]
        );
        for refused in ["notices", "filesystem"] {
            assert!(!LINKABLE_CAPABILITIES.contains(&refused), "{refused}");
        }
    }
}
