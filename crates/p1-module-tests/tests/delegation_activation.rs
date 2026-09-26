//! S6.7 (#313): the eight worker and workflow members, as built packages, run through the
//! runtime's worker and workflow link (`p1_module_runtime::delegation`, B-S6-8, D065) and
//! through the host's assembly (B-S6-9, D068).
//!
//! The runtime cases (S6.7.1) load the built packages through the runtime `Loader`, build a
//! `WasmTool` with a filled `Services` and call it, with no host. The host cases (S6.7.2, the
//! section at the end) select the members by `modules.lock` key and assemble them for a main
//! agent through the host's catalog entry point and module hook. The
//! worker members run over a real `p1_workers::WorkerScope` on a fake `WorkerService` that
//! records what reached it, and the workflow members over a fake run service. Every ordering is
//! explicit (a `Notify`, a cancel token); nothing sleeps or asserts on time, and each case runs
//! under the harness's deadlock guard.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_assembly::{
    AssemblyError, Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions,
    ToolServices, ToolSpec, assemble_for_agent,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, ModelOptions, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription, StopReason, Tool, ToolCall, ToolContext, ToolInput,
    ToolOutcome, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_host::workflow::{
    MemberScopes, WORKER_MODULES, WORKFLOW_MODULES, member_services, worker_member_services,
};
use p1_module_runtime::delegation::{WorkerServices, WorkflowServices};
use p1_module_runtime::{
    ExecutionLimits, LinkError, LoadedModule, Loader, ReleaseManifest, Services, ToolError,
    wasm_tool,
};
use p1_module_tests::{Release, within_deadline};
use p1_redact::MaskCounter;
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
    ScriptedProvider, text_response,
};
use p1_workers::{
    AgentFactory, ChildAgent, ChildId, ChildResult, ChildSpec, ChildStatus, FinishReport,
    InProcessWorkers, ScopeKey, WorkerError, WorkerReport, WorkerScope, WorkerScopes,
    WorkerService, WorkersObserve, WorkersStart,
};
use p1_workflow::{
    CancelRuns, Counts, ObserveRuns, RunId, RunOutcome, RunProgress, RunReport, RunStatus,
    StartRequest, StartRuns, WorkflowError, WorkflowService,
};
use tokio::sync::{Notify, Semaphore};

/// The eight members: package directory, manifest name, model-facing tool name.
const MEMBERS: [(&str, &str, &str); 8] = [
    ("p1-module-worker-start", "p1/worker-start", "worker_start"),
    (
        "p1-module-worker-result",
        "p1/worker-result",
        "worker_result",
    ),
    (
        "p1-module-worker-continue",
        "p1/worker-continue",
        "worker_continue",
    ),
    (
        "p1-module-worker-cancel",
        "p1/worker-cancel",
        "worker_cancel",
    ),
    (
        "p1-module-workflow-start",
        "p1/workflow-start",
        "workflow_start",
    ),
    (
        "p1-module-workflow-status",
        "p1/workflow-status",
        "workflow_status",
    ),
    (
        "p1-module-workflow-result",
        "p1/workflow-result",
        "workflow_result",
    ),
    (
        "p1-module-workflow-cancel",
        "p1/workflow-cancel",
        "workflow_cancel",
    ),
];

/// Where `scripts/build-modules.sh` publishes the packages.
fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

fn package_file(package: &str, suffix: &str) -> String {
    let path = built().join(package).join(format!("{package}{suffix}"));
    std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the build output {} is missing ({error}): run scripts/build-modules.sh --all first",
            path.display()
        )
    })
}

/// A loader over a release manifest listing the eight members exactly as their packages were
/// built: name, digest, class, world, protocol and grants from each package manifest.
fn loader() -> Loader {
    let components: Vec<Value> = MEMBERS
        .iter()
        .map(|(package, _, _)| {
            let manifest: Value =
                serde_json::from_str(&package_file(package, ".manifest.json")).expect("JSON");
            json!({
                "name": manifest["name"],
                "digest": manifest["digest"],
                "path": format!("{package}/{package}.wasm"),
                "kind": manifest["kind"],
                "world": manifest["world"],
                "protocol": manifest["protocol"],
                "capabilities": manifest["capabilities"],
                "variant": manifest["variant"],
            })
        })
        .collect();
    let release = json!({ "format": "p1-release-manifest/1", "components": components });
    let release = ReleaseManifest::parse(&release.to_string()).expect("release manifest");
    Loader::new(release, built()).expect("loader")
}

fn load(loader: &Loader, name: &str) -> LoadedModule {
    loader
        .load(name)
        .unwrap_or_else(|error| panic!("load {name}: {error}"))
}

fn tool(module: &LoadedModule, services: Services) -> Arc<dyn Tool> {
    match wasm_tool(
        module,
        services,
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    ) {
        Ok(tool) => tool,
        Err(error) => panic!("{}: {error}", module.name()),
    }
}

fn link_error(module: &LoadedModule, services: Services) -> LinkError {
    match wasm_tool(
        module,
        services,
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    ) {
        Err(ToolError::Link { source, .. }) => source,
        Err(other) => panic!("{}: wrong refusal {other}", module.name()),
        Ok(_) => panic!("{} must not link", module.name()),
    }
}

fn call(name: &str, input: Value) -> ToolCall {
    ToolCall {
        call_id: "c1".to_owned(),
        name: name.to_owned(),
        input: ToolInput::Json(input.to_string()),
    }
}

