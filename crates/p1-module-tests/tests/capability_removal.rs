//! S6.8 (#334): runtime disablement of the worker and workflow families (ADR-0085 item 6).
//!
//! `[capabilities]` in `settings.toml` switches either family off for the next main-agent
//! assembly. The first section drives the whole host (`p1_host::run::run`) with a scripted
//! provider and a settings file in an injected `XDG_CONFIG_HOME`, so the flag is read exactly
//! where the host reads it and the assembly is the one the provider is asked with:
//! - a disabled family's tools and its `{{#tool:...}}` prompt sections are gone, the other
//!   family's are not, and re-enabling brings them back on the next assembly;
//! - an environment that names a disabled member (native key or package lock key) fails
//!   with "workers are disabled" / "workflows are disabled", and `p1 workflow run` is refused
//!   the same way: never a skip, never a native member in its place;
//! - the runtime-disabled twins of the feature-off cases in `crates/p1-host/tests/host.rs`
//!   (`worker_modules_are_unknown_without_delegation`, `plain_environment_works_without_delegation`).
//!
//! The second section is the running work, over the host's public assembly pieces
//! (`with_worker_tools`, `MemberScopes`, the module hook) and the built member packages,
//! loaded through the release harness from `scripts/build-modules.sh` output as
//! `delegation_activation.rs` loads them: a child and a workflow run started through one
//! generation's members keep running after the next assembly leaves their family out. They
//! complete, their results stay readable through the members of their own generation and the
//! service, and the parent is notified. That generation is retired only at teardown.
//!
//! Every ordering is explicit (a semaphore, a `Notify`); nothing sleeps or asserts on time,
//! and the running-work cases run under the harness's deadlock guard.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Arc, Mutex};

use p1_assembly::{
    Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolServices, ToolSpec,
    assemble_for_agent,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, ModelOptions, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome,
    ToolStatus,
};
use p1_core::{Agent, AgentParts};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_host::workflow::{
    Capabilities, MemberScopes, member_services, with_worker_tools, worker_member_services,
};
use p1_host::{HostDeps, InterruptSource, LineSource, SharedWriter};
use p1_module_runtime::Services;
use p1_module_tests::{Release, within_deadline};
use p1_provider_http::testing::ScriptedTransport;
use p1_redact::MaskCounter;
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, text_response,
};
use p1_workers::{
    AgentFactory, ChildAgent, ChildId, ChildSpec, ChildStatus, InProcessWorkers, WorkerError,
    WorkerReport, WorkerService, WorkersObserve,
};
use p1_workflow::{
    InProcessWorkflows, ModelResolver, ResolvedModel, RoleSpec, RunId, RunOutcome, RunReport,
    RunStatus, SchemaCheck, StepEnd, StepLine, StepOutcome, StepRequest, StepRunner, WorkerRef,
    WorkflowObserver, WorkflowService, WorkflowSettings,
};
use tokio::sync::{Notify, Semaphore};

const WORKER_TOOLS: [&str; 4] = [
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];
const WORKFLOW_TOOLS: [&str; 4] = [
    "workflow_start",
    "workflow_status",
    "workflow_result",
    "workflow_cancel",
];

/// A prompt with conditional sections per family (ADR-0050 item 4): each is rendered only
/// when its tool is assembled. A section names a catalog key, so the package path's lock keys
/// get sections of their own.
const PROMPT: &str = "Base prompt.\n\
{{#tool:worker_start}}WORKER SECTION: delegate with {{tool:worker_start}}.{{/tool:worker_start}}\n\
{{#tool:workflow_start}}WORKFLOW SECTION: orchestrate with {{tool:workflow_start}}.{{/tool:workflow_start}}\n\
{{#tool:worker-start}}WORKER SECTION: delegate with {{tool:worker-start}}.{{/tool:worker-start}}\n\
{{#tool:workflow-start}}WORKFLOW SECTION: orchestrate with {{tool:workflow-start}}.{{/tool:workflow-start}}\n";

// ================================================================ the whole host

