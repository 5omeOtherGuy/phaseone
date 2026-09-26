//! The workflow substrate split (S6.3): the native substrate of `p1-workflow` asking its
//! decisions through the `Decisions` seam, under the script shapes that run `agent()` from
//! several OS threads at once.
//!
//! - Nested fan-outs — `parallel` inside `pipeline` stages, `pipeline` inside `parallel`
//!   thunks, and a mix that walks a fallback chain, takes the repair turn and hits a cap —
//!   give the same values and counts through a recording seam as through the plain native
//!   service, and the values the engine gave before the split.
//! - Shared captures keep their semantics: curried thunks run clean, and a closure several
//!   thunks method-call is still the data race the engine reports (`audit_script_race.rs`).
//! - No Store re-entry: a recording `Decisions` counts the entries of every call, per thread
//!   and across threads. No decision call is ever entered while another is live on the same
//!   thread, and no runner call or Rhai callback (a `log`, a fan-out, an observer event) runs
//!   inside a decision call. One case holds two decision calls live at once on two threads
//!   with a barrier, which the native decisions must bear.
//!
//! - The loaded decision component (S6.9): every case above also runs with the recording
//!   seam over `WasmWorkflowDecisions`, the adapter over the built
//!   `p1-module-workflow-decision` component, and must give the same values and counts. There
//!   the adapter's instance count must equal the decision calls: each call had an instance of
//!   its own, so no instance was entered twice — neither by two threads at once nor across a
//!   callback, which the recording seam checks as for the native decisions.
//!
//! The runner is a fake, and every wait is on an explicit signal (a barrier, a `Notify`);
//! no case sleeps or asserts on time. Each case runs under S0's deadlock guard.

use std::cell::Cell;
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Barrier, Mutex};

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{BoxFuture, CancellationToken};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, WasmWorkflowDecisions};
use p1_module_tests::within_deadline;
use p1_workflow::decision::{AttemptOutcome, PlanRequest, Snapshot, Transition};
use p1_workflow::{
    Counts, Decisions, InProcessWorkflows, ModelResolver, NativeDecisions, ResolvedModel, RoleSpec,
    RunId, RunOutcome, RunReport, RunStatus, SchemaCheck, StartRequest, StepEnd, StepLine,
    StepOutcome, StepRequest, StepRunner, WorkerRef, WorkflowObserver, WorkflowService,
    WorkflowSettings,
};
use tokio::sync::Notify;

// ------------------------------------------------------------------ the recording seam

thread_local! {
    /// Decision calls live on this thread right now.
    static DEPTH: Cell<usize> = const { Cell::new(0) };
}

/// What the recording seam and the fakes saw.
#[derive(Default)]
struct Seen {
    plans: AtomicUsize,
    accepts: AtomicUsize,
    /// Decision calls live across all threads, and the most at once.
    live: AtomicUsize,
    max_live: AtomicUsize,
    /// Entries of a decision call while another was live on the same thread.
    nested: AtomicUsize,
    /// Runner calls and Rhai callbacks that ran inside a decision call, by name.
    inside: Mutex<Vec<String>>,
}

impl Seen {
    /// Records that `what` ran; a violation when a decision call is live on this thread.
    fn outside(&self, what: &str) {
        if DEPTH.with(Cell::get) > 0 {
            self.inside
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .push(what.to_string());
        }
    }

    fn assert_no_reentry(&self) {
        assert_eq!(
            self.nested.load(Ordering::SeqCst),
            0,
            "a decision call was entered inside another on the same thread"
        );
        let inside = self
            .inside
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        assert!(
            inside.is_empty(),
            "these ran inside a decision call: {inside:?}"
        );
        assert_eq!(self.live.load(Ordering::SeqCst), 0, "a call is still live");
    }
}

/// Two first plans that must be live at the same time: each waits at the barrier for the
/// other, inside its decision call, before it answers.
struct Meet {
    labels: [&'static str; 2],
    barrier: Barrier,
}

/// Decisions — the native ones or the loaded component — with every call counted.
struct Recording {
    seen: Arc<Seen>,
    meet: Option<Meet>,
    inner: Arc<dyn Decisions>,
}

impl Recording {
    fn new(seen: Arc<Seen>) -> Self {
        Self {
            seen,
            meet: None,
            inner: Arc::new(NativeDecisions),
        }
    }