async fn run(tool: &Arc<dyn Tool>, name: &str, input: Value) -> ToolOutcome {
    tool.execute(
        &call(name, input),
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

fn workers(scope: &WorkerScope) -> Services {
    Services {
        workers: Some(WorkerServices::scoped(scope.clone())),
        ..Services::default()
    }
}

fn workflows(services: WorkflowServices) -> Services {
    Services {
        workflows: Some(services),
        ..Services::default()
    }
}

fn key(parent: &str) -> ScopeKey {
    ScopeKey {
        generation: 1,
        operation: "delegate".to_owned(),
        parent: parent.to_owned(),
    }
}

// ---------------------------------------------------------------- fake services

/// A worker service with no agents: children are ids with a status the test sets, and every
/// call that reaches it is recorded, so a case sees exactly what passed the scope.
#[derive(Default)]
struct FakeWorkers {
    children: Mutex<Vec<(ChildId, ChildStatus)>>,
    calls: Mutex<Vec<String>>,
    /// Signalled when a `wait` on a running child has begun; that wait then never ends by
    /// itself and ignores its token, so only the runtime can answer it.
    waiting: Notify,
}

impl FakeWorkers {
    fn known(&self, id: &ChildId) -> Result<ChildStatus, WorkerError> {
        self.children
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, status)| status.clone())
            .ok_or(WorkerError::UnknownChild)
    }

    fn set(&self, id: &str, status: ChildStatus) {
        let mut children = self.children.lock().unwrap();
        let entry = children
            .iter_mut()
            .find(|(known, _)| known.0 == id)
            .expect("a started child");
        entry.1 = status;
    }

    fn record(&self, call: String) {
        self.calls.lock().unwrap().push(call);
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }
}

impl WorkerService for FakeWorkers {
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        Box::pin(async move {
            self.record(format!(
                "start {} {} [{}] {:?}",
                spec.environment,
                spec.task,
                spec.tools.join(","),
                spec.workspace
            ));
            let mut children = self.children.lock().unwrap();
            let id = ChildId(format!("w{}", children.len() + 1));
            children.push((id.clone(), ChildStatus::Running));
            Ok(id)
        })
    }

    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move { self.known(id) })
    }

    fn wait<'a>(
        &'a self,
        id: &'a ChildId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move {
            let status = self.known(id)?;
            if status != ChildStatus::Running {
                return Ok(status);
            }
            self.waiting.notify_one();
            pending().await
        })
    }

    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            self.known(id)?;
            self.record(format!("cancel {}", id.0));
            Ok(())
        })
    }

    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            self.known(id)?;
            self.record(format!(
                "continue {} {message} [{}]",
                id.0,
                add_tools.join(",")
            ));
            Ok(())
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        Box::pin(async move { self.children.lock().unwrap().clone() })
    }

    fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        Box::pin(async move { self.known(id).map(|_| "fake-route/fake-model".to_owned()) })
    }
}

fn scopes() -> (Arc<FakeWorkers>, WorkerScopes) {
    let service = Arc::new(FakeWorkers::default());
    let scopes = WorkerScopes::new(service.clone() as Arc<dyn WorkerService>);
    (service, scopes)
}

/// A run service with no engine: runs are ids with a status the test sets, and every start
/// and cancel that reaches it is recorded.
#[derive(Default)]
struct FakeRuns {
    runs: Mutex<Vec<(RunId, RunStatus)>>,
    starts: Mutex<Vec<StartRequest>>,
    cancels: Mutex<Vec<RunId>>,
}

impl FakeRuns {
    fn known(&self, id: &RunId) -> Result<RunStatus, WorkflowError> {
        self.runs
            .lock()
            .unwrap()
            .iter()
            .find(|(known, _)| known == id)
            .map(|(_, status)| status.clone())
            .ok_or(WorkflowError::UnknownRun)
    }

    fn set(&self, id: &str, status: RunStatus) {
        let mut runs = self.runs.lock().unwrap();
        let entry = runs
            .iter_mut()
            .find(|(known, _)| known.0 == id)
            .expect("a started run");
        entry.1 = status;
    }
}

fn progress() -> RunStatus {
    RunStatus::Running(RunProgress {
        phase: Some("build".to_owned()),
        steps_started: 1,
        steps_ended: 0,
        replayed: 0,
        log: vec!["compiling".to_owned()],
    })
}

fn ended(id: &str) -> RunStatus {
    RunStatus::Ended(RunReport {
        id: RunId(id.to_owned()),
        outcome: RunOutcome::Completed,
        value: json!({"answer": 42}),
        counts: Counts {
            steps: 1,
            done: 1,
            ..Counts::default()
        },
        steps: Vec::new(),
        error: None,
        run_dir: PathBuf::from("/runs/wf1"),
    })
}

impl StartRuns for FakeRuns {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move {
            self.starts.lock().unwrap().push(request);
            let mut runs = self.runs.lock().unwrap();
            let id = RunId(format!("wf{}", runs.len() + 1));
            runs.push((id.clone(), progress()));
            Ok(id)
        })
    }
}

impl ObserveRuns for FakeRuns {
    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move { self.known(id) })
    }

    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move { self.known(id) })
    }
}

impl CancelRuns for FakeRuns {
    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            self.known(id)?;
            self.cancels.lock().unwrap().push(id.clone());
            Ok(())
        })
    }
}