/// A writer the test reads back.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl Capture {
    fn writer(&self) -> SharedWriter {
        Arc::new(Mutex::new(Box::new(self.clone())))
    }

    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl Write for Capture {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// No input: every run below is headless, so the line source is at EOF.
struct NoLines;

impl LineSource for NoLines {
    fn next_line<'a>(&'a self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async { None })
    }
}

/// No Ctrl-C ever arrives.
struct NoInterrupt;

impl InterruptSource for NoInterrupt {
    fn recv<'a>(&'a self) -> Pin<Box<dyn Future<Output = ()> + Send + 'a>> {
        Box::pin(std::future::pending())
    }
}

/// One host: its environments, its config and state directories (the only environment the
/// host sees), and a workspace.
struct Host {
    environments: tempfile::TempDir,
    config: tempfile::TempDir,
    state: tempfile::TempDir,
    workspace: tempfile::TempDir,
}

/// What one host invocation left behind.
struct Ran {
    code: i32,
    stdout: String,
    stderr: String,
    requests: Vec<ProviderRequest>,
}

impl Host {
    fn new() -> Self {
        let host = Self {
            environments: tempfile::tempdir().expect("environments"),
            config: tempfile::tempdir().expect("config"),
            state: tempfile::tempdir().expect("state"),
            workspace: tempfile::tempdir().expect("workspace"),
        };
        host.environment("deleg", &["read"]);
        host
    }

    /// `<environments>/<name>/` over the scripted `fake` provider with `PROMPT`.
    fn environment(&self, name: &str, tools: &[&str]) {
        let dir = self.environments.path().join(name);
        std::fs::create_dir_all(&dir).expect("environment dir");
        let mut toml =
            format!("family = \"{name}\"\nprovider = \"fake\"\nmodel = \"fake-model\"\n");
        for tool in tools {
            toml.push_str(&format!("[[tools]]\nmodule = \"{tool}\"\n"));
        }
        std::fs::write(dir.join("environment.toml"), toml).expect("environment.toml");
        std::fs::write(dir.join("prompt.md"), PROMPT).expect("prompt.md");
    }

    /// Write (or rewrite) `settings.toml` where the host reads it.
    fn settings(&self, text: &str) {
        let dir = self.config.path().join("p1");
        std::fs::create_dir_all(&dir).expect("settings dir");
        std::fs::write(dir.join("settings.toml"), text).expect("settings.toml");
    }

    /// Run the host once with `args`, a fresh scripted provider answering one text turn.
    async fn run(&self, args: &[&str]) -> Ran {
        let stdout = Capture::default();
        let stderr = Capture::default();
        let mut deps = HostDeps::new(
            stdout.writer(),
            stderr.writer(),
            Arc::new(NoLines),
            Arc::new(ScriptedTransport::new(Vec::new())),
            "2026-01-02".to_owned(),
            Arc::new(NoInterrupt),
            vec![self.environments.path().to_path_buf()],
            false,
        );
        deps.home = None;
        deps.shell_env = Some(vec![
            ("XDG_CONFIG_HOME".into(), self.config.path().into()),
            ("XDG_STATE_HOME".into(), self.state.path().into()),
        ]);
        let provider = ScriptedProvider::new(vec![text_response("ok")]);
        let registered = provider.clone();
        deps.catalog_hook = Some(Box::new(move |catalog: &mut Catalog| {
            let provider = registered.clone();
            catalog.provider(
                "fake",
                Box::new(move |_spec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
            );
        }));
        let args: Vec<String> = args.iter().map(|arg| (*arg).to_owned()).collect();
        let options = p1_host::cli::parse(&args).expect("test args parse");
        let code = p1_host::run::run(&mut deps, options).await;
        Ran {
            code,
            stdout: stdout.text(),
            stderr: stderr.text(),
            requests: provider.requests(),
        }
    }

    /// A headless one-turn run of `environment`.
    async fn turn(&self, environment: &str) -> Ran {
        let workspace = self.workspace.path().to_str().expect("utf-8").to_owned();
        self.run(&["--env", environment, "--workspace", &workspace, "go"])
            .await
    }
}

impl Ran {
    /// The one request the main agent sent: the assembly as the provider saw it.
    fn request(&self) -> &ProviderRequest {
        assert_eq!(self.code, 0, "stderr: {}", self.stderr);
        assert_eq!(self.requests.len(), 1, "one turn");
        &self.requests[0]
    }