    /// The loaded decision component behind the recording seam.
    fn loaded(seen: Arc<Seen>, decisions: Arc<WasmWorkflowDecisions>) -> Self {
        Self {
            seen,
            meet: None,
            inner: decisions,
        }
    }

    fn call<T>(&self, count: &AtomicUsize, body: impl FnOnce() -> T) -> T {
        count.fetch_add(1, Ordering::SeqCst);
        let depth = DEPTH.with(|depth| {
            let entered = depth.get();
            depth.set(entered + 1);
            entered
        });
        if depth > 0 {
            self.seen.nested.fetch_add(1, Ordering::SeqCst);
        }
        let live = self.seen.live.fetch_add(1, Ordering::SeqCst) + 1;
        self.seen.max_live.fetch_max(live, Ordering::SeqCst);
        let answer = body();
        self.seen.live.fetch_sub(1, Ordering::SeqCst);
        DEPTH.with(|depth| depth.set(depth.get() - 1));
        answer
    }
}

impl Decisions for Recording {
    fn plan_step(&self, snapshot: &Snapshot, request: &PlanRequest) -> Result<Transition, String> {
        self.call(&self.seen.plans, || {
            if let Some(meet) = &self.meet
                && snapshot.step.link.is_none()
                && request
                    .label
                    .as_deref()
                    .is_some_and(|label| meet.labels.contains(&label))
            {
                meet.barrier.wait();
            }
            self.inner.plan_step(snapshot, request)
        })
    }

    fn accept_step(
        &self,
        snapshot: &Snapshot,
        outcome: &AttemptOutcome,
    ) -> Result<Transition, String> {
        self.call(&self.seen.accepts, || {
            self.inner.accept_step(snapshot, outcome)
        })
    }
}

// ------------------------------------------------------------------ the loaded component

/// The built decision package, as `scripts/build-modules.sh` publishes it.
const DECISION_PACKAGE: (&str, &str) = ("p1-module-workflow-decision", "p1/workflow-decision");

/// The adapter over the built decision component, loaded through a release manifest as a
/// release lays it out.
fn loaded_decisions() -> Arc<WasmWorkflowDecisions> {
    let built = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules");
    let (package, name) = DECISION_PACKAGE;
    let manifest_path = built.join(package).join(format!("{package}.manifest.json"));
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(&manifest_path).unwrap_or_else(|error| {
            panic!(
                "the build output {} is missing ({error}): run scripts/build-modules.sh first",
                manifest_path.display()
            )
        }),
    )
    .expect("the package manifest is JSON");
    let entry = json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("{package}/{package}.wasm"),
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"],
        "variant": manifest["variant"],
    });
    let release = json!({ "format": "p1-release-manifest/1", "components": [entry] });
    let release = ReleaseManifest::parse(&release.to_string()).expect("release manifest");
    let module = Loader::new(release, built)
        .expect("loader")
        .load(name)
        .expect("the built decision component loads");
    Arc::new(
        WasmWorkflowDecisions::new(&module, ExecutionLimits::default())
            .expect("the decision adapter builds"),
    )
}

/// Every decision call the recording seam saw had an instance of its own.
fn assert_one_instance_per_call(seen: &Seen, decisions: &WasmWorkflowDecisions) {
    let calls = seen.plans.load(Ordering::SeqCst) + seen.accepts.load(Ordering::SeqCst);
    assert!(calls > 0, "the loaded component was asked");
    assert_eq!(
        decisions.instances(),
        calls as u64,
        "one instance per decision call, none entered twice"
    );
}

// ------------------------------------------------------------------ the fakes

/// A step parked in the runner until the case lets it go.
struct Park {
    reached: Notify,
    release: Notify,
}

/// Answers by prompt: `route …` fails its route on the head of the chain, `nudge …` ends
/// without `finish` and is repaired, a parked prompt waits for its release, and every other
/// prompt is done with `did: <prompt>`.
struct FakeRunner {
    seen: Arc<Seen>,
    workers: AtomicUsize,
    parked: Mutex<BTreeMap<String, Arc<Park>>>,
}