/// The per-member adapter a workflow member is linked with: it passes through the operations
/// its member owns and answers every other one with `preflight("not granted: <op>")` (D045,
/// ADR-0085 item 3). The host's own adapter is S6.7.2; this is the test's, built the same way.
struct Granted {
    runs: Arc<FakeRuns>,
    owns: &'static [&'static str],
}

impl Granted {
    fn check(&self, operation: &str) -> Result<(), WorkflowError> {
        if self.owns.contains(&operation) {
            Ok(())
        } else {
            Err(WorkflowError::Preflight(format!(
                "not granted: {operation}"
            )))
        }
    }
}

impl StartRuns for Granted {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move {
            self.check("start")?;
            self.runs.start(request).await
        })
    }
}

impl ObserveRuns for Granted {
    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            self.check("status")?;
            self.runs.status(id).await
        })
    }

    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            self.check("wait")?;
            self.runs.wait(id, cancel).await
        })
    }
}

impl CancelRuns for Granted {
    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            self.check("cancel")?;
            self.runs.cancel(id).await
        })
    }
}

fn assert_ok(outcome: &ToolOutcome, member: &str) {
    assert_eq!(
        outcome.status,
        ToolStatus::Ok,
        "{member}: {}",
        outcome.content
    );
}

// ---------------------------------------------------------------- cases

/// Each of the eight members loads from its built package and runs one call through its
/// linked interface against a fake service; every WIT record crosses both ways.
#[tokio::test]
async fn each_member_loads_and_runs_one_call_against_its_scoped_service() {
    within_deadline("each member runs one call", async {
        let loader = loader();
        let (service, scopes) = scopes();
        let scope = scopes.scope(key("main"));

        // worker_start: the spec goes in, the id and the child's description come back.
        let start = tool(&load(&loader, "p1/worker-start"), workers(&scope));
        let outcome = run(
            &start,
            "worker_start",
            json!({"environment": "child", "task": "do it", "tools": ["read"]}),
        )
        .await;
        assert_ok(&outcome, "worker_start");
        assert!(
            outcome.content.starts_with(
                "Started worker w1 on fake-route/fake-model with tools: read, finish."
            ),
            "{}",
            outcome.content
        );
        assert_eq!(service.calls(), ["start child do it [read] None"]);

        // worker_result: a finished child's whole result record, report included.
        service.set(
            "w1",
            ChildStatus::Finished(ChildResult {
                final_text: "all done".to_owned(),
                turn_end: TurnEnd::Completed {
                    stop: StopReason::EndTurn,
                },
                usage_total: None,
                report: WorkerReport {
                    tools: vec!["read".to_owned(), "finish".to_owned()],
                    finish: Some(FinishReport {
                        status: "done".to_owned(),
                        needs: None,
                        summary: Some("ok".to_owned()),
                        evidence: Some("commands passed: true".to_owned()),
                    }),
                    missing_tool_calls: vec![("grep".to_owned(), 2)],
                },
            }),
        );
        let result = tool(&load(&loader, "p1/worker-result"), workers(&scope));
        for wait in [false, true] {
            let outcome = run(&result, "worker_result", json!({"id": "w1", "wait": wait})).await;
            assert_ok(&outcome, "worker_result");
            assert_eq!(
                outcome.content,
                "tools: read, finish\nfinish: done — commands passed: true\n\
                 calls to tools it was not given: grep x2\n---\nWorker w1: finished\n\nall done",
                "wait {wait}"
            );
        }

        // worker_continue and worker_cancel: the id and the arguments reach the service.
        let continued = tool(&load(&loader, "p1/worker-continue"), workers(&scope));
        let outcome = run(
            &continued,
            "worker_continue",
            json!({"id": "w1", "message": "again", "add_tools": ["grep"]}),
        )
        .await;
        assert_ok(&outcome, "worker_continue");
        assert_eq!(
            outcome.content,
            "Added tools: grep. Message sent to worker w1."
        );
        let cancel = tool(&load(&loader, "p1/worker-cancel"), workers(&scope));
        let outcome = run(&cancel, "worker_cancel", json!({"id": "w1"})).await;
        assert_ok(&outcome, "worker_cancel");
        assert_eq!(outcome.content, "Worker w1 cancelled.");
        assert_eq!(
            service.calls()[1..],
            ["continue w1 again [grep]", "cancel w1"]
        );

        // The workflow members over one run service.
        let runs = Arc::new(FakeRuns::default());
        let services = WorkflowServices::of(runs.clone());
        let start = tool(
            &load(&loader, "p1/workflow-start"),
            workflows(services.clone()),
        );
        let outcome = run(
            &start,
            "workflow_start",
            json!({"script": "agent(\"x\", #{role: \"worker\"})", "args": {"k": 1}}),
        )
        .await;
        assert_ok(&outcome, "workflow_start");
        assert!(
            outcome.content.starts_with("Started workflow wf1."),
            "{}",
            outcome.content
        );
        {
            let starts = runs.starts.lock().unwrap();
            assert_eq!(starts.len(), 1);
            assert_eq!(starts[0].script, "agent(\"x\", #{role: \"worker\"})");
            assert_eq!(starts[0].args, json!({"k": 1}));
            assert_eq!(starts[0].resume_from, None);
        }

        let status = tool(
            &load(&loader, "p1/workflow-status"),
            workflows(services.clone()),
        );
        let outcome = run(&status, "workflow_status", json!({"id": "wf1"})).await;
        assert_ok(&outcome, "workflow_status");
        assert_eq!(
            outcome.content,
            "Workflow wf1: running — phase build, 1 steps started, 0 ended, 0 replayed\n  compiling"
        );

        runs.set("wf1", ended("wf1"));
        let result = tool(
            &load(&loader, "p1/workflow-result"),
            workflows(services.clone()),
        );
        let outcome = run(&result, "workflow_result", json!({"id": "wf1"})).await;
        assert_ok(&outcome, "workflow_result");
        assert!(
            outcome
                .content
                .starts_with("Workflow wf1: completed — 1 steps (0 replayed): 1 done"),
            "{}",
            outcome.content
        );
        assert!(
            outcome.content.contains("\"answer\": 42"),
            "{}",
            outcome.content
        );

        let cancel = tool(&load(&loader, "p1/workflow-cancel"), workflows(services));
        let outcome = run(&cancel, "workflow_cancel", json!({"id": "wf1"})).await;
        assert_ok(&outcome, "workflow_cancel");
        assert_eq!(outcome.content, "Workflow wf1 cancelled.");
        assert_eq!(*runs.cancels.lock().unwrap(), [RunId("wf1".to_owned())]);
    })
    .await;
}

