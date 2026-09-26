//! The reload suite (S5.7, issue #331; ADR-0084 §3, ADR-0078 §4): `/modules reload`
//! replaces the session's module assembly, its authorization policy included, between
//! complete turns.
//!
//! Every generation's catalog is loaded again from a release through S1.4's loader
//! (`load_locked_modules` and `register_modules`, the fixture package selected by a
//! `modules.lock`), its assembly is built with `p1_assembly::assemble`, and its policy is
//! the host's `AskBridge` over a built `p1/policy/*` component. The candidate is installed
//! through the host's `install_candidate`, which is `Agent::reconfigure` with the policy
//! plus the generation swap. Busy sessions queue on the host's `ReloadQueue`.
//!
//! The cases:
//! - *busy turn*: [`busy_turn`];
//! - *commit failure*: [`commit_failure`];
//! - *policy change*: [`policy_change`];
//! - *old generation*: [`old_generation`];
//! - a validation failure against the current history: [`validation_failure`].
//!
//! Synchronization is explicit (a tool that parks until the test releases it, a child
//! waited for through its worker service); nothing sleeps and nothing reaches a network.
//! The built packages come from `scripts/build-modules.sh`; a missing build fails the case
//! with how to build it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Weak};

use p1_assembly::{
    Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolServices, ToolSpec,
    assemble,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, CommitError,
    CommitSink, Effect, Item, JournalRecord, ModelOptions, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RecordBody, RouteDescription, Tool,
    ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolOutcome, ToolStatus, TurnEnd,
};
use p1_core::{Agent, AgentParts, ReconfigureError};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_host::policy::{
    AskBridge, Asker, OperatorAnswer, PolicyId, USER_DENY, Verdict as HostVerdict, VerdictSource,
};
use p1_host::run::{
    Candidate, CandidateParts, Generation, Generations, ReloadQueue, ReloadRequested,
    install_candidate,
};
use p1_module_runtime::{
    ExecutionLimits, Loader, ProcessService, ReleaseManifest, Services, Verdict as WasmVerdict,
    WasmAuthorizationPolicy,
};
use p1_module_tests::{
    FIXTURE_NAME, FakeProcesses, Release, fake_processes, lock_text, within_deadline,
};
use p1_testkit::{
    FakeTool, PassthroughContext, RecordingEvents, RecordingJournal, ScriptedProvider, Step,
    json_call, text_response, tool_call_response,
};
use p1_workers::{
    AgentFactory, ChildAgent, ChildSpec, ChildStatus, InProcessWorkers, WorkerReport, WorkerService,
};
use tokio::sync::{Notify, Semaphore};

/// The parent's provider key in every generation's catalog.
const PARENT: &str = "parent";
/// The children's provider key.
const CHILD: &str = "child";
/// The module name the lock gives the fixture package.
const MODULE: &str = "fixture";

/// The built restrictive authorization policy (S5.3).
const ASK: (&str, &str) = ("p1-module-policy-ask", "p1/policy/ask");
/// The built full-access authorization policy (S5.3), the shipped default.
const FULL_ACCESS: (&str, &str) = ("p1-module-policy-full-access", "p1/policy/full-access");

// ------------------------------------------------------------------ the built policies

/// Where `scripts/build-modules.sh` publishes the built packages.
fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// A release manifest entry for the built `package`, as the build published it.
fn entry(package: (&str, &str)) -> Value {
    let path = built()
        .join(package.0)
        .join(format!("{}.manifest.json", package.0));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the build output {} is missing ({error}): run scripts/build-modules.sh first",
            path.display()
        )
    });
    let manifest: Value = serde_json::from_str(&text).expect("the package manifest is JSON");
    json!({
        "name": manifest["name"],
        "digest": manifest["digest"],
        "path": format!("{0}/{0}.wasm", package.0),
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": manifest["capabilities"],
        "variant": manifest["variant"],
    })
}

/// The component's verdicts in the host's shape, keyed by its package name and digest.
struct ComponentSource(WasmAuthorizationPolicy);

impl VerdictSource for ComponentSource {
    fn policy(&self) -> PolicyId {
        PolicyId {
            package: self.0.name().to_owned(),
            digest: self.0.digest().to_string(),
        }
    }

    fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, HostVerdict> {
        Box::pin(async move {
            match self.0.verdict(request).await {
                WasmVerdict::Permit => HostVerdict::Permit,
                WasmVerdict::Deny(reason) => HostVerdict::Deny(reason),
                WasmVerdict::Ask => HostVerdict::Ask,
            }
        })
    }
}

/// The operator: scripted answers in order, every question counted.
#[derive(Default)]
struct Operator {
    answers: Mutex<VecDeque<OperatorAnswer>>,
    asked: AtomicUsize,
}

impl Operator {
    fn will_answer(&self, answer: OperatorAnswer) {
        self.answers.lock().unwrap().push_back(answer);
    }

    fn asked(&self) -> usize {
        self.asked.load(Ordering::SeqCst)
    }
}

impl Asker for Operator {
    fn ask<'a>(
        &'a self,
        _request: AuthorizationRequest<'a>,
    ) -> BoxFuture<'a, Option<OperatorAnswer>> {
        self.asked.fetch_add(1, Ordering::SeqCst);
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move { answer })
    }
}

/// One generation's policy: the built `package` loaded from its release, behind the host's
/// ask bridge, asking the session's one operator.
fn policy(package: (&str, &str), operator: &Arc<Operator>) -> Arc<dyn AuthorizationPolicy> {
    let manifest = json!({ "format": "p1-release-manifest/1", "components": [entry(package)] });
    let release = ReleaseManifest::parse(&manifest.to_string()).expect("release manifest");
    let loader = Loader::new(release, built()).expect("loader");
    let module = loader.load(package.1).expect("the built policy loads");
    let component = WasmAuthorizationPolicy::new(&module, ExecutionLimits::default())
        .expect("the policy adapter builds");
    Arc::new(AskBridge::with_asker(
        Arc::new(ComponentSource(component)),
        false,
        operator.clone(),
        CancellationToken::new(),
    ))
}

// ------------------------------------------------------------------ tools and journal

/// A tool that announces its start and then parks until the test releases it: the turn
/// that called it is busy exactly as long as the test says.
#[derive(Clone)]
struct GatedTool {
    inner: FakeTool,
    entered: Arc<Notify>,
    release: Arc<Semaphore>,
}

impl GatedTool {
    fn new(name: &str) -> Self {
        Self {
            inner: FakeTool::new(name),
            entered: Arc::new(Notify::new()),
            release: Arc::new(Semaphore::new(0)),
        }
    }

    fn open(&self) {
        self.release.add_permits(1);
    }
}

impl Tool for GatedTool {
    fn declaration(&self) -> &ToolDeclaration {
        self.inner.declaration()
    }

    fn identity(&self) -> &ToolIdentity {
        self.inner.identity()
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        self.inner.effect(call)
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.entered.notify_one();
            let _permit = self.release.acquire().await.expect("the gate stays open");
            self.inner.execute(call, context).await
        })
    }
}

/// A journal that refuses every `Environment` record while `refuse` is set.
#[derive(Default)]
struct RefusingJournal {
    inner: RecordingJournal,
    refuse: AtomicBool,
}

impl CommitSink for RefusingJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        Box::pin(async move {
            if self.refuse.load(Ordering::SeqCst)
                && matches!(record.body, RecordBody::Environment { .. })
            {
                return Err(CommitError("the journal refuses the environment".into()));
            }
            self.inner.commit(record).await
        })
    }
}

fn environments(records: &[JournalRecord]) -> usize {
    records
        .iter()
        .filter(|record| matches!(record.body, RecordBody::Environment { .. }))
        .count()
}

/// The model-visible items a journal's records add up to: each user input and each
/// completed answer (the cases here use no tool before the history is checked).
fn before_history_items(records: &[JournalRecord]) -> usize {
    records
        .iter()
        .filter(|record| {
            matches!(
                record.body,
                RecordBody::UserInput { .. } | RecordBody::AssistantCompleted { .. }
            )
        })
        .count()
}

fn assert_dense(records: &[JournalRecord]) {
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "{records:#?}");
    }
}