    fn tools(&self) -> Vec<String> {
        self.request()
            .tools
            .iter()
            .map(|tool| tool.name.clone())
            .collect()
    }

    fn prompt(&self) -> &str {
        &self.request().system_prompt
    }
}

fn expected(workers: bool, workflows: bool) -> Vec<String> {
    let mut tools = vec!["read"];
    if workers {
        tools.extend(WORKER_TOOLS);
    }
    if workflows {
        tools.extend(WORKFLOW_TOOLS);
    }
    tools.into_iter().map(str::to_owned).collect()
}

/// The shipped default is today's behaviour: no `settings.toml`, or one without the table,
/// gives every main agent both families and both prompt sections.
#[tokio::test]
async fn enabled_by_default_the_main_agent_gets_both_families() {
    let host = Host::new();
    let ran = host.turn("deleg").await;
    assert_eq!(ran.tools(), expected(true, true));
    assert!(ran.prompt().contains("WORKER SECTION"), "{}", ran.prompt());
    assert!(
        ran.prompt().contains("WORKFLOW SECTION"),
        "{}",
        ran.prompt()
    );

    host.settings("[capabilities]\n");
    let ran = host.turn("deleg").await;
    assert_eq!(ran.tools(), expected(true, true));
}

/// Workers disabled: the next main-agent assembly has no `worker_*` tool and no worker
/// prompt text; the workflow family is untouched.
#[tokio::test]
async fn workers_disabled_removes_the_worker_tools_and_their_prompt_section() {
    let host = Host::new();
    host.settings("[capabilities]\nworkers = false\n");
    let ran = host.turn("deleg").await;
    assert_eq!(ran.tools(), expected(false, true));
    assert!(!ran.prompt().contains("WORKER SECTION"), "{}", ran.prompt());
    assert!(!ran.prompt().contains("worker_"), "{}", ran.prompt());
    assert!(
        ran.prompt().contains("WORKFLOW SECTION"),
        "{}",
        ran.prompt()
    );
}

/// Workflows disabled: the same for the `workflow_*` tools and their text.
#[tokio::test]
async fn workflows_disabled_removes_the_workflow_tools_and_their_prompt_section() {
    let host = Host::new();
    host.settings("[capabilities]\nworkflows = false\n");
    let ran = host.turn("deleg").await;
    assert_eq!(ran.tools(), expected(true, false));
    assert!(
        !ran.prompt().contains("WORKFLOW SECTION"),
        "{}",
        ran.prompt()
    );
    assert!(!ran.prompt().contains("workflow_"), "{}", ran.prompt());
    assert!(ran.prompt().contains("WORKER SECTION"), "{}", ran.prompt());
}

/// `p1 env show` assembles as a run's start does: the disabled family is not in the
/// resolved environment it prints.
#[tokio::test]
async fn env_show_leaves_the_disabled_family_out() {
    let host = Host::new();
    host.settings("[capabilities]\nworkers = false\nworkflows = false\n");
    let ran = host.run(&["env", "show", "deleg"]).await;
    assert_eq!(ran.code, 0, "stderr: {}", ran.stderr);
    let json = &ran.stdout[ran.stdout.find('{').expect("the resolved JSON")..];
    let resolved: Value = serde_json::from_str(json).expect("JSON");
    let modules: Vec<&str> = resolved["tools"]
        .as_array()
        .expect("tools")
        .iter()
        .map(|tool| tool["module"].as_str().expect("module"))
        .collect();
    assert_eq!(modules, ["read"]);
    let prompt = resolved["system_prompt"].as_str().expect("prompt");
    assert!(!prompt.contains("SECTION"), "{prompt}");
}

/// An environment that names a member of a disabled family fails assembly with the family's
/// explicit error, whether it names the native key or the package's lock key: it is never
/// skipped, and never served by the native member. No provider request is made. This is the
/// runtime-disabled twin of `worker_modules_are_unknown_without_delegation`.
#[tokio::test]
async fn an_environment_naming_a_disabled_member_fails_with_the_explicit_error() {
    let host = Host::new();
    host.settings("[capabilities]\nworkers = false\nworkflows = false\n");
    for (environment, member, family) in [
        ("names-worker", "worker_start", "workers"),
        ("names-worker-package", "worker-result", "workers"),
        ("names-workflow", "workflow_status", "workflows"),
        ("names-workflow-package", "workflow-cancel", "workflows"),
    ] {
        host.environment(environment, &["read", member]);
        let ran = host.turn(environment).await;
        assert_eq!(ran.code, 1, "{environment}: stderr {}", ran.stderr);
        assert!(
            ran.stderr.contains(&format!("{family} are disabled")),
            "{environment}: {}",
            ran.stderr
        );
        assert!(
            ran.stderr.contains(&format!("`{member}`")),
            "{environment}: {}",
            ran.stderr
        );
        assert!(
            !ran.stderr.contains("unknown tool module"),
            "{environment}: {}",
            ran.stderr
        );
        assert!(ran.requests.is_empty(), "{environment}: nothing was asked");
    }
}

/// `p1 workflow run` is the workflow capability too: disabled, it is refused explicitly
/// before any script runs.
#[tokio::test]
async fn workflow_run_is_refused_when_workflows_are_disabled() {
    let host = Host::new();
    host.settings("[capabilities]\nworkflows = false\n");
    let script = host.workspace.path().join("one.rhai");
    std::fs::write(&script, "agent(\"never\")").expect("script");
    let workspace = host.workspace.path().to_str().expect("utf-8");
    let ran = host
        .run(&[
            "workflow",
            "run",
            script.to_str().expect("utf-8"),
            "--workspace",
            workspace,
        ])
        .await;
    assert_eq!(ran.code, 1, "stderr: {}", ran.stderr);
    assert!(
        ran.stderr.contains("workflows are disabled"),
        "{}",
        ran.stderr
    );
    assert!(ran.requests.is_empty());
}

/// The runtime-disabled twin of `plain_environment_works_without_delegation`: with both
/// families disabled an environment that names neither runs as before.
#[tokio::test]
async fn a_plain_environment_works_with_both_families_disabled() {
    let host = Host::new();
    host.environment("plain", &["read"]);
    host.settings("[capabilities]\nworkers = false\nworkflows = false\n");
    let ran = host.turn("plain").await;
    assert_eq!(ran.tools(), ["read"]);
    assert!(ran.stdout.contains("ok"), "{}", ran.stdout);
}

/// Re-enabling restores both families and their sections on the next assembly.
#[tokio::test]
async fn re_enabling_restores_the_tools_on_the_next_assembly() {
    let host = Host::new();
    host.settings("[capabilities]\nworkers = false\nworkflows = false\n");
    let ran = host.turn("deleg").await;
    assert_eq!(ran.tools(), expected(false, false));
    assert!(!ran.prompt().contains("SECTION"), "{}", ran.prompt());

    host.settings("[capabilities]\nworkers = true\nworkflows = true\n");
    let ran = host.turn("deleg").await;
    assert_eq!(ran.tools(), expected(true, true));
    assert!(ran.prompt().contains("WORKER SECTION"), "{}", ran.prompt());
    assert!(
        ran.prompt().contains("WORKFLOW SECTION"),
        "{}",
        ran.prompt()
    );
}

/// A malformed table is the settings file's usage error, never a silent default.
#[tokio::test]
async fn an_unknown_capability_key_is_refused() {
    let host = Host::new();
    host.settings("[capabilities]\ndelegation = false\n");
    let ran = host.turn("deleg").await;
    assert_eq!(ran.code, 2, "stderr: {}", ran.stderr);
    assert!(ran.stderr.contains("delegation"), "{}", ran.stderr);
    assert!(ran.requests.is_empty());
}

// ================================================================ running work

/// The member packages the running-work cases load: directory and lock key.
const MEMBERS: [(&str, &str); 4] = [
    ("p1-module-worker-start", "worker-start"),
    ("p1-module-worker-result", "worker-result"),
    ("p1-module-workflow-start", "workflow-start"),
    ("p1-module-workflow-result", "workflow-result"),
];

/// Where `scripts/build-modules.sh` publishes the packages.
fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

fn package_manifest(package: &str) -> Value {
    let path = built()
        .join(package)
        .join(format!("{package}.manifest.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the build output {} is missing ({error}): run scripts/build-modules.sh --all first",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("JSON")
}

/// A release of the members, laid out as p1's release ships them.
fn release() -> Release {
    let mut release = Release::empty();
    for (package, _) in MEMBERS {
        let manifest = package_manifest(package);
        let bytes = std::fs::read(built().join(package).join(format!("{package}.wasm")))
            .expect("the built component");
        release.add(
            json!({
                "name": manifest["name"],
                "digest": manifest["digest"],
                "path": format!("packages/{package}/{package}.wasm"),
                "kind": manifest["kind"],
                "world": manifest["world"],
                "protocol": manifest["protocol"],
                "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            }),
            &bytes,
        );
    }
    release
}

/// The `modules.lock` selecting the release's members under their documented keys.
fn lock(release: &Release) -> ModulesLock {
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(release.manifest_file()).expect("release manifest"),
    )
    .expect("JSON");
    let mut text = String::from("format = \"p1-modules-lock/1\"\n");
    for entry in manifest["components"].as_array().expect("components") {
        let name = entry["name"].as_str().expect("name");
        let key = name.strip_prefix("p1/").expect("a p1 module");
        text.push_str(&format!(
            "\n[modules.{key}]\npackage = \"{name}\"\nversion = \"0.0.1\"\n\
             digest = \"{}\"\nworld = \"{}\"\nprotocol = \"{}\"\n",
            entry["digest"].as_str().expect("digest"),
            entry["world"].as_str().expect("world"),
            entry["protocol"].as_str().expect("protocol"),
        ));
    }
    ModulesLock::parse(&release.root().join("modules.lock"), &text).expect("lock")
}

/// One main agent's catalog: a scripted provider, `read`, stand-ins under the eight native
/// member keys (so an enabled family's native path assembles), and the member packages
/// registered through the host's entry point with `hook`.
struct Generation {
    catalog: Catalog,
    _release: Release,
}

fn generation(hook: ModuleServices) -> Generation {
    let release = release();
    let mut catalog = Catalog::new();
    let provider = ScriptedProvider::new(Vec::new());
    catalog.provider(
        "scripted",
        Box::new(move |_spec: &ProviderSpec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
    );
    for key in ["read"]
        .into_iter()
        .chain(WORKER_TOOLS)
        .chain(WORKFLOW_TOOLS)
    {
        catalog.tool(
            key,
            Box::new(move |_spec: &ToolSpec, _services: &ToolServices| {
                Ok(Arc::new(FakeTool::new(key)) as Arc<dyn Tool>)
            }),
        );
    }
    let packages = load_locked_modules(&lock(&release), &release.manifest_file())
        .expect("the members load through the host");
    register_modules(&mut catalog, packages, hook).expect("registration");
    Generation {
        catalog,
        _release: release,
    }
}

fn environment(modules: &[&str]) -> EnvironmentFile {
    EnvironmentFile {
        name: "capability-removal".into(),
        family: "test".into(),
        provider: "scripted".into(),
        model: "test-model".into(),
        profile: None,
        options: ModelOptions::default(),
        tools: modules
            .iter()
            .map(|module| ToolSpec {
                module: (*module).into(),
                name: None,
                description: None,
                variant: None,
            })
            .collect(),
        prompt_template: PROMPT.into(),
        context: None,
        summarize_prompt: None,
    }
}

/// The main agent's assembly as the host makes it: `with_worker_tools` under
/// `capabilities`, then the catalog, for the main agent `0`.
fn assemble_main(
    generation: &Generation,
    modules: &[&str],
    capabilities: Capabilities,
) -> Result<(Vec<Arc<dyn Tool>>, String), String> {
    let mut environment = environment(modules);
    with_worker_tools(&mut environment, capabilities)?;
    let workspace = std::env::temp_dir();
    let assembled = assemble_for_agent(
        &generation.catalog,
        &environment,
        &workspace,
        &Substitutions {
            workspace: "/work".into(),
            date: "2026-01-01".into(),
            os: "linux".into(),
        },
        &Arc::new(MaskCounter::new()),
        Some("0"),
        |_| ModelOptions::default(),
    )
    .map_err(|error| error.to_string())?;
    Ok((assembled.tools, assembled.system_prompt))
}

/// The refusal of an assembly that must fail.
fn refused(assembled: Result<(Vec<Arc<dyn Tool>>, String), String>) -> String {
    match assembled {
        Err(error) => error,
        Ok((tools, _)) => panic!("assembled {:?}", names(&tools)),
    }
}

fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect()
}

fn named<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.declaration().name == name)
        .unwrap_or_else(|| panic!("{name} is not assembled"))
}

async fn call(tool: &Arc<dyn Tool>, name: &str, input: Value) -> ToolOutcome {
    tool.execute(
        &ToolCall {
            call_id: "c1".to_owned(),
            name: name.to_owned(),
            input: ToolInput::Json(input.to_string()),
        },
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

fn enabled() -> Capabilities {
    Capabilities::default()
}

fn only(workers: bool, workflows: bool) -> Capabilities {
    Capabilities { workers, workflows }
}

/// A hook that links no member: what the host installs for a family it disabled.
fn no_members() -> ModuleServices {
    Arc::new(|_module: &str, _services: &ToolServices| Services::default())
}

/// A provider whose every stream waits for one permit of `gate`, so a child stays running
/// exactly until the test lets it finish.
struct GatedProvider {
    gate: Arc<Semaphore>,
    inner: ScriptedProvider,
}

impl Provider for GatedProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            self.gate
                .acquire()
                .await
                .expect("the test never closes the gate")
                .forget();
            self.inner.stream(request, cancel).await
        })
    }
}