/// `worker_result` declares and imports only `workers-observe` of the worker interfaces, and
/// its instance links with that one service alone; `worker_start` given the same services is
/// refused for the start it would need.
#[tokio::test]
async fn worker_result_imports_no_start_and_links_observe_alone() {
    within_deadline("worker_result links observe alone", async {
        let imports = package_file("p1-module-worker-result", ".imports");
        assert!(
            imports
                .lines()
                .any(|line| line == "p1:module/workers-observe@1.0.0"),
            "{imports}"
        );
        for absent in ["workers-start", "workers-control", "workflows"] {
            assert!(!imports.contains(absent), "{absent} in {imports}");
        }

        let loader = loader();
        // The loader refuses any import the manifest does not grant, so loading it proves the
        // component imports nothing beyond these.
        let module = load(&loader, "p1/worker-result");
        assert_eq!(module.capabilities(), ["control", "workers-observe"]);

        let (_service, scopes) = scopes();
        let scope = scopes.scope(key("main"));
        let observe_only = || Services {
            workers: Some(WorkerServices {
                observe: Some(Arc::new(scope.clone()) as Arc<dyn WorkersObserve>),
                ..WorkerServices::default()
            }),
            ..Services::default()
        };
        let result = tool(&module, observe_only());
        let outcome = run(&result, "worker_result", json!({"id": "w1"})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "No worker w1.");

        match link_error(&load(&loader, "p1/worker-start"), observe_only()) {
            LinkError::MissingService(capability) => assert_eq!(capability, "workers-start"),
            other => panic!("wrong link error: {other}"),
        }
    })
    .await;
}

/// A member whose manifest grants an interface the caller gave no service for is not built:
/// the link fails with `MissingService` naming that interface.
#[tokio::test]
async fn a_granted_interface_without_its_service_is_missing_service() {
    within_deadline("missing service", async {
        let loader = loader();
        for (_, name, _) in MEMBERS {
            let module = load(&loader, name);
            let needed = module
                .capabilities()
                .iter()
                .find(|capability| *capability != "control")
                .expect("each member grants one delegation interface")
                .clone();
            match link_error(&module, Services::default()) {
                LinkError::MissingService(capability) => assert_eq!(capability, needed, "{name}"),
                other => panic!("{name}: wrong link error: {other}"),
            }
        }

        // One interface present does not cover another: worker_start needs both of its own.
        let (_service, scopes) = scopes();
        let scope = scopes.scope(key("main"));
        let start_only = Services {
            workers: Some(WorkerServices {
                start: Some(Arc::new(scope)),
                ..WorkerServices::default()
            }),
            ..Services::default()
        };
        match link_error(&load(&loader, "p1/worker-start"), start_only) {
            LinkError::MissingService(capability) => assert_eq!(capability, "workers-observe"),
            other => panic!("wrong link error: {other}"),
        }
    })
    .await;
}

/// An instance linked to one scope cannot read or control a child another scope started: the
/// id is `unknown-child` through the WIT, and nothing reaches the service.
#[tokio::test]
async fn one_instance_cannot_reach_another_scopes_child() {
    within_deadline("scopes are separate", async {
        let loader = loader();
        let (service, scopes) = scopes();
        let mine = scopes.scope(key("main"));
        let theirs = scopes.scope(key("helper"));

        let start = tool(&load(&loader, "p1/worker-start"), workers(&mine));
        let outcome = run(
            &start,
            "worker_start",
            json!({"environment": "child", "task": "do it", "tools": ["read"]}),
        )
        .await;
        assert_ok(&outcome, "worker_start");

        let foreign_result = tool(&load(&loader, "p1/worker-result"), workers(&theirs));
        let outcome = run(&foreign_result, "worker_result", json!({"id": "w1"})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "No worker w1.");

        let foreign_cancel = tool(&load(&loader, "p1/worker-cancel"), workers(&theirs));
        let outcome = run(&foreign_cancel, "worker_cancel", json!({"id": "w1"})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "No worker w1.");
        assert_eq!(service.calls(), ["start child do it [read] None"]);

        // The starting scope still reads its own child.
        let own_result = tool(&load(&loader, "p1/worker-result"), workers(&mine));
        let outcome = run(&own_result, "worker_result", json!({"id": "w1"})).await;
        assert_ok(&outcome, "worker_result");
        assert_eq!(outcome.content, "Worker w1: running");
    })
    .await;
}