/// The status of every tool result the journal holds, in order.
fn tool_results(records: &[JournalRecord]) -> Vec<(String, ToolStatus, String)> {
    records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::ToolFinished { result } => {
                Some((result.name.clone(), result.status, result.content.clone()))
            }
            _ => None,
        })
        .collect()
}

// ------------------------------------------------------------------ one generation

/// What one generation's catalog serves: the parent's and the children's scripted
/// providers, and the `write` and `gate` tools whose calls the case counts.
struct Serves {
    parent: ScriptedProvider,
    child: ScriptedProvider,
    write: FakeTool,
    gate: GatedTool,
    /// The parent's provider cannot replay a history that holds an answer.
    refuses_replay: bool,
}

impl Serves {
    fn new(parent: Vec<Step>) -> Self {
        Self::with_child(parent, Vec::new())
    }

    fn with_child(parent: Vec<Step>, child: Vec<Step>) -> Self {
        Self {
            parent: ScriptedProvider::new(parent),
            child: ScriptedProvider::new(child),
            write: FakeTool::new("write").with_effect(Effect::WritesFiles),
            gate: GatedTool::new("gate"),
            refuses_replay: false,
        }
    }
}

/// A provider that cannot carry a history holding an assistant answer: it validates the
/// empty history the assembly checks, and refuses to replay the session's.
struct ReplayRefusing(ScriptedProvider);

impl Provider for ReplayRefusing {
    fn describe(&self) -> RouteDescription {
        self.0.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.0.validate(request)?;
        if request
            .history
            .iter()
            .any(|item| matches!(item, Item::Assistant(_)))
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "cannot replay this history",
            ));
        }
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.0.stream(request, cancel)
    }
}

fn environment(provider: &str, modules: &[&str]) -> EnvironmentFile {
    EnvironmentFile {
        name: "reload-test".into(),
        family: "test".into(),
        provider: provider.into(),
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

/// The session's own environment: the fixture package among compiled-in stand-ins.
fn parent_environment() -> EnvironmentFile {
    environment(PARENT, &["read", "write", "gate", MODULE])
}

fn child_environment() -> EnvironmentFile {
    environment(CHILD, &["write", "gate"])
}

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
    }
}

/// The session: the release every generation is loaded from, the lock selecting the
/// fixture, the one operator every policy asks, and the generations.
struct Session {
    release: Release,
    lock: ModulesLock,
    services: ModuleServices,
    instantiations: Arc<AtomicUsize>,
    _processes: FakeProcesses,
    workspace: tempfile::TempDir,
    operator: Arc<Operator>,
}

impl Session {
    fn new() -> Self {
        let release = Release::with_fixture();
        let lock = ModulesLock::parse(
            &release.root().join("modules.lock"),
            &lock_text(MODULE, &release.fixture_entry(FIXTURE_NAME)),
        )
        .expect("fixture lock");
        let instantiations = Arc::new(AtomicUsize::new(0));
        let (process, processes) = fake_processes();
        let process: Arc<dyn ProcessService> = process;
        let counted = instantiations.clone();
        let services: ModuleServices = Arc::new(move |_: &ToolServices| {
            counted.fetch_add(1, Ordering::SeqCst);
            Services {
                process: Some(process.clone()),
                ..Services::default()
            }
        });
        Self {
            release,
            lock,
            services,
            instantiations,
            _processes: processes,
            workspace: tempfile::tempdir().expect("workspace"),
            operator: Arc::new(Operator::default()),
        }
    }

    /// How many module tool instances the generations built.
    fn instantiations(&self) -> usize {
        self.instantiations.load(Ordering::SeqCst)
    }