fn test_agent(provider: Arc<dyn Provider>) -> Agent {
    Agent::new(AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: "agent".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    })
    .expect("the test agent builds")
}

/// A child started through one generation's `worker_start` member keeps running when the
/// next assembly disables workers: that assembly has no worker tool and refuses one by name,
/// while the child completes, its result is read through the old generation's
/// `worker_result`, and the parent is notified. Only teardown retires that generation.
#[tokio::test]
async fn a_child_started_before_disabling_completes_is_readable_and_notifies() {
    within_deadline("child survives disabling", async {
        let gate = Arc::new(Semaphore::new(0));
        let factory: AgentFactory = {
            let gate = gate.clone();
            Arc::new(move |_spec: &ChildSpec| {
                Ok(ChildAgent {
                    agent: test_agent(Arc::new(GatedProvider {
                        gate: gate.clone(),
                        inner: ScriptedProvider::new(vec![text_response("child done")]),
                    })),
                    description: "fake/route".into(),
                    report: Arc::new(WorkerReport::default),
                    regrant: None,
                })
            })
        };
        let service = InProcessWorkers::new(factory, 8);
        let parent = test_agent(Arc::new(ScriptedProvider::new(Vec::new())));
        service.set_parent_inbox(parent.inbox());

        // Generation A: workers enabled, the members from their packages.
        let first = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        let old = generation(worker_member_services(first.clone(), None));
        let members = ["worker-start", "worker-result"];
        let (old_tools, old_prompt) =
            assemble_main(&old, &members, only(true, false)).expect("generation A");
        assert_eq!(names(&old_tools), ["worker_start", "worker_result"]);
        assert!(old_prompt.contains("WORKER SECTION"), "{old_prompt}");
        let outcome = call(
            named(&old_tools, "worker_start"),
            "worker_start",
            json!({"environment": "child", "task": "do it", "tools": ["read"]}),
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        let child = ChildId("w1".to_owned());

        // Workers disabled: the next assembly is a new generation with no worker hook.
        let second = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        assert_ne!(second.generation(), first.generation());
        let next = generation(no_members());
        let (tools, prompt) =
            assemble_main(&next, &["read"], only(false, false)).expect("generation B");
        assert_eq!(names(&tools), ["read"]);
        assert!(!prompt.contains("WORKER SECTION"), "{prompt}");
        let error = refused(assemble_main(&next, &members, only(false, false)));
        assert!(error.starts_with("workers are disabled"), "{error}");

        // The child was not cancelled, and its generation still reaches it.
        assert_eq!(service.status(&child).await, Ok(ChildStatus::Running));
        assert_eq!(
            first.workers("0").status(&child).await,
            Ok(ChildStatus::Running)
        );
        assert_eq!(
            second.workers("0").status(&child).await,
            Err(WorkerError::UnknownChild),
            "the new generation never had it"
        );

        gate.add_permits(1);
        match service.wait(&child, CancellationToken::new()).await {
            Ok(ChildStatus::Finished(result)) => assert_eq!(result.final_text, "child done"),
            other => panic!("{other:?}"),
        }
        let outcome = call(
            named(&old_tools, "worker_result"),
            "worker_result",
            json!({"id": "w1"}),
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert!(
            outcome.content.contains("Worker w1: finished")
                && outcome.content.contains("child done"),
            "{}",
            outcome.content
        );
        assert!(parent.has_pending_inbox(), "the parent hears of it");

        // Teardown of generation A is what retires it.
        first.registry().retire_generation(first.generation()).await;
        assert_eq!(
            first.workers("0").status(&child).await,
            Err(WorkerError::UnknownChild)
        );
        assert!(matches!(
            service.status(&child).await,
            Ok(ChildStatus::Finished(_))
        ));
    })
    .await;
}

/// One step that parks until the test releases it.
struct ParkedRunner {
    reached: Notify,
    release: Notify,
}

impl StepRunner for ParkedRunner {
    fn run<'a>(
        &'a self,
        request: &'a StepRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepOutcome, String>> {
        Box::pin(async move {
            self.reached.notify_one();
            self.release.notified().await;
            Ok(StepOutcome {
                worker: WorkerRef {
                    id: "w1".to_owned(),
                    description: request.model.reference.clone(),
                },
                end: StepEnd::Done {
                    summary: format!("did: {}", request.prompt),
                    evidence: "commands passed: fake".to_owned(),
                    result: None,
                    schema: SchemaCheck::NotRequested,
                },
            })
        })
    }

    fn repair<'a>(
        &'a self,
        _worker: &'a WorkerRef,
        _message: String,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepEnd, String>> {
        Box::pin(async { Err("no repair in this case".to_owned()) })
    }
}

/// `env/<profile>`; the wire model is the profile.
struct Resolver;

impl ModelResolver for Resolver {
    fn resolve(&self, reference: &str) -> Result<ResolvedModel, String> {
        let (environment, profile) = reference
            .split_once('/')
            .ok_or_else(|| format!("not environment/profile: {reference}"))?;
        Ok(ResolvedModel {
            reference: reference.to_owned(),
            environment: environment.to_owned(),
            profile: profile.to_owned(),
            effort: None,
            wire_model: profile.to_owned(),
        })
    }
}

/// The parent's side of a run: its one end notification, recorded.
#[derive(Default)]
struct Notified {
    ends: Mutex<Vec<RunReport>>,
    ended: Notify,
}

impl WorkflowObserver for Notified {
    fn phase(&self, _id: &RunId, _name: &str) {}
    fn log(&self, _id: &RunId, _text: &str) {}
    fn step_started(&self, _id: &RunId, _request: &StepRequest, _worker: &WorkerRef) {}
    fn step_ended(&self, _id: &RunId, _line: &StepLine) {}
    fn thunk_failed(&self, _id: &RunId, _error: &str) {}
    fn jobs_queued(&self, _id: &RunId, _count: usize) {}
    fn run_ended(&self, _id: &RunId, report: &RunReport) {
        self.ends.lock().unwrap().push(report.clone());
        self.ended.notify_one();
    }
}

fn workflow_settings() -> WorkflowSettings {
    WorkflowSettings {
        roles: [(
            "worker".to_owned(),
            RoleSpec {
                model: "env/head".to_owned(),
                fallback: Vec::new(),
                tools: vec!["read".to_owned()],
            },
        )]
        .into(),
        caps: Default::default(),
        max_steps: 10,
        max_threads: 2,
    }
}

/// A workflow run started through one generation's `workflow_start` member runs to its end
/// after the next assembly disables workflows: that assembly has no workflow tool and refuses
/// one by name, while the run's step is let go, the run ends, the parent's notification
/// comes once, and the old generation's `workflow_result` reads the report.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_workflow_run_started_before_disabling_runs_to_its_end() {
    within_deadline("run survives disabling", async {
        let root = tempfile::tempdir().expect("run root");
        let runner = Arc::new(ParkedRunner {
            reached: Notify::new(),
            release: Notify::new(),
        });
        let observer = Arc::new(Notified::default());
        let service = InProcessWorkflows::new(
            runner.clone(),
            Arc::new(Resolver),
            observer.clone(),
            workflow_settings(),
            root.path().to_path_buf(),
        );
        let runs: Arc<dyn WorkflowService> = service.clone();

        // Generation A: workflows enabled, the members from their packages.
        let old = generation(member_services(None, runs.clone()));
        let members = ["workflow-start", "workflow-result"];
        let (old_tools, old_prompt) =
            assemble_main(&old, &members, only(false, true)).expect("generation A");
        assert_eq!(names(&old_tools), ["workflow_start", "workflow_result"]);
        assert!(old_prompt.contains("WORKFLOW SECTION"), "{old_prompt}");
        let outcome = call(
            named(&old_tools, "workflow_start"),
            "workflow_start",
            json!({"script": "agent(\"the step\").value", "args": {}}),
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        let id = outcome
            .content
            .strip_prefix("Started workflow ")
            .and_then(|rest| rest.split(['.', ' ']).next())
            .expect("the run id")
            .to_owned();
        runner.reached.notified().await;

        // Workflows disabled: the next assembly has no workflow tool, and naming one fails.
        let next = generation(no_members());
        let (tools, prompt) =
            assemble_main(&next, &["read"], only(false, false)).expect("generation B");
        assert_eq!(names(&tools), ["read"]);
        assert!(!prompt.contains("WORKFLOW SECTION"), "{prompt}");
        let error = refused(assemble_main(&next, &members, only(false, false)));
        assert!(error.starts_with("workflows are disabled"), "{error}");
        assert!(
            matches!(
                runs.status(&RunId(id.clone())).await,
                Ok(RunStatus::Running(_))
            ),
            "disabling cancelled nothing"
        );

        runner.release.notify_one();
        observer.ended.notified().await;
        match runs
            .wait(&RunId(id.clone()), CancellationToken::new())
            .await
        {
            Ok(RunStatus::Ended(report)) => {
                assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}")
            }
            other => panic!("{other:?}"),
        }
        assert_eq!(observer.ends.lock().unwrap().len(), 1, "one notification");
        let outcome = call(
            named(&old_tools, "workflow_result"),
            "workflow_result",
            json!({"id": id}),
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert!(
            outcome
                .content
                .starts_with(&format!("Workflow {id}: completed")),
            "{}",
            outcome.content
        );
        service.shutdown().await;
    })
    .await;
}

/// Re-enabling restores the tools on the next assembly on both paths: the native members
/// when the environment names no package, the packages when it names them.
#[tokio::test]
async fn re_enabling_restores_both_paths_on_the_next_assembly() {
    within_deadline("re-enable", async {
        let generation = generation(no_members());
        let (tools, _) =
            assemble_main(&generation, &["read"], only(false, false)).expect("disabled");
        assert_eq!(names(&tools), ["read"]);
        let (tools, prompt) = assemble_main(&generation, &["read"], enabled()).expect("enabled");
        assert_eq!(names(&tools), expected(true, true));
        assert!(prompt.contains("WORKER SECTION"), "{prompt}");
        assert!(prompt.contains("WORKFLOW SECTION"), "{prompt}");
        assert!(
            assemble_main(&generation, &["worker-result"], only(false, true)).is_err(),
            "disabled, the package is refused"
        );
    })
    .await;
}