/// A workflow member linked through a per-member adapter is refused an operation its member
/// does not own with `preflight("not granted: start")`, which the member reports; nothing
/// starts. The operation the adapter grants passes through.
#[tokio::test]
async fn a_workflow_member_is_refused_an_operation_it_does_not_own() {
    within_deadline("not granted", async {
        let loader = loader();
        let runs = Arc::new(FakeRuns::default());
        // The adapter of `workflow_status`: it owns `status` and nothing else.
        let adapter = WorkflowServices::of(Arc::new(Granted {
            runs: runs.clone(),
            owns: &["status"],
        }));

        let start = tool(
            &load(&loader, "p1/workflow-start"),
            workflows(adapter.clone()),
        );
        let outcome = run(&start, "workflow_start", json!({"script": "1", "args": {}})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "Cannot start workflow: not granted: start");
        assert!(runs.starts.lock().unwrap().is_empty());

        let cancel = tool(
            &load(&loader, "p1/workflow-cancel"),
            workflows(adapter.clone()),
        );
        let outcome = run(&cancel, "workflow_cancel", json!({"id": "wf1"})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(
            outcome.content,
            "workflow cannot start: not granted: cancel"
        );
        assert!(runs.cancels.lock().unwrap().is_empty());

        let status = tool(&load(&loader, "p1/workflow-status"), workflows(adapter));
        let outcome = run(&status, "workflow_status", json!({"id": "wf1"})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "No workflow wf1.");
    })
    .await;
}

/// `workers-observe.wait` on a running child answers `running` once the call is cancelled,
/// even from a service that ignores the token; the member reports the cancellation.
#[tokio::test]
async fn a_cancelled_wait_answers_running_through_the_wit() {
    within_deadline("cancelled wait", async {
        let loader = loader();
        let (service, scopes) = scopes();
        let scope = scopes.scope(key("main"));
        let started = scope
            .start(ChildSpec {
                environment: "child".to_owned(),
                task: "do it".to_owned(),
                tools: vec!["read".to_owned()],
                workspace: None,
            })
            .await
            .expect("start");
        assert_eq!(started.0, "w1");

        let result = tool(&load(&loader, "p1/worker-result"), workers(&scope));
        let cancel = CancellationToken::new();
        let waiting = tokio::spawn({
            let cancel = cancel.clone();
            async move {
                result
                    .execute(
                        &call("worker_result", json!({"id": "w1", "wait": true})),
                        ToolContext { cancel },
                    )
                    .await
            }
        });
        service.waiting.notified().await;
        cancel.cancel();
        let outcome = waiting.await.expect("the call task");
        assert_eq!(outcome.status, ToolStatus::Cancelled, "{}", outcome.content);
    })
    .await;
}

// ---------------------------------------------------------------- the host assembly (S6.7.2)
//
// The cases below go through the host: the members are selected by `modules.lock` keys,
// loaded and registered by the host's catalog entry point (`p1_host::catalog::modules`) with
// the module hook the host installs (`p1_host::workflow::member_services` over
// `worker_member_services`, as `catalog/children.rs` and `workflow::compose` install it), and
// assembled for a main agent through `p1_assembly::assemble_for_agent`, the call the host's
// `assemble_with_cache_key` makes (B-S6-9, D068).

/// A run service over [`FakeRuns`]: the host hook takes the whole `WorkflowService`, as the
/// host's composed service is one.
struct HostRuns(Arc<FakeRuns>);

impl WorkflowService for HostRuns {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        StartRuns::start(self.0.as_ref(), request)
    }

    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        ObserveRuns::status(self.0.as_ref(), id)
    }

    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        ObserveRuns::wait(self.0.as_ref(), id, cancel)
    }

    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        CancelRuns::cancel(self.0.as_ref(), id)
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        Box::pin(async move { self.0.runs.lock().unwrap().clone() })
    }
}