impl FakeRunner {
    fn new(seen: Arc<Seen>) -> Arc<Self> {
        Arc::new(Self {
            seen,
            workers: AtomicUsize::new(0),
            parked: Mutex::new(BTreeMap::new()),
        })
    }

    fn park(&self, prompt: &str) -> Arc<Park> {
        let park = Arc::new(Park {
            reached: Notify::new(),
            release: Notify::new(),
        });
        self.parked
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
            .insert(prompt.to_string(), park.clone());
        park
    }
}

fn done(summary: String) -> StepEnd {
    StepEnd::Done {
        summary,
        evidence: "commands passed: fake".to_string(),
        result: None,
        schema: SchemaCheck::NotRequested,
    }
}

impl StepRunner for FakeRunner {
    fn run<'a>(
        &'a self,
        request: &'a StepRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepOutcome, String>> {
        Box::pin(async move {
            self.seen.outside("runner.run");
            let number = self.workers.fetch_add(1, Ordering::SeqCst) + 1;
            let worker = WorkerRef {
                id: format!("w{number}|{}", request.prompt),
                description: request.model.reference.clone(),
            };
            let park = self
                .parked
                .lock()
                .unwrap_or_else(|poison| poison.into_inner())
                .get(&request.prompt)
                .cloned();
            if let Some(park) = park {
                park.reached.notify_one();
                park.release.notified().await;
            }
            let end =
                if request.prompt.starts_with("route ") && request.model.reference == "env/head" {
                    StepEnd::RouteFailed {
                        model: String::new(),
                        error: "no balance".to_string(),
                    }
                } else if request.prompt.starts_with("nudge ") {
                    StepEnd::EndedWithoutFinish {
                        text: "forgot".to_string(),
                    }
                } else {
                    done(format!("did: {}", request.prompt))
                };
            Ok(StepOutcome { worker, end })
        })
    }

    fn repair<'a>(
        &'a self,
        worker: &'a WorkerRef,
        _message: String,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepEnd, String>> {
        Box::pin(async move {
            self.seen.outside("runner.repair");
            let prompt = worker.id.split_once('|').map_or("", |(_, prompt)| prompt);
            Ok(done(format!("repaired: {prompt}")))
        })
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
            reference: reference.to_string(),
            environment: environment.to_string(),
            profile: profile.to_string(),
            effort: None,
            wire_model: profile.to_string(),
        })
    }
}

/// The observer: every event is a Rhai-driven callback that must not run inside a decision.
struct Observer {
    seen: Arc<Seen>,
    thunk_failed: Notify,
}

impl WorkflowObserver for Observer {
    fn phase(&self, _id: &RunId, _name: &str) {
        self.seen.outside("observer.phase");
    }
    fn log(&self, _id: &RunId, _text: &str) {
        self.seen.outside("observer.log");
    }
    fn step_started(&self, _id: &RunId, _request: &StepRequest, _worker: &WorkerRef) {
        self.seen.outside("observer.step_started");
    }
    fn step_ended(&self, _id: &RunId, _line: &StepLine) {
        self.seen.outside("observer.step_ended");
    }
    fn thunk_failed(&self, _id: &RunId, _error: &str) {
        self.seen.outside("observer.thunk_failed");
        self.thunk_failed.notify_one();
    }
    fn jobs_queued(&self, _id: &RunId, _count: usize) {
        self.seen.outside("observer.jobs_queued");
    }
    fn run_ended(&self, _id: &RunId, _report: &RunReport) {
        self.seen.outside("observer.run_ended");
    }
}

// ------------------------------------------------------------------ the harness

/// `worker` walks `env/head → env/next`; `judge` is `env/fable`, capped at one attempt.
fn settings() -> WorkflowSettings {
    let role = |model: &str, fallback: &[&str]| RoleSpec {
        model: model.to_string(),
        fallback: fallback.iter().map(|model| model.to_string()).collect(),
        tools: vec!["read".to_string()],
    };
    WorkflowSettings {
        roles: BTreeMap::from([
            ("worker".to_string(), role("env/head", &["env/next"])),
            ("judge".to_string(), role("env/fable", &[])),
        ]),
        caps: BTreeMap::from([("fable".to_string(), 1)]),
        max_steps: 200,
        max_threads: 8,
    }
}

