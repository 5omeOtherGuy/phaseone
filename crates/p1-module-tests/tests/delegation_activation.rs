//! S6.7.1 (#313): the eight worker and workflow members, as built packages, run through the
//! runtime's worker and workflow link (`p1_module_runtime::delegation`, B-S6-8, D065).
//!
//! Each case loads the built packages through the runtime `Loader`, builds a `WasmTool` with a
//! filled `Services` and calls it; there is no host here (the host assembly is S6.7.2). The
//! worker members run over a real `p1_workers::WorkerScope` on a fake `WorkerService` that
//! records what reached it, and the workflow members over a fake run service. Every ordering is
//! explicit (a `Notify`, a cancel token); nothing sleeps or asserts on time, and each case runs
//! under the harness's deadlock guard.

use std::future::pending;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    BoxFuture, CancellationToken, StopReason, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome,
    ToolStatus, TurnEnd,
};
use p1_module_runtime::delegation::{WorkerServices, WorkflowServices};
use p1_module_runtime::{
    ExecutionLimits, LinkError, LoadedModule, Loader, ReleaseManifest, Services, ToolError,
    wasm_tool,
};
use p1_module_tests::within_deadline;
use p1_redact::MaskCounter;
use p1_workers::{
    ChildId, ChildResult, ChildSpec, ChildStatus, FinishReport, ScopeKey, WorkerError,
    WorkerReport, WorkerScope, WorkerScopes, WorkerService, WorkersObserve, WorkersStart,
};
use p1_workflow::{
    CancelRuns, Counts, ObserveRuns, RunId, RunOutcome, RunProgress, RunReport, RunStatus,
    StartRequest, StartRuns, WorkflowError,
};
use tokio::sync::Notify;

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