    /// A generation's catalog, loaded again from the release: the stand-ins `serves`
    /// holds, then every package the lock selects, verified by the loader.
    fn load_catalog(&self, serves: &Serves) -> Catalog {
        let mut catalog = Catalog::new();
        let parent = serves.parent.clone();
        let refuses_replay = serves.refuses_replay;
        catalog.provider(
            PARENT,
            Box::new(move |_: &ProviderSpec| {
                Ok(if refuses_replay {
                    Arc::new(ReplayRefusing(parent.clone())) as Arc<dyn Provider>
                } else {
                    Arc::new(parent.clone()) as Arc<dyn Provider>
                })
            }),
        );
        let child = serves.child.clone();
        catalog.provider(
            CHILD,
            Box::new(move |_: &ProviderSpec| Ok(Arc::new(child.clone()) as Arc<dyn Provider>)),
        );
        let read = FakeTool::new("read");
        let write = serves.write.clone();
        let gate = serves.gate.clone();
        catalog.tool(
            "read",
            Box::new(move |_: &ToolSpec, _: &ToolServices| {
                Ok(Arc::new(read.clone()) as Arc<dyn Tool>)
            }),
        );
        catalog.tool(
            "write",
            Box::new(move |_: &ToolSpec, _: &ToolServices| {
                Ok(Arc::new(write.clone()) as Arc<dyn Tool>)
            }),
        );
        catalog.tool(
            "gate",
            Box::new(move |_: &ToolSpec, _: &ToolServices| {
                Ok(Arc::new(gate.clone()) as Arc<dyn Tool>)
            }),
        );
        let packages = load_locked_modules(&self.lock, &self.release.manifest_file())
            .expect("the release loads");
        register_modules(&mut catalog, packages, self.services.clone()).expect("registration");
        catalog
    }

    /// The complete candidate: the catalog loaded again, the policy, and the session's
    /// assembly on that catalog.
    fn candidate(&self, serves: &Serves, package: (&str, &str)) -> Candidate {
        self.candidate_with(serves, policy(package, &self.operator))
    }

    fn candidate_with(
        &self,
        serves: &Serves,
        authorization: Arc<dyn AuthorizationPolicy>,
    ) -> Candidate {
        let catalog = Arc::new(self.load_catalog(serves));
        let assembled = assemble(
            &catalog,
            &parent_environment(),
            self.workspace.path(),
            &substitutions(),
        )
        .expect("the session's environment assembles");
        Candidate {
            catalog,
            authorization,
            parts: CandidateParts {
                provider: assembled.provider,
                tools: assembled.tools,
                system_prompt: assembled.system_prompt,
                options: assembled.options,
                context: Arc::new(PassthroughContext),
            },
        }
    }

    /// The parent agent on generation 0, as the start path builds it.
    fn start(&self, first: Candidate, journal: Arc<dyn CommitSink>) -> (Agent, Arc<Generations>) {
        let Candidate {
            catalog,
            authorization,
            parts,
        } = first;
        let generations = Arc::new(Generations::new(catalog, authorization.clone()));
        let agent = Agent::new(AgentParts {
            provider: parts.provider,
            tools: parts.tools,
            system_prompt: parts.system_prompt,
            options: parts.options,
            context: parts.context,
            authorization,
            journal,
            events: Arc::new(RecordingEvents::new()),
        })
        .expect("the parent builds");
        (agent, generations)
    }
}

async fn turn(agent: &mut Agent, input: &str) {
    let end = agent.run_turn(input.into(), CancellationToken::new()).await;
    assert!(matches!(end, TurnEnd::Completed { .. }), "{end:?}");
}

fn write_call(id: &str) -> Step {
    tool_call_response(vec![json_call(id, "write", "{}")])
}

// ------------------------------------------------------------------ the cases

