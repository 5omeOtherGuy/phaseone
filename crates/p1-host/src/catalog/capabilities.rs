//! Semantic capabilities (S1.5): what the host relies on a tool to DO, as opposed to
//! what it is called. Host behaviour that depends on which tool ran (the child's
//! completion policy, the worker report's `finish` call) asks for a capability here
//! and never compares an implementation name itself.
//!
//! A capability is a host fact about a VERIFIED identity, so neither a module nor an
//! environment can claim one:
//! - a package's capabilities are derived from what its verified manifest grants
//!   ([`package_capabilities`]); the frozen manifest (`docs/design/modules/package.md`)
//!   has no field for them, and the loader, not the module, builds the identity. The
//!   registration that assembles a loaded module records them ([`declare_package`]), so
//!   the checks below see a package tool too (ADR-0083 rule 7);
//! - a still-native tool's capabilities are declared by its catalog registration
//!   (`catalog/tools.rs`), keyed by the identity its constructor builds.
//!
//! An environment's face changes a tool's model-facing name, description and variant,
//! never its identity implementation, so a face can neither grant nor hide one.

use std::collections::HashMap;
use std::sync::{OnceLock, RwLock};

use p1_contracts::{Tool, ToolIdentity};
use p1_module_runtime::{LoadedModule, ModuleKind};

/// The capability interface whose grant lets a tool package run commands.
const PROCESS_INTERFACE: &str = "process";
/// The capability interface whose grant lets a tool package submit a completion.
const COMPLETION_INTERFACE: &str = "completion";

/// One thing the host may rely on a tool to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SemanticCapability {
    /// `records-command-evidence`: the tool's outcomes record the commands it ran, so
    /// its runs are evidence a `finish` verification can name (ADR-0051 item 1,
    /// ADR-0083 rule 2).
    RecordsCommandEvidence,
    /// `reports-completion`: a successful call ends the turn with a completion report
    /// (`status`, `needs`, `summary`) the host reads for the worker report.
    ReportsCompletion,
}

impl SemanticCapability {
    const ALL: [Self; 2] = [Self::RecordsCommandEvidence, Self::ReportsCompletion];

    /// The kebab-case name the ADR and diagnostics use.
    pub const fn name(self) -> &'static str {
        match self {
            Self::RecordsCommandEvidence => "records-command-evidence",
            Self::ReportsCompletion => "reports-completion",
        }
    }

    const fn bit(self) -> u8 {
        match self {
            Self::RecordsCommandEvidence => 1,
            Self::ReportsCompletion => 2,
        }
    }
}

/// A set of [`SemanticCapability`]. `Copy` and `const`-constructible, so a native
/// registration declares its set as plain data.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Capabilities(u8);

impl Capabilities {
    /// The empty set: what an undeclared identity carries.
    pub const NONE: Self = Self(0);

    /// The set holding exactly `capabilities`.
    pub const fn of(capabilities: &[SemanticCapability]) -> Self {
        let mut bits = 0;
        let mut index = 0;
        while index < capabilities.len() {
            bits |= capabilities[index].bit();
            index += 1;
        }
        Self(bits)
    }

    pub const fn contains(self, capability: SemanticCapability) -> bool {
        self.0 & capability.bit() != 0
    }

    /// The members, in declaration order (for diagnostics and the ADR's list).
    pub fn names(self) -> Vec<&'static str> {
        SemanticCapability::ALL
            .into_iter()
            .filter(|capability| self.contains(*capability))
            .map(SemanticCapability::name)
            .collect()
    }
}

/// What a still-native catalog registration declares: the identity implementation its
/// constructor builds and the capabilities that implementation has. It disappears with
/// the registration when the tool becomes a package, whose capabilities then come
/// from its manifest instead.
#[derive(Debug, Clone, Copy)]
pub struct NativeDeclaration {
    pub implementation: &'static str,
    pub capabilities: Capabilities,
}

/// The capabilities a verified package carries. The frozen manifest has no field for
/// them, so they are derived from verified metadata that exists: the module class and
/// the capability interfaces the manifest grants, which the loader has already checked
/// against the component's imports.
pub fn package_capabilities(module: &LoadedModule) -> Capabilities {
    derive(module.kind(), module.capabilities())
}

/// The derivation rule, apart from the loader so every class is covered:
/// - a `tool` granted `process` records command evidence: `process` is the only way a
///   component runs a command, the host's process service runs it, and each run is a
///   finished `Executes` call the host records from the event stream (never from the
///   component), so the command is known without trusting the module;
/// - a `tool` granted `completion` reports completion: `completion` is how a
///   component submits the turn's outcome, and ADR-0083 lets only a `tool`'s
///   `execute` commit it.
///
/// No other class runs tool calls, so a grant outside a `tool` carries nothing.
fn derive(kind: ModuleKind, granted: &[String]) -> Capabilities {
    if kind != ModuleKind::Tool {
        return Capabilities::NONE;
    }
    let grants = |interface: &str| granted.iter().any(|granted| granted == interface);
    let mut capabilities = Vec::new();
    if grants(PROCESS_INTERFACE) {
        capabilities.push(SemanticCapability::RecordsCommandEvidence);
    }
    if grants(COMPLETION_INTERFACE) {
        capabilities.push(SemanticCapability::ReportsCompletion);
    }
    Capabilities::of(&capabilities)
}