/// Which decisions the service asks.
enum Seam {
    /// `InProcessWorkflows::new`: the native decisions, as the host composes them.
    Native,
    /// The native decisions behind the recording seam.
    Recorded(Recording),
}

struct Service {
    service: Arc<InProcessWorkflows>,
    runner: Arc<FakeRunner>,
    observer: Arc<Observer>,
    _root: tempfile::TempDir,
}

impl Service {
    fn new(seen: &Arc<Seen>, seam: Seam) -> Self {
        let root = tempfile::tempdir().expect("temp dir");
        let runner = FakeRunner::new(seen.clone());
        let observer = Arc::new(Observer {
            seen: seen.clone(),
            thunk_failed: Notify::new(),
        });
        let run_root: PathBuf = root.path().to_path_buf();
        let service = match seam {
            Seam::Native => InProcessWorkflows::new(
                runner.clone(),
                Arc::new(Resolver),
                observer.clone(),
                settings(),
                run_root,
            ),
            Seam::Recorded(recording) => InProcessWorkflows::with_decisions(
                runner.clone(),
                Arc::new(Resolver),
                observer.clone(),
                settings(),
                run_root,
                Arc::new(recording),
            ),
        };
        Self {
            service,
            runner,
            observer,
            _root: root,
        }
    }

    async fn start(&self, script: &str, args: Value) -> RunId {
        self.service
            .start(StartRequest {
                script: script.to_string(),
                args,
                resume_from: None,
                role_models: BTreeMap::new(),
                workspace: None,
                base: None,
            })
            .await
            .expect("start")
    }

    async fn wait(&self, id: &RunId) -> RunReport {
        match self
            .service
            .wait(id, CancellationToken::new())
            .await
            .expect("wait")
        {
            RunStatus::Ended(report) => report,
            RunStatus::Running(progress) => panic!("still running: {progress:?}"),
        }
    }
}

/// Runs `script` natively, through the recording seam over the native decisions and through
/// the recording seam over the loaded decision component; all three must end `outcome` with
/// `value` and `counts`. Returns what the recording seam over the native decisions saw.
async fn both_ways(
    script: &str,
    args: Value,
    outcome: RunOutcome,
    value: Value,
    counts: Counts,
) -> Arc<Seen> {
    let native_seen = Arc::new(Seen::default());
    let native = Service::new(&native_seen, Seam::Native);
    let id = native.start(script, args.clone()).await;
    let native_report = native.wait(&id).await;

    let seen = Arc::new(Seen::default());
    let recorded = Service::new(&seen, Seam::Recorded(Recording::new(seen.clone())));
    let id = recorded.start(script, args.clone()).await;
    let recorded_report = recorded.wait(&id).await;

    let loaded_seen = Arc::new(Seen::default());
    let decisions = loaded_decisions();
    let loaded = Service::new(
        &loaded_seen,
        Seam::Recorded(Recording::loaded(loaded_seen.clone(), decisions.clone())),
    );
    let id = loaded.start(script, args).await;
    let loaded_report = loaded.wait(&id).await;

    for (seam, report) in [
        ("native", &native_report),
        ("recorded", &recorded_report),
        ("loaded", &loaded_report),
    ] {
        assert_eq!(report.outcome, outcome, "{seam}: {report:?}");
        assert_eq!(report.value, value, "{seam}");
        assert_eq!(report.counts, counts, "{seam}");
    }
    seen.assert_no_reentry();
    let steps = recorded_report.counts.steps as usize;
    assert!(
        seen.plans.load(Ordering::SeqCst) >= steps,
        "every step is planned"
    );
    loaded_seen.assert_no_reentry();
    assert_eq!(
        loaded_seen.plans.load(Ordering::SeqCst),
        seen.plans.load(Ordering::SeqCst),
        "the loaded component was asked every plan the native decisions were"
    );
    assert_eq!(
        loaded_seen.accepts.load(Ordering::SeqCst),
        seen.accepts.load(Ordering::SeqCst),
        "the loaded component was asked every accept the native decisions were"
    );
    assert_one_instance_per_call(&loaded_seen, &decisions);
    seen
}