/// *busy turn*: a reload requested while a turn's tool call is still running is
/// reported pending and changes nothing until the turn and its call settled; the loop
/// then applies it at the boundary, and the next turn runs on the new assembly.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn busy_turn() {
    within_deadline("busy turn", async {
        let session = Session::new();
        let first = Serves::new(vec![
            tool_call_response(vec![json_call("c1", "gate", "{}")]),
            text_response("first done"),
        ]);
        let second = Serves::new(vec![text_response("second")]);
        let journal = Arc::new(RecordingJournal::new());
        let (mut agent, generations) =
            session.start(session.candidate(&first, FULL_ACCESS), journal.clone());
        assert_eq!(session.instantiations(), 1);
        let queue = ReloadQueue::default();

        let ((), requested) = tokio::join!(turn(&mut agent, "one"), async {
            first.gate.entered.notified().await;
            // The turn is parked inside its tool call: the session is busy.
            let requested = queue.request(true);
            assert_eq!(
                generations.current().number(),
                0,
                "nothing applies mid-turn"
            );
            first.gate.open();
            requested
        });
        assert_eq!(requested, ReloadRequested::Pending);
        // The turn ended, its tool call settled, and the old assembly answered it all.
        let settled = journal.records();
        assert_eq!(environments(&settled), 1);
        assert_eq!(
            tool_results(&settled),
            [("gate".to_owned(), ToolStatus::Ok, "gate ok".to_owned())]
        );
        assert_eq!(first.parent.requests().len(), 2);

        // The boundary: the loop takes the queued request once and applies it.
        assert!(queue.take());
        assert!(!queue.take(), "one request applies once");
        let generation = install_candidate(
            &generations,
            &mut agent,
            session.candidate(&second, FULL_ACCESS),
        )
        .await
        .expect("the reload installs");
        assert_eq!(generation.number(), 1);
        assert_eq!(generations.current().number(), 1);
        assert_eq!(
            session.instantiations(),
            2,
            "the candidate's module was loaded and built again"
        );
        let records = journal.records();
        assert_eq!(environments(&records), 2);
        assert!(
            matches!(
                records.last().map(|record| &record.body),
                Some(RecordBody::Environment { .. })
            ),
            "the new environment follows the settled turn: {records:#?}"
        );

        turn(&mut agent, "two").await;
        assert_eq!(
            second.parent.requests().len(),
            1,
            "the next turn runs on the new assembly"
        );
        assert_eq!(
            first.parent.requests().len(),
            2,
            "the old assembly is not asked again"
        );
        let records = journal.records();
        assert_dense(&records);
        assert_eq!(
            environments(&records),
            2,
            "one record for the reload, none lazily after it"
        );
    })
    .await;
}

/// *commit failure*: a journal that refuses the candidate's `Environment` record leaves
/// the old assembly answering, its policy included; the error is reported, the
/// generation stays, and no sequence number is used.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn commit_failure() {
    within_deadline("commit failure", async {
        let session = Session::new();
        let first = Serves::new(vec![
            text_response("first"),
            write_call("c1"),
            text_response("written"),
        ]);
        let second = Serves::new(Vec::new());
        let journal = Arc::new(RefusingJournal::default());
        let (mut agent, generations) =
            session.start(session.candidate(&first, FULL_ACCESS), journal.clone());
        turn(&mut agent, "one").await;
        let before = journal.inner.records();

        journal.refuse.store(true, Ordering::SeqCst);
        let error = install_candidate(&generations, &mut agent, session.candidate(&second, ASK))
            .await
            .expect_err("the refused commit fails the reload");
        assert!(
            matches!(error, ReconfigureError::CommitFailed(_)),
            "{error:?}"
        );
        assert!(
            error.to_string().contains("could not be committed"),
            "{error}"
        );
        journal.refuse.store(false, Ordering::SeqCst);
        assert_eq!(generations.current().number(), 0);
        assert_eq!(journal.inner.records(), before, "nothing was written");

        // The old assembly answers, and its policy decides: full access writes without
        // asking, where the candidate's `p1/policy/ask` would have asked.
        turn(&mut agent, "two").await;
        assert_eq!(first.parent.requests().len(), 3);
        assert!(second.parent.requests().is_empty());
        assert_eq!(first.write.calls().len(), 1);
        assert_eq!(session.operator.asked(), 0);
        let records = journal.inner.records();
        assert_dense(&records);
        assert_eq!(
            records[before.len()].seq,
            before.len() as u64,
            "no sequence number used"
        );
        assert_eq!(environments(&records), 1);
    })
    .await;
}