/// A release holding the eight built member packages, laid out as p1's release ships them.
fn host_release() -> Release {
    let mut release = Release::empty();
    for (package, _, _) in MEMBERS {
        let manifest: Value =
            serde_json::from_str(&package_file(package, ".manifest.json")).expect("JSON");
        let wasm_path = built().join(package).join(format!("{package}.wasm"));
        let bytes = std::fs::read(&wasm_path).expect("the built component");
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

/// The `modules.lock` a release of the members is selected by: each member under its
/// documented key (the module id without `p1/`), pinning what the release ships.
fn host_lock(release: &Release) -> ModulesLock {
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

/// The hook the host installs for a run: the worker family's over `scopes`, with the
/// workflow family's over `runs` added. `asked` records each module id it is asked for.
fn host_hook(
    scopes: &Arc<MemberScopes>,
    runs: Arc<dyn WorkflowService>,
    asked: Arc<Mutex<Vec<String>>>,
) -> ModuleServices {
    let hook = member_services(Some(worker_member_services(scopes.clone(), None)), runs);
    Arc::new(move |module: &str, services: &ToolServices| {
        asked.lock().unwrap().push(module.to_owned());
        hook(module, services)
    })
}

/// A catalog with a scripted provider, a `probe` tool that keeps the `ToolServices` it is
/// assembled with, and the eight members registered through the host's entry point.
struct HostCatalog {
    catalog: Catalog,
    probed: Arc<Mutex<Option<ToolServices>>>,
    asked: Arc<Mutex<Vec<String>>>,
    _release: Release,
}

fn host_catalog(scopes: &Arc<MemberScopes>, runs: Arc<dyn WorkflowService>) -> HostCatalog {
    let release = host_release();
    let mut catalog = Catalog::new();
    let provider = ScriptedProvider::new(Vec::new());
    catalog.provider(
        "scripted",
        Box::new(move |_spec: &ProviderSpec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
    );
    let probed = Arc::new(Mutex::new(None));
    let slot = probed.clone();
    catalog.tool(
        "probe",
        Box::new(move |_spec: &ToolSpec, services: &ToolServices| {
            *slot.lock().unwrap() = Some(services.clone());
            Ok(Arc::new(FakeTool::new("probe")) as Arc<dyn Tool>)
        }),
    );
    let asked = Arc::new(Mutex::new(Vec::new()));
    let packages = load_locked_modules(&host_lock(&release), &release.manifest_file())
        .expect("the members load through the host");
    register_modules(
        &mut catalog,
        packages,
        host_hook(scopes, runs, asked.clone()),
    )
    .expect("registration");
    HostCatalog {
        catalog,
        probed,
        asked,
        _release: release,
    }
}

impl HostCatalog {
    fn asked(&self) -> Vec<String> {
        self.asked.lock().unwrap().clone()
    }
}

fn host_environment(modules: &[&str]) -> EnvironmentFile {
    EnvironmentFile {
        name: "activation-host".into(),
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
        prompt_template: "tools: {{tool_names}}".into(),
        context: None,
        summarize_prompt: None,
    }
}

/// Assembles `modules` for the main agent `agent`, as the host assembles a parent.
fn assemble_main(
    catalog: &HostCatalog,
    modules: &[&str],
    agent: &str,
    workspace: &Path,
) -> Vec<Arc<dyn Tool>> {
    assemble_for_agent(
        &catalog.catalog,
        &host_environment(modules),
        workspace,
        &Substitutions {
            workspace: workspace.display().to_string(),
            date: "2026-01-01".into(),
            os: "linux".into(),
        },
        &Arc::new(MaskCounter::new()),
        Some(agent),
        |_| ModelOptions::default(),
    )
    .unwrap_or_else(|error| panic!("assembly: {error}"))
    .tools
}

fn named<'a>(tools: &'a [Arc<dyn Tool>], name: &str) -> &'a Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.declaration().name == name)
        .unwrap_or_else(|| panic!("{name} is not assembled"))
}

fn names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect()
}

fn start_input() -> Value {
    json!({"environment": "child", "task": "do it", "tools": ["read"]})
}

/// Every member, selected by its lock key and assembled for a main agent through the host
/// catalog, runs against the service the host scoped for that parent: the child the
/// `worker_start` member starts is in the parent's scope and in no other, and the workflow
/// members reach the run service through their adapters.
#[tokio::test]
async fn a_member_assembled_by_name_through_the_host_runs_against_the_scoped_service() {
    within_deadline("host assembly by name", async {
        let (service, _) = scopes();
        let member_scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        let runs = Arc::new(FakeRuns::default());
        let catalog = host_catalog(&member_scopes, Arc::new(HostRuns(runs.clone())));
        let workspace = tempfile::tempdir().expect("workspace");
        let keys: Vec<&str> = WORKER_MODULES
            .iter()
            .chain(WORKFLOW_MODULES.iter())
            .map(|module| module.strip_prefix("p1/").expect("a p1 module"))
            .collect();
        let tools = assemble_main(&catalog, &keys, "0", workspace.path());
        assert_eq!(
            names(&tools),
            MEMBERS.map(|(_, _, tool)| tool),
            "each member is its package"
        );
        let mut asked = catalog.asked();
        asked.sort();
        let mut expected: Vec<&str> = MEMBERS.iter().map(|(_, name, _)| *name).collect();
        expected.sort();
        assert_eq!(asked, expected, "the hook is asked by module id");

        let outcome = run(named(&tools, "worker_start"), "worker_start", start_input()).await;
        assert_ok(&outcome, "worker_start");
        assert_eq!(service.calls(), ["start child do it [read] None"]);
        let child = ChildId("w1".to_owned());
        assert_eq!(
            member_scopes.workers("0").status(&child).await,
            Ok(ChildStatus::Running),
            "the child is in the parent's scope"
        );
        assert_eq!(
            member_scopes.workers("1").status(&child).await,
            Err(WorkerError::UnknownChild),
            "and in no other parent's"
        );
        let outcome = run(
            named(&tools, "worker_result"),
            "worker_result",
            json!({"id": "w1"}),
        )
        .await;
        assert_ok(&outcome, "worker_result");
        assert_eq!(outcome.content, "Worker w1: running");

        let outcome = run(
            named(&tools, "workflow_start"),
            "workflow_start",
            json!({"script": "1", "args": {}}),
        )
        .await;
        assert_ok(&outcome, "workflow_start");
        assert_eq!(runs.starts.lock().unwrap().len(), 1);
        let outcome = run(
            named(&tools, "workflow_status"),
            "workflow_status",
            json!({"id": "wf1"}),
        )
        .await;
        assert_ok(&outcome, "workflow_status");
        let outcome = run(
            named(&tools, "workflow_cancel"),
            "workflow_cancel",
            json!({"id": "wf1"}),
        )
        .await;
        assert_ok(&outcome, "workflow_cancel");
        assert_eq!(*runs.cancels.lock().unwrap(), [RunId("wf1".to_owned())]);
    })
    .await;
}

/// The host's adapter gives each workflow member the operations it owns and answers every
/// other one with `preflight("not granted: <op>")` before the run service is asked (D045).
#[tokio::test]
async fn the_host_adapter_refuses_a_workflow_member_what_it_does_not_own() {
    within_deadline("host adapter", async {
        let (service, _) = scopes();
        let member_scopes = MemberScopes::new(service as Arc<dyn WorkerService>);
        let runs = Arc::new(FakeRuns::default());
        let host_runs: Arc<dyn WorkflowService> = Arc::new(HostRuns(runs.clone()));
        let catalog = host_catalog(&member_scopes, host_runs.clone());
        let workspace = tempfile::tempdir().expect("workspace");
        assemble_main(&catalog, &["probe"], "0", workspace.path());
        let services = catalog
            .probed
            .lock()
            .unwrap()
            .clone()
            .expect("the probe was assembled");
        assert_eq!(
            services.agent.as_deref(),
            Some("0"),
            "a main agent is named"
        );
        let hook = member_services(None, host_runs);
        let adapter = |module: &str| hook(module, &services).workflows.expect(module);
        fn refused<T>(operation: &str) -> Result<T, WorkflowError> {
            Err(WorkflowError::Preflight(format!(
                "not granted: {operation}"
            )))
        }
        let run_id = RunId("wf1".to_owned());
        let request = || StartRequest {
            script: "1".to_owned(),
            args: json!({}),
            resume_from: None,
            role_models: Default::default(),
            workspace: None,
            base: None,
        };

        let status = adapter("p1/workflow-status");
        assert_eq!(status.start.start(request()).await, refused("start"));
        assert_eq!(
            status.observe.wait(&run_id, CancellationToken::new()).await,
            refused("wait")
        );
        assert_eq!(status.cancel.cancel(&run_id).await, refused("cancel"));
        assert_eq!(
            status.observe.status(&run_id).await,
            Err(WorkflowError::UnknownRun)
        );

        let result = adapter("p1/workflow-result");
        assert_eq!(result.start.start(request()).await, refused("start"));
        assert_eq!(result.cancel.cancel(&run_id).await, refused("cancel"));
        assert_eq!(
            result.observe.wait(&run_id, CancellationToken::new()).await,
            Err(WorkflowError::UnknownRun),
            "wait is its own"
        );

        let cancel = adapter("p1/workflow-cancel");
        assert_eq!(cancel.start.start(request()).await, refused("start"));
        assert_eq!(cancel.observe.status(&run_id).await, refused("status"));

        let start = adapter("p1/workflow-start");
        assert_eq!(start.observe.status(&run_id).await, refused("status"));
        assert_eq!(start.cancel.cancel(&run_id).await, refused("cancel"));
        assert!(
            runs.starts.lock().unwrap().is_empty(),
            "no refused start reached the service"
        );
        assert_eq!(
            start.start.start(request()).await,
            Ok(RunId("wf1".to_owned()))
        );
    })
    .await;
}

/// An environment that names only `worker-result` gets that member and nothing else of the
/// family: no `worker_start` is assembled, so none can be dispatched, and the one member's
/// component has no start to call.
#[tokio::test]
async fn worker_result_assembled_alone_cannot_dispatch_worker_start() {
    within_deadline("worker_result alone", async {
        let (service, _) = scopes();
        let member_scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        let catalog = host_catalog(&member_scopes, Arc::new(HostRuns(Arc::default())));
        let workspace = tempfile::tempdir().expect("workspace");
        let tools = assemble_main(&catalog, &["worker-result"], "0", workspace.path());
        assert_eq!(names(&tools), ["worker_result"]);
        assert_eq!(catalog.asked(), ["p1/worker-result"]);
        assert!(
            !package_file("p1-module-worker-result", ".imports").contains("workers-start"),
            "its component does not import workers-start"
        );
        let outcome = run(&tools[0], "worker_result", json!({"id": "w1"})).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "No worker w1.");
        assert!(service.calls().is_empty(), "nothing started a child");
    })
    .await;
}

