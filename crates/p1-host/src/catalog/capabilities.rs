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
//!   host's registration records them for every tool package it accepts
//!   ([`declare_package`], `catalog/modules.rs`), so the checks below see a package tool
//!   too (ADR-0083 rule 7);
//! - a still-native tool's capabilities are declared by its catalog registration
//!   (`catalog/tools.rs`), keyed by the identity its constructor builds.
//!
//! An assembled tool carries the snapshot of ITS generation's verified sources
//! ([`bind_assembled`]): the tool object handed to the agent is a wrapper holding the
//! capabilities, so nothing shared between sessions or generations is consulted and an old
//! generation's tool keeps its grants whatever a later one binds. The declaration lookup
//! remains only for standalone native tools and test fixtures nobody bound.
//!
//! An environment's face changes a tool's model-facing name, description and variant,
//! never its identity implementation, so a face can neither grant nor hide one.
//!
//! notice: crates/p1-host/src/catalog/capabilities.rs (S1): S3.8 (D083b) makes `declared`
//! fall back to a declared package of the same identity IMPLEMENTATION, so a host-applied
//! presentation variant of a verified package (the shell's `+sandbox`) carries the package's
//! capabilities; the exact-identity lookup and the native declarations are unchanged.

use std::any::Any;
use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolOutcome, ToolResultItem,
};
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

/// Package declarations keyed by loader-built identity AND verified digest. An assembled
/// tool binds this value at construction, so later generations cannot rewrite its grants.
/// Only the loader's registration writes here (through [`declare_package`]), never a module, so a
/// package reachable by [`carries`] carries no more than its verified manifest grants.
fn package_declarations() -> &'static RwLock<HashMap<(ToolIdentity, String), Capabilities>> {
    static DECLARATIONS: OnceLock<RwLock<HashMap<(ToolIdentity, String), Capabilities>>> =
        OnceLock::new();
    DECLARATIONS.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Record what a loaded package's verified manifest grants, keyed by identity and digest,
/// and return it. This is a package's equivalent of a [`NativeDeclaration`]:
/// the host's registration calls it for every tool package it accepts (`catalog/modules.rs`,
/// `register_modules`), the way the native registrations list their declarations. Without
/// it, a package tool's derived capabilities would be invisible to [`declared`] — and to
/// every check built on [`carries`] — so a `tool` package granted `process` (ADR-0083 rules 2
/// and 7) could not count as evidence.
pub fn declare_package(module: &LoadedModule) -> Capabilities {
    let capabilities = package_capabilities(module);
    package_declarations()
        .write()
        .expect("the package declarations lock is never held across a panic")
        .insert(
            (module.identity().clone(), module.digest().to_string()),
            capabilities,
        );
    capabilities
}

/// Compatibility lookup for standalone native tools and test fixtures that are not part
/// of a host assembly. Production assemblies bind grants by verified digest. A package is
/// keyed by its whole loader-built identity (name and variant), a native tool by its
/// implementation alone (its registrations build one identity each).
///
/// A host-applied presentation VARIANT of a declared package — the shell entry's
/// `+sandbox` (ADR-0083 §1) or an environment's face — still carries the package's
/// capabilities: the grant is the manifest's and belongs to the package, not to the
/// variant the host presents it under.
pub fn declared(identity: &ToolIdentity) -> Capabilities {
    let declarations = package_declarations()
        .read()
        .expect("the package declarations lock is never held across a panic");
    if let Some(capabilities) = declarations
        .iter()
        .find(|((declared, _digest), _)| declared == identity)
        .map(|(_, capabilities)| capabilities)
    {
        return *capabilities;
    }
    // A host-applied presentation variant of a declared package still carries the
    // package's capabilities: the grant is the manifest's and belongs to the package,
    // not to the variant the host presents it under.
    if let Some(capabilities) = declarations
        .iter()
        .find(|((declared, _digest), _)| declared.implementation == identity.implementation)
        .map(|(_, capabilities)| *capabilities)
    {
        return capabilities;
    }
    super::tools::NATIVE_CAPABILITIES
        .iter()
        .find(|declaration| declaration.implementation == identity.implementation)
        .map_or(Capabilities::NONE, |declaration| declaration.capabilities)
}