/// The package declarations made so far, keyed by the loader-built identity. Only the
/// loader's registration writes here (through [`declare_package`]), never a module, so a
/// package reachable by [`carries`] carries no more than its verified manifest grants.
fn package_declarations() -> &'static RwLock<HashMap<ToolIdentity, Capabilities>> {
    static DECLARATIONS: OnceLock<RwLock<HashMap<ToolIdentity, Capabilities>>> = OnceLock::new();
    DECLARATIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Record what a loaded package's verified manifest grants, keyed by the identity the
/// loader built, and return it. This is a package's equivalent of a [`NativeDeclaration`]:
/// the registration that assembles the module calls it (`catalog/modules.rs`, S1.4), the
/// way the native registrations list their declarations. Without it, a package tool's
/// derived capabilities would be invisible to [`declared`] — and to every check built on
/// [`carries`] — so a `tool` package granted `process` (ADR-0083 rules 2 and 7) could not
/// count as evidence.
pub fn declare_package(module: &LoadedModule) -> Capabilities {
    let capabilities = package_capabilities(module);
    package_declarations()
        .write()
        .expect("the package declarations lock is never held across a panic")
        .insert(module.identity().clone(), capabilities);
    capabilities
}

/// The capabilities `identity` carries: what the loader declared for that package
/// identity, else what a native registration declares for it, else none. A package is
/// keyed by its whole loader-built identity (name and variant), a native tool by its
/// implementation alone (its registrations build one identity each).
pub fn declared(identity: &ToolIdentity) -> Capabilities {
    if let Some(capabilities) = package_declarations()
        .read()
        .expect("the package declarations lock is never held across a panic")
        .get(identity)
    {
        return *capabilities;
    }
    super::tools::NATIVE_CAPABILITIES
        .iter()
        .find(|declaration| declaration.implementation == identity.implementation)
        .map_or(Capabilities::NONE, |declaration| declaration.capabilities)
}