/// A member the release installs and the lock resolves, but the environment does not name,
/// is never instantiated and cannot be dispatched; a module id is not a selectable name.
#[tokio::test]
async fn an_installed_but_unassembled_member_cannot_dispatch() {
    within_deadline("installed but unassembled", async {
        let (service, _) = scopes();
        let member_scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        let catalog = host_catalog(&member_scopes, Arc::new(HostRuns(Arc::default())));
        assert!(
            catalog
                .catalog
                .tool_keys()
                .contains(&"worker-start".to_owned()),
            "installed and resolved"
        );
        let workspace = tempfile::tempdir().expect("workspace");
        let tools = assemble_main(
            &catalog,
            &["probe", "workflow-status"],
            "0",
            workspace.path(),
        );
        assert_eq!(names(&tools), ["probe", "workflow_status"]);
        assert_eq!(
            catalog.asked(),
            ["p1/workflow-status"],
            "no worker member was instantiated"
        );
        assert!(service.calls().is_empty());

        let assembled = assemble_for_agent(
            &catalog.catalog,
            &host_environment(&["p1/worker-start"]),
            workspace.path(),
            &Substitutions {
                workspace: "/work".into(),
                date: "2026-01-01".into(),
                os: "linux".into(),
            },
            &Arc::new(MaskCounter::new()),
            Some("0"),
            |_| ModelOptions::default(),
        );
        match assembled {
            Err(AssemblyError::UnknownToolModule { module, .. }) => {
                assert_eq!(module, "p1/worker-start")
            }
            Err(other) => panic!("wrong refusal: {other}"),
            Ok(_) => panic!("a module id is not a catalog key"),
        }
    })
    .await;
}