fn done_counts(steps: u32) -> Counts {
    Counts {
        steps,
        done: steps,
        ..Counts::default()
    }
}

// ------------------------------------------------------------------ nested fan-outs

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_inside_pipeline_stages() {
    within_deadline("parallel_inside_pipeline_stages", async {
        let script = r#"
let out = pipeline(["x", "y"],
    |item| {
        log("stage one " + item);
        parallel([|| agent("left " + item).value, || agent("right " + item).value])
    },
    |pair| {
        log("stage two");
        pair[0] + " | " + pair[1]
    });
out
"#;
        let seen = both_ways(
            script,
            Value::Null,
            RunOutcome::Completed,
            json!(["did: left x | did: right x", "did: left y | did: right y"]),
            done_counts(4),
        )
        .await;
        assert_eq!(seen.plans.load(Ordering::SeqCst), 4);
        assert_eq!(seen.accepts.load(Ordering::SeqCst), 4);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn pipeline_inside_parallel_thunks() {
    within_deadline("pipeline_inside_parallel_thunks", async {
        let script = r#"
parallel([
    || pipeline(["a", "b"], |x| agent("first " + x).value, |v| agent("then " + v).value),
    || pipeline(["c"], |x| {
        log("inner " + x);
        agent("first " + x).value
    }),
])
"#;
        both_ways(
            script,
            Value::Null,
            RunOutcome::Completed,
            json!([
                ["did: then did: first a", "did: then did: first b"],
                ["did: first c"]
            ]),
            done_counts(5),
        )
        .await;
    })
    .await;
}

/// Every kind of decision inside nested fan-outs: a fallback hop, the repair turn, and a
/// capped link, with the envelopes' chains and attempts.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn fallback_repair_and_cap_inside_nested_fan_outs() {
    within_deadline("fallback_repair_and_cap_inside_nested_fan_outs", async {
        let script = r#"
phase("fan out");
let r = parallel([
    || pipeline(["route a", "nudge b"], |x| agent(x)),
    || agent("route c"),
]);
let once = agent("judge once", #{role: "judge"});
let twice = agent("judge twice", #{role: "judge"});
[
    r[0][0].status, r[0][0].models, r[0][0].attempts,
    r[0][1].value, r[0][1].attempts,
    r[1].value, r[1].attempts,
    once.value, twice.status, twice.error, twice.models,
]
"#;
        let chain = json!([
            {"model": "env/head", "moved_on": "route_failed"},
            {"model": "env/next", "moved_on": null}
        ]);
        both_ways(
            script,
            Value::Null,
            RunOutcome::CompletedWithIssues,
            json!([
                "done", chain, 2,
                "repaired: nudge b", 2,
                "did: route c", 2,
                "did: judge once", "failed", "quota_exceeded: fable used=1 limit=1",
                [{"model": "env/fable", "moved_on": null}],
            ]),
            Counts {
                steps: 5,
                done: 4,
                failed: 1,
                capped: 1,
                fell_back: 2,
                ..Counts::default()
            },
        )
        .await;
    })
    .await;
}

// ------------------------------------------------------------------ shared captures

/// The audit script's shape: a script `fn` curried with its inputs, one per thunk.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn curried_thunks_share_their_captures() {
    within_deadline("curried_thunks_share_their_captures", async {
        let script = r#"
let a = args;
fn vote(preamble, lens, facts) {
    agent(preamble + " " + lens + " " + facts).value
}
parallel([
    Fn("vote").curry(a.preamble, "rule", a.facts),
    Fn("vote").curry(a.preamble, "code", a.facts),
])
"#;
        both_ways(
            script,
            json!({"preamble": "vote", "facts": "# Facts"}),
            RunOutcome::Completed,
            json!(["did: vote rule # Facts", "did: vote code # Facts"]),
            done_counts(2),
        )
        .await;
    })
    .await;
}

/// A value several thunks capture and only read.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_value_captured_by_several_thunks() {
    within_deadline("a_value_captured_by_several_thunks", async {
        let script = r#"
let prefix = "shared";
parallel([|| agent(prefix + " 1").value, || agent(prefix + " 2").value, || prefix])
"#;
        both_ways(
            script,
            Value::Null,
            RunOutcome::Completed,
            json!(["did: shared 1", "did: shared 2", "shared"]),
            done_counts(2),
        )
        .await;
    })
    .await;
}

/// A closure several thunks method-call is still the data race the engine reports: the
/// split changes nothing about closure semantics. One vote parks in the runner (a model call
/// takes a while) while its sibling tries the closure the parked one holds.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_closure_captured_by_several_thunks_still_races() {
    within_deadline("a_closure_captured_by_several_thunks_still_races", async {
        let script = r#"
let refute = |lens| agent("vote " + lens);
let votes = parallel([|| refute.call("rule"), || refute.call("code")]);
votes.len()
"#;
        for way in ["native", "recorded", "loaded"] {
            let seen = Arc::new(Seen::default());
            let decisions = (way == "loaded").then(loaded_decisions);
            let seam = match (way, &decisions) {
                ("native", _) => Seam::Native,
                (_, Some(decisions)) => {
                    Seam::Recorded(Recording::loaded(seen.clone(), decisions.clone()))
                }
                _ => Seam::Recorded(Recording::new(seen.clone())),
            };
            let service = Service::new(&seen, seam);
            let rule = service.runner.park("vote rule");
            let code = service.runner.park("vote code");
            let id = service.start(script, Value::Null).await;
            // Whichever vote parks first holds the closure; its sibling either parks too or
            // fails on the race. Then both are let go.
            let other = tokio::select! {
                _ = rule.reached.notified() => &code,
                _ = code.reached.notified() => &rule,
            };
            tokio::select! {
                _ = other.reached.notified() => {}
                _ = service.observer.thunk_failed.notified() => {}
            }
            rule.release.notify_one();
            code.release.notify_one();
            let report = service.wait(&id).await;
            assert_eq!(report.outcome, RunOutcome::Failed, "{report:?}");
            let error = report.error.clone().unwrap_or_default();
            assert!(error.contains("Data race"), "{way}: {error}");
            seen.assert_no_reentry();
            if let Some(decisions) = &decisions {
                assert_one_instance_per_call(&seen, decisions);
            }
        }
    })
    .await;
}

// ------------------------------------------------------------------ no Store re-entry

/// Two thunks' first plans meet at a barrier inside their decision calls, so two calls of
/// the one seam are live at once on two threads — and still none is nested on its thread,
/// and nothing else runs inside one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn concurrent_decision_calls_never_nest_on_a_thread() {
    within_deadline("concurrent_decision_calls_never_nest_on_a_thread", async {
        let script = r#"
parallel([
    || { log("thunk a"); agent("a", #{label: "a"}).value },
    || { log("thunk b"); agent("b", #{label: "b"}).value },
])
"#;
        // First over the native decisions, then over the loaded component, which then
        // serves two calls at once on two threads.
        for loaded in [None, Some(loaded_decisions())] {
            let seen = Arc::new(Seen::default());
            let mut recording = match &loaded {
                Some(decisions) => Recording::loaded(seen.clone(), decisions.clone()),
                None => Recording::new(seen.clone()),
            };
            recording.meet = Some(Meet {
                labels: ["a", "b"],
                barrier: Barrier::new(2),
            });
            let service = Service::new(&seen, Seam::Recorded(recording));
            let id = service.start(script, Value::Null).await;
            let report = service.wait(&id).await;
            assert_eq!(report.outcome, RunOutcome::Completed, "{report:?}");
            assert_eq!(report.value, json!(["did: a", "did: b"]));
            assert_eq!(
                seen.max_live.load(Ordering::SeqCst),
                2,
                "the two first plans were live at once"
            );
            assert_eq!(seen.plans.load(Ordering::SeqCst), 2);
            assert_eq!(seen.accepts.load(Ordering::SeqCst), 2);
            seen.assert_no_reentry();
            if let Some(decisions) = &loaded {
                assert_one_instance_per_call(&seen, decisions);
            }
        }
    })
    .await;
}