/// Whether `tool` carries `capability`. Read from the tool's identity only: the
/// model-facing name, which a face may change, plays no part.
pub fn carries(tool: &dyn Tool, capability: SemanticCapability) -> bool {
    declared(tool.identity()).contains(capability)
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use p1_contracts::tool::ToolFace;
    use p1_module_tests::{FIXTURE_NAME, Release};
    use p1_testkit::FakeTool;

    use super::*;

    fn shell_in(dir: &std::path::Path) -> p1_tool_shell::ShellTool {
        p1_tool_shell::ShellTool::new(p1_workspace::Workspace::new(dir).expect("workspace"))
    }

    fn finish() -> p1_tool_finish::FinishTool {
        p1_tool_finish::FinishTool::new(
            Arc::new(crate::activity::ActivityLog::default()),
            p1_tool_finish::FinishOutcome::default(),
        )
    }

    #[test]
    fn the_names_are_kebab_case() {
        assert_eq!(
            Capabilities::of(&SemanticCapability::ALL).names(),
            ["records-command-evidence", "reports-completion"]
        );
        assert_eq!(Capabilities::NONE.names(), Vec::<&str>::new());
    }

    /// The native registrations declare the shell's and `finish`'s capabilities on the
    /// identities their constructors build, and nothing else carries either.
    #[test]
    fn the_native_registrations_declare_shell_and_finish() {
        let dir = tempfile::tempdir().unwrap();
        let shell = shell_in(dir.path());
        assert!(carries(&shell, SemanticCapability::RecordsCommandEvidence));
        assert!(!carries(&shell, SemanticCapability::ReportsCompletion));
        let finish = finish();
        assert!(carries(&finish, SemanticCapability::ReportsCompletion));
        assert!(!carries(
            &finish,
            SemanticCapability::RecordsCommandEvidence
        ));
        let read = p1_tool_read::ReadTool::new(
            p1_workspace::Workspace::new(dir.path()).unwrap(),
            Default::default(),
        );
        assert_eq!(declared(read.identity()), Capabilities::NONE);
    }

    /// A face can neither grant nor hide a capability: a shell presented as `run` with
    /// another variant still records command evidence, and a tool merely NAMED `shell`
    /// (or `finish`) carries nothing.
    #[test]
    fn a_face_neither_grants_nor_hides_a_capability() {
        let dir = tempfile::tempdir().unwrap();
        let renamed = shell_in(dir.path()).with_face(ToolFace::new("run", "Runs things."), "gpt");
        assert_eq!(renamed.declaration().name, "run");
        let renamed: Arc<dyn Tool> = Arc::new(renamed);
        assert!(carries(
            renamed.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
        let named_shell = FakeTool::new("shell").with_identity("fake-shell", "claude");
        assert!(!carries(
            &named_shell,
            SemanticCapability::RecordsCommandEvidence
        ));
        let named_finish = FakeTool::new("finish").with_identity("fake-finish", "claude");
        assert!(!carries(
            &named_finish,
            SemanticCapability::ReportsCompletion
        ));
    }

    #[test]
    fn only_a_tool_package_derives_capabilities_from_its_grants() {
        let granted = |list: &[&str]| list.iter().map(|name| name.to_string()).collect::<Vec<_>>();
        assert_eq!(
            derive(ModuleKind::Tool, &granted(&["control", "clock", "process"])),
            Capabilities::of(&[SemanticCapability::RecordsCommandEvidence])
        );
        assert_eq!(
            derive(ModuleKind::Tool, &granted(&["completion"])),
            Capabilities::of(&[SemanticCapability::ReportsCompletion])
        );
        assert_eq!(
            derive(ModuleKind::Tool, &granted(&["control", "clock"])),
            Capabilities::NONE
        );
        // `completion` is allocated to context policies too; they run no tool calls.
        assert_eq!(
            derive(ModuleKind::ContextPolicy, &granted(&["completion"])),
            Capabilities::NONE
        );
        assert_eq!(
            derive(ModuleKind::Provider, &granted(&["process"])),
            Capabilities::NONE
        );
    }

    /// The fixture package, loaded through S0's verifying loader: its identity and its
    /// capabilities come from the release manifest entry, and the same bytes under
    /// another manifest name are another identity, because the module has no way to
    /// state its own.
    #[test]
    fn a_package_identity_and_capabilities_come_from_its_verified_manifest() {
        let mut release = Release::with_fixture();
        let mut renamed = release.fixture_entry("p1/renamed-tool");
        renamed["variant"] = "other".into();
        let bytes = release.fixture().wasm.clone();
        release.add(renamed, &bytes);
        let loader = release.loader();

        let fixture = loader.load(FIXTURE_NAME).expect("the fixture loads");
        assert_eq!(
            fixture.identity(),
            &ToolIdentity {
                implementation: FIXTURE_NAME.to_string(),
                variant: "default".to_string(),
            }
        );
        // The fixture's manifest grants `process` to a `tool`.
        assert!(fixture.capabilities().iter().any(|c| c == "process"));
        assert_eq!(
            package_capabilities(&fixture),
            Capabilities::of(&[SemanticCapability::RecordsCommandEvidence])
        );

        let other = loader.load("p1/renamed-tool").expect("the same bytes load");
        assert_eq!(other.digest(), fixture.digest());
        assert_eq!(
            other.identity(),
            &ToolIdentity {
                implementation: "p1/renamed-tool".to_string(),
                variant: "other".to_string(),
            }
        );
        // A package identity is never a native declaration's: a package's capabilities
        // come from its grants alone.
        assert_eq!(declared(other.identity()), Capabilities::NONE);
    }

    /// The carrier ADR-0083 rule 7 needs once S1.4 registers package tools: when the
    /// registration declares a loaded module, a tool of that loader-built identity is
    /// visible to `carries`, whatever its model-facing name; until then, nothing is.
    #[test]
    fn a_declared_package_reaches_carries_through_its_verified_identity() {
        let release = Release::with_fixture();
        let module = release
            .loader()
            .load(FIXTURE_NAME)
            .expect("the fixture loads");
        let identity = module.identity().clone();
        let package_tool =
            FakeTool::new("recall").with_identity(&identity.implementation, &identity.variant);
        assert!(
            !carries(&package_tool, SemanticCapability::RecordsCommandEvidence),
            "an unregistered package identity carries nothing"
        );

        assert_eq!(
            declare_package(&module),
            Capabilities::of(&[SemanticCapability::RecordsCommandEvidence])
        );
        assert!(carries(
            &package_tool,
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(!carries(
            &package_tool,
            SemanticCapability::ReportsCompletion
        ));
        // The declaration is the identity's, not the name's: another tool merely NAMED
        // the same thing, with another identity, carries nothing.
        let same_name = FakeTool::new("recall").with_identity("fake-recall", "test");
        assert!(!carries(
            &same_name,
            SemanticCapability::RecordsCommandEvidence
        ));
    }

    /// A capability cannot be had without its verified grant: the fixture imports
    /// `process`, so a manifest entry that withholds the grant is refused by the
    /// loader rather than loading a module that could run commands unrecorded.
    #[test]
    fn a_package_without_the_grant_is_refused_not_downgraded() {
        let mut release = Release::empty();
        let mut narrow = release.fixture_entry("p1/narrow");
        narrow["capabilities"] = serde_json::json!(["control", "clock"]);
        let bytes = release.fixture().wasm.clone();
        release.add(narrow, &bytes);
        let error = match release.loader().load("p1/narrow") {
            Ok(_) => panic!("a module importing an ungranted `process` must not load"),
            Err(error) => error,
        };
        assert!(
            matches!(
                error,
                p1_module_runtime::LoadError::UndeclaredImport { ref import, .. }
                    if import.contains("process")
            ),
            "{error}"
        );
    }
}