/// Two main agents never reach each other's children: not two parents sharing one
/// generation, and not two main agents each with its own generation under the same name.
#[tokio::test]
async fn two_main_agents_scopes_cannot_reach_each_others_children() {
    within_deadline("two main agents", async {
        let (service, _) = scopes();
        let member_scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        let catalog = host_catalog(&member_scopes, Arc::new(HostRuns(Arc::default())));
        let workspace = tempfile::tempdir().expect("workspace");
        let members = ["worker-start", "worker-result", "worker-cancel"];
        let first = assemble_main(&catalog, &members, "a", workspace.path());
        let second = assemble_main(&catalog, &members, "b", workspace.path());

        let outcome = run(named(&first, "worker_start"), "worker_start", start_input()).await;
        assert_ok(&outcome, "worker_start");
        for (tool, name) in [
            (named(&second, "worker_result"), "worker_result"),
            (named(&second, "worker_cancel"), "worker_cancel"),
        ] {
            let outcome = run(tool, name, json!({"id": "w1"})).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{name}");
            assert_eq!(outcome.content, "No worker w1.", "{name}");
        }
        assert_eq!(service.calls(), ["start child do it [read] None"]);
        let outcome = run(
            named(&first, "worker_result"),
            "worker_result",
            json!({"id": "w1"}),
        )
        .await;
        assert_eq!(outcome.content, "Worker w1: running");

        // Another main agent of the same process: its own generation over the same service,
        // with the same name the host gives every parent.
        let other_scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        assert_ne!(other_scopes.generation(), member_scopes.generation());
        let other = host_catalog(&other_scopes, Arc::new(HostRuns(Arc::default())));
        let theirs = assemble_main(&other, &members, "a", workspace.path());
        let outcome = run(
            named(&theirs, "worker_result"),
            "worker_result",
            json!({"id": "w1"}),
        )
        .await;
        assert_eq!(outcome.content, "No worker w1.");
    })
    .await;
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

/// Teardown retires the main agent's generation as `run.rs` does: the member's ids become
/// unknown through every scope of it, a later assembly of the same generation starts
/// nothing, and the running child is untouched: it completes and the parent hears of it.
#[tokio::test]
async fn teardown_retires_the_generation_and_a_running_child_still_completes_and_notifies() {
    within_deadline("teardown retire", async {
        let gate = Arc::new(Semaphore::new(0));
        let built = Arc::new(Mutex::new(0_usize));
        let factory: AgentFactory = {
            let gate = gate.clone();
            let built = built.clone();
            Arc::new(move |_spec: &ChildSpec| {
                *built.lock().unwrap() += 1;
                Ok(ChildAgent {
                    agent: test_agent(Arc::new(GatedProvider {
                        gate: gate.clone(),
                        inner: ScriptedProvider::new(vec![text_response("done")]),
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
        let member_scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
        let catalog = host_catalog(&member_scopes, Arc::new(HostRuns(Arc::default())));
        let workspace = tempfile::tempdir().expect("workspace");
        let members = ["worker-start", "worker-result"];
        let tools = assemble_main(&catalog, &members, "0", workspace.path());

        let outcome = run(named(&tools, "worker_start"), "worker_start", start_input()).await;
        assert_ok(&outcome, "worker_start");
        let child = ChildId("w1".to_owned());
        let outcome = run(
            named(&tools, "worker_result"),
            "worker_result",
            json!({"id": "w1"}),
        )
        .await;
        assert_eq!(outcome.content, "Worker w1: running");

        // What `run.rs` does when the main agent's assembly is dropped.
        member_scopes
            .registry()
            .retire_generation(member_scopes.generation())
            .await;

        let outcome = run(
            named(&tools, "worker_result"),
            "worker_result",
            json!({"id": "w1"}),
        )
        .await;
        assert_eq!(outcome.content, "No worker w1.");
        let again = assemble_main(&catalog, &members, "0", workspace.path());
        let outcome = run(named(&again, "worker_start"), "worker_start", start_input()).await;
        assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
        assert_eq!(
            *built.lock().unwrap(),
            1,
            "a retired generation builds no child"
        );
        assert_eq!(
            service.status(&child).await,
            Ok(ChildStatus::Running),
            "retiring cancelled nothing"
        );

        gate.add_permits(1);
        match service.wait(&child, CancellationToken::new()).await {
            Ok(ChildStatus::Finished(result)) => assert_eq!(result.final_text, "done"),
            other => panic!("{other:?}"),
        }
        assert!(
            parent.has_pending_inbox(),
            "the parent still hears of it once"
        );
    })
    .await;
}