/// *policy change*: `p1/policy/full-access` → `p1/policy/ask` → `p1/policy/full-access`
/// → `p1/policy/ask`. The next call after each reload is decided by the new policy, and a
/// grant remembered under an earlier policy does not carry over to a later one.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn policy_change() {
    within_deadline("policy change", async {
        let session = Session::new();
        let serves = || Serves::new(vec![write_call("c1"), text_response("done")]);
        let full = serves();
        let journal = Arc::new(RecordingJournal::new());
        let (mut agent, generations) =
            session.start(session.candidate(&full, FULL_ACCESS), journal.clone());
        turn(&mut agent, "full access").await;
        assert_eq!(full.write.calls().len(), 1);
        assert_eq!(session.operator.asked(), 0, "full access asks nothing");

        // To `ask`: the write asks, and the operator grants it for good.
        let ask = Serves::new(vec![
            write_call("c2"),
            text_response("done"),
            write_call("c3"),
            text_response("done"),
        ]);
        install_candidate(&generations, &mut agent, session.candidate(&ask, ASK))
            .await
            .expect("to ask");
        session.operator.will_answer(OperatorAnswer::Always);
        turn(&mut agent, "ask").await;
        assert_eq!(session.operator.asked(), 1);
        assert_eq!(ask.write.calls().len(), 1);
        turn(&mut agent, "ask again").await;
        assert_eq!(
            session.operator.asked(),
            1,
            "the grant answers under this policy"
        );
        assert_eq!(ask.write.calls().len(), 2);

        // Back to full access: permitted, nothing asked.
        let back = serves();
        install_candidate(
            &generations,
            &mut agent,
            session.candidate(&back, FULL_ACCESS),
        )
        .await
        .expect("back to full access");
        turn(&mut agent, "full access again").await;
        assert_eq!(back.write.calls().len(), 1);
        assert_eq!(session.operator.asked(), 1);

        // And to `ask` again: the grant given under the earlier `ask` generation does not
        // carry over; the operator is asked again and this time refuses.
        let again = serves();
        install_candidate(&generations, &mut agent, session.candidate(&again, ASK))
            .await
            .expect("to ask again");
        session.operator.will_answer(OperatorAnswer::No);
        turn(&mut agent, "ask once more").await;
        assert_eq!(session.operator.asked(), 2, "a reloaded policy asks again");
        assert!(
            again.write.calls().is_empty(),
            "the refused write never ran"
        );
        let results = tool_results(&journal.records());
        assert_eq!(
            results.last(),
            Some(&("write".to_owned(), ToolStatus::Denied, USER_DENY.to_owned())),
            "{results:?}"
        );

        assert_eq!(generations.current().number(), 3);
        let records = journal.records();
        assert_dense(&records);
        assert_eq!(environments(&records), 4, "one record per generation");
    })
    .await;
}

/// The children's factory: every child pins the generation current when it starts and
/// builds its agent from that generation's catalog and policy, as the host's child
/// builder does; its description names the generation.
fn child_factory(generations: Arc<Generations>, workspace: PathBuf) -> AgentFactory {
    Arc::new(move |_spec: &ChildSpec| {
        let generation: Arc<Generation> = generations.current();
        let assembled = assemble(
            generation.catalog(),
            &child_environment(),
            &workspace,
            &substitutions(),
        )
        .map_err(|error| error.to_string())?;
        let agent = Agent::new(AgentParts {
            provider: assembled.provider,
            tools: assembled.tools,
            system_prompt: assembled.system_prompt,
            options: assembled.options,
            context: Arc::new(PassthroughContext),
            authorization: generation.authorization(),
            journal: Arc::new(RecordingJournal::new()),
            events: Arc::new(RecordingEvents::new()),
        })
        .map_err(|error| error.to_string())?;
        Ok(ChildAgent {
            agent,
            description: format!("generation {}", generation.number()),
            report: Arc::new(WorkerReport::default),
            regrant: None,
        })
    })
}

fn spec() -> ChildSpec {
    ChildSpec {
        environment: "child".into(),
        task: "do it".into(),
        tools: Vec::new(),
        workspace: None,
    }
}

async fn finished(service: &InProcessWorkers, id: &p1_workers::ChildId) -> String {
    match service.wait(id, CancellationToken::new()).await {
        Ok(ChildStatus::Finished(result)) => result.final_text,
        other => panic!("the child did not finish: {other:?}"),
    }
}