/// A tool as one verified generation assembled it: the object itself plus the immutable
/// capability snapshot of that generation's verified sources. The snapshot travels with the
/// assembly's own tool object, so no table shared between sessions is involved, and an old
/// generation's tool keeps its grants whatever a later generation binds.
struct BoundTool {
    inner: Arc<dyn Tool>,
    capabilities: Capabilities,
}

impl Tool for BoundTool {
    fn declaration(&self) -> &ToolDeclaration {
        self.inner.declaration()
    }

    fn identity(&self) -> &ToolIdentity {
        self.inner.identity()
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn synthetic_command_result(&self) -> bool {
        self.inner.synthetic_command_result()
    }

    fn take_command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.take_command_exit_code(call_id)
    }

    fn command_exit_code(&self, call_id: &str) -> Option<i32> {
        self.inner.command_exit_code(call_id)
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        self.inner.describe(call)
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        self.inner.describe_result(call, result)
    }

    /// ADR-0120: a delegating wrapper forwards the inner tool's answer.
    fn ends_turn(&self, outcome: &ToolOutcome) -> bool {
        self.inner.ends_turn(outcome)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        self.inner.execute(call, context)
    }

    fn as_any(&self) -> Option<&dyn Any> {
        Some(self)
    }
}

/// The tool object without a binding of an earlier generation, so a re-bind replaces the
/// snapshot instead of stacking wrappers.
fn unbound(tool: &Arc<dyn Tool>) -> Arc<dyn Tool> {
    match tool
        .as_any()
        .and_then(|any| any.downcast_ref::<BoundTool>())
    {
        Some(bound) => bound.inner.clone(),
        None => tool.clone(),
    }
}

/// Bind `capabilities` to `tool`: the returned object carries them.
fn bind(tool: &Arc<dyn Tool>, capabilities: Capabilities) -> Arc<dyn Tool> {
    Arc::new(BoundTool {
        inner: unbound(tool),
        capabilities,
    })
}

/// Snapshot capability grants for this assembly from its verified generation: the assembled
/// tools are replaced by objects that carry them.
pub fn bind_assembled(
    assembled: &mut p1_assembly::Assembled,
    sources: &super::modules::VerifiedSources,
) {
    bind_tools(&mut assembled.tools, &assembled.resolved.tools, sources);
}

/// Bind a tool set, also after a completion handoff replaced its finish object.
pub fn bind_tools(
    tools: &mut [Arc<dyn Tool>],
    resolved: &[p1_assembly::ResolvedTool],
    sources: &super::modules::VerifiedSources,
) {
    for (tool, resolved) in tools.iter_mut().zip(resolved) {
        let capabilities = if let Some(package) = sources.resolve(&resolved.module) {
            package.semantic
        } else {
            super::tools::NATIVE_CAPABILITIES
                .iter()
                .find(|native| native.implementation == tool.identity().implementation)
                .map_or(Capabilities::NONE, |native| native.capabilities)
        };
        *tool = bind(tool, capabilities);
    }
}

/// Whether `tool` carries `capability`. Read from the tool's identity only: the
/// model-facing name, which a face may change, plays no part. An assembled tool answers
/// from the snapshot its generation bound; a standalone fixture tool falls back to the
/// declaration lookup.
pub fn carries(tool: &dyn Tool, capability: SemanticCapability) -> bool {
    if let Some(bound) = tool
        .as_any()
        .and_then(|any| any.downcast_ref::<BoundTool>())
    {
        return bound.capabilities.contains(capability);
    }
    declared(tool.identity()).contains(capability)
}

/// The built package `package`, loaded through a test release whose one entry is the
/// package's own build manifest: the identity and the grants the loader verifies. Test-only
/// support for the cases that need a REAL package rather than the fixture.
#[cfg(test)]
pub(crate) fn built_package(package: &str) -> p1_module_runtime::LoadedModule {
    use p1_module_tests::Release;
    let dir = p1_module_tests::fixture_dir()
        .parent()
        .expect("the publish directory")
        .join(package);
    let read = |file: String| {
        let path = dir.join(file);
        std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "{} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    };
    let wasm = read(format!("{package}.wasm"));
    let manifest: p1_contracts::serde_json::Value =
        p1_contracts::serde_json::from_slice(&read(format!("{package}.manifest.json")))
            .expect("the package manifest is JSON");
    let entry = p1_contracts::serde_json::json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("packages/{package}/{package}.wasm"),
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"],
        "variant": manifest["variant"],
    });
    let name = manifest["name"]
        .as_str()
        .expect("the manifest name")
        .to_owned();
    let mut release = Release::empty();
    release.add(entry, &wasm);
    release.loader().load(&name).expect("the package loads")
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use p1_contracts::tool::ToolFace;
    use p1_module_tests::{FIXTURE_NAME, Release};
    use p1_testkit::FakeTool;

    use super::*;

    #[test]
    fn same_bytes_new_grants_do_not_change_old_generation() {
        let first = Release::with_fixture();
        let old = first.loader().load(FIXTURE_NAME).expect("old load");
        let mut next = Release::empty();
        let mut entry = next.fixture_entry(FIXTURE_NAME);
        let bytes = next.fixture().wasm.clone();
        entry["capabilities"]
            .as_array_mut()
            .unwrap()
            .push("completion".into());
        next.add(entry, &bytes);
        let new = next.loader().load(FIXTURE_NAME).expect("new grants load");
        assert_eq!(old.digest(), new.digest());
        let old_sources = super::super::modules::VerifiedSources::default();
        let new_sources = super::super::modules::VerifiedSources::default();
        old_sources.record("fixture", &old);
        new_sources.record("fixture", &new);
        declare_package(&old);
        declare_package(&new);
        let old_caps = old_sources.resolve("fixture").unwrap().semantic;
        let new_caps = new_sources.resolve("fixture").unwrap().semantic;
        assert!(!old_caps.contains(SemanticCapability::ReportsCompletion));
        assert!(new_caps.contains(SemanticCapability::ReportsCompletion));
    }

    #[test]
    fn package_declarations_distinguish_two_digests_of_one_identity() {
        let identity = ToolIdentity {
            implementation: "p1/digest-fixture".into(),
            variant: "default".into(),
        };
        let mut declared = package_declarations().write().unwrap();
        declared.insert(
            (identity.clone(), "sha256:old".into()),
            Capabilities::of(&[SemanticCapability::RecordsCommandEvidence]),
        );
        declared.insert((identity.clone(), "sha256:new".into()), Capabilities::NONE);
        assert_ne!(
            declared[&(identity.clone(), "sha256:old".into())],
            declared[&(identity, "sha256:new".into())]
        );
    }

    #[test]
    fn two_sessions_binding_one_tool_object_share_no_state() {
        // The same tool object assembled into two sessions with different verified grants:
        // each session's copy answers from its own snapshot, and the object itself, which
        // nobody bound, answers from neither.
        let shared: Arc<dyn Tool> =
            Arc::new(FakeTool::new("shared").with_identity("p1/two-sessions-fixture", "default"));
        let evidence = Capabilities::of(&[SemanticCapability::RecordsCommandEvidence]);
        let first = bind(&shared, evidence);
        let second = bind(&shared, Capabilities::NONE);
        assert!(carries(
            first.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(!carries(
            second.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(!carries(
            shared.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
    }

    #[test]
    fn rebinding_replaces_the_snapshot_instead_of_stacking_wrappers() {
        let base: Arc<dyn Tool> =
            Arc::new(FakeTool::new("base").with_identity("p1/rebind-fixture", "default"));
        let evidence = Capabilities::of(&[SemanticCapability::RecordsCommandEvidence]);
        let once = bind(&base, evidence);
        let twice = bind(&once, Capabilities::NONE);
        assert!(carries(
            once.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(!carries(
            twice.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(Arc::ptr_eq(&unbound(&twice), &base));
    }

    #[test]
    fn an_old_tool_keeps_its_bound_capabilities_after_a_replacement() {
        let implementation = "p1/old-generation-fixture";
        let old: Arc<dyn Tool> =
            Arc::new(FakeTool::new("same").with_identity(implementation, "default"));
        let replacement: Arc<dyn Tool> =
            Arc::new(FakeTool::new("same").with_identity(implementation, "default"));
        let granted = Capabilities::of(&[SemanticCapability::RecordsCommandEvidence]);
        let old = bind(&old, granted);
        let replacement = bind(&replacement, Capabilities::NONE);
        assert!(carries(
            old.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(!carries(
            replacement.as_ref(),
            SemanticCapability::RecordsCommandEvidence
        ));
    }

    fn shell_in(dir: &std::path::Path) -> p1_tool_shell::ShellTool {
        p1_tool_shell::ShellTool::new(p1_workspace::Workspace::new(dir).expect("workspace"))
    }

    /// The native tool reads no record here: only its identity is checked.
    struct NoActivity;

    impl p1_tool_finish::SessionActivity for NoActivity {
        fn last_file_change(&self) -> Option<u64> {
            None
        }

        fn shell_runs(&self) -> Vec<p1_finish_guest::ShellRun> {
            Vec::new()
        }
    }

    fn finish() -> p1_tool_finish::FinishTool {
        p1_tool_finish::FinishTool::new(
            Arc::new(NoActivity),
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

    /// S3.8 (D083b): the shell's and `finish`'s capabilities come from their verified
    /// manifest grants through [`package_capabilities`], not from a native declaration,
    /// which left with its registration; nothing else carries either.
    #[test]
    fn the_shell_and_finish_packages_declare_their_grants() {
        let dir = tempfile::tempdir().unwrap();
        // The native adapters carry nothing on their own now.
        assert_eq!(
            declared(shell_in(dir.path()).identity()),
            Capabilities::NONE
        );
        assert_eq!(declared(finish().identity()), Capabilities::NONE);
        for (package, capability, variant) in [
            (
                "p1-module-shell",
                SemanticCapability::RecordsCommandEvidence,
                "claude+sandbox",
            ),
            (
                "p1-module-finish",
                SemanticCapability::ReportsCompletion,
                "claude",
            ),
        ] {
            let module = built_package(package);
            assert_eq!(
                declare_package(&module),
                Capabilities::of(&[capability]),
                "{package}"
            );
            // A host-applied presentation variant of the same verified package carries it.
            let presented = FakeTool::new("x").with_identity(module.name(), variant);
            assert!(carries(&presented, capability), "{package}");
        }
        let read = p1_tool_read::ReadTool::new(
            p1_workspace::Workspace::new(dir.path()).unwrap(),
            Default::default(),
        );
        assert_eq!(declared(read.identity()), Capabilities::NONE);
    }

    /// ADR-0109 item 8 (#511): reading a stored output is never a verification run. The
    /// `read_output` package is granted the store alone, so it derives no capability: its calls
    /// record no command evidence and cannot stand in for the run `finish` names.
    #[test]
    fn the_read_output_package_records_no_command_evidence() {
        let granted = vec!["tool-outputs".to_owned()];
        assert_eq!(derive(ModuleKind::Tool, &granted), Capabilities::NONE);
        let module = built_package("p1-module-read-output");
        assert_eq!(module.capabilities(), granted.as_slice());
        assert_eq!(declare_package(&module), Capabilities::NONE);
        let presented = FakeTool::new("read_output").with_identity(module.name(), "claude");
        assert!(!carries(
            &presented,
            SemanticCapability::RecordsCommandEvidence
        ));
        assert!(!crate::activity::records_command_evidence(&presented));
    }

    /// A face can neither grant nor hide a capability: the shell PACKAGE presented under
    /// another model-facing name and variant still records command evidence, and a tool
    /// merely NAMED `shell` (or `finish`) carries nothing.
    #[test]
    fn a_face_neither_grants_nor_hides_a_capability() {
        let dir = tempfile::tempdir().unwrap();
        let renamed = shell_in(dir.path()).with_face(ToolFace::new("run", "Runs things."), "gpt");
        assert_eq!(renamed.declaration().name, "run");
        // The native adapter is not declared any more, so the face names nothing.
        assert!(!carries(
            &renamed,
            SemanticCapability::RecordsCommandEvidence
        ));
        // The package's identity carries it whatever the model is told.
        let module = built_package("p1-module-shell");
        declare_package(&module);
        let presented = FakeTool::new("run").with_identity(module.name(), "gpt");
        assert!(carries(
            &presented,
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
    ///
    /// A declaration lives for the whole test process, and `catalog/modules.rs`'s
    /// registration cases declare the fixture under the name the build published
    /// (`p1/fixture`): this case loads the same verified bytes under a name of its own, so
    /// "nothing is declared yet" holds whatever order the cases run in.
    #[test]
    fn a_declared_package_reaches_carries_through_its_verified_identity() {
        let mut release = Release::empty();
        let mut entry = release.fixture_entry("p1/carrier");
        entry["variant"] = "carrier".into();
        let bytes = release.fixture().wasm.clone();
        release.add(entry, &bytes);
        let module = release
            .loader()
            .load("p1/carrier")
            .expect("the fixture bytes load");
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