/// *old generation*: a child started before the reload keeps running on its pinned
/// generation — its provider and its policy — while one started after takes the new
/// one; the old generation is dropped when its last user ends.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn old_generation() {
    within_deadline("old generation", async {
        let session = Session::new();
        // Generation 0: full access. Its child parks in `gate`, then writes.
        let old = Serves::with_child(
            vec![text_response("parent")],
            vec![
                tool_call_response(vec![json_call("g1", "gate", "{}")]),
                write_call("w1"),
                text_response("old child done"),
            ],
        );
        // Generation 1: ask. Its child writes, which the operator refuses.
        let new = Serves::with_child(
            vec![text_response("parent again")],
            vec![write_call("w2"), text_response("new child done")],
        );
        let journal = Arc::new(RecordingJournal::new());
        let (mut agent, generations) =
            session.start(session.candidate(&old, FULL_ACCESS), journal.clone());
        let old_generation: Weak<Generation> = Arc::downgrade(&generations.current());
        let old_policy: Weak<dyn AuthorizationPolicy> =
            Arc::downgrade(&generations.current().authorization());
        let service = InProcessWorkers::new(
            child_factory(generations.clone(), session.workspace.path().to_owned()),
            4,
        );
        turn(&mut agent, "one").await;

        let before = service.start(spec()).await.expect("the first child starts");
        old.gate.entered.notified().await;
        assert_eq!(service.describe(&before).await.unwrap(), "generation 0");

        // The reload, while that child is inside its tool call.
        install_candidate(&generations, &mut agent, session.candidate(&new, ASK))
            .await
            .expect("the reload installs");
        assert_eq!(generations.current().number(), 1);
        assert!(
            old_generation.upgrade().is_none(),
            "no assembly pins the generation record once it is replaced"
        );
        assert!(
            old_policy.upgrade().is_some(),
            "the running child still holds generation 0's policy"
        );

        let after = service
            .start(spec())
            .await
            .expect("the second child starts");
        assert_eq!(service.describe(&after).await.unwrap(), "generation 1");
        session.operator.will_answer(OperatorAnswer::No);
        assert_eq!(finished(&service, &after).await, "new child done");
        assert_eq!(
            session.operator.asked(),
            1,
            "the new child's write is decided by `ask`"
        );
        assert!(new.write.calls().is_empty());

        // The first child resumes on generation 0: its provider answers, and its write
        // is decided by full access, which asks nothing.
        old.gate.open();
        assert_eq!(finished(&service, &before).await, "old child done");
        assert_eq!(old.write.calls().len(), 1);
        assert_eq!(session.operator.asked(), 1);
        assert_eq!(old.child.requests().len(), 3);
        assert_eq!(new.child.requests().len(), 2);

        // The parent's next turn is on generation 1.
        turn(&mut agent, "two").await;
        assert_eq!(new.parent.requests().len(), 1);

        // The last user of generation 0 ends: its policy goes with it.
        service.shutdown().await;
        assert!(
            old_policy.upgrade().is_none(),
            "generation 0 is dropped once its last child ended"
        );
    })
    .await;
}

/// A candidate whose provider refuses to replay the current history changes nothing: no
/// record, no generation, the old assembly and policy answering.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn validation_failure() {
    within_deadline("validation failure", async {
        let session = Session::new();
        let first = Serves::new(vec![
            text_response("first"),
            write_call("c1"),
            text_response("written"),
        ]);
        let mut refusing = Serves::new(Vec::new());
        refusing.refuses_replay = true;
        let journal = Arc::new(RecordingJournal::new());
        let (mut agent, generations) =
            session.start(session.candidate(&first, FULL_ACCESS), journal.clone());
        turn(&mut agent, "one").await;
        let before = journal.records();

        let error = install_candidate(&generations, &mut agent, session.candidate(&refusing, ASK))
            .await
            .expect_err("the candidate is refused");
        assert!(matches!(error, ReconfigureError::Rejected(_)), "{error:?}");
        assert!(
            error.to_string().contains("cannot replay this history"),
            "{error}"
        );
        let validated = refusing.parent.validated();
        assert!(
            validated
                .last()
                .is_some_and(|request| request.history.len() == before_history_items(&before)),
            "validated against the current history: {validated:#?}"
        );
        assert_eq!(journal.records(), before);
        assert_eq!(generations.current().number(), 0);

        turn(&mut agent, "two").await;
        assert_eq!(first.parent.requests().len(), 3);
        assert!(refusing.parent.requests().is_empty());
        assert_eq!(first.write.calls().len(), 1);
        assert_eq!(session.operator.asked(), 0, "the old policy still decides");
        assert_dense(&journal.records());
    })
    .await;
}
