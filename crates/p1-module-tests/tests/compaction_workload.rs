//! The compaction workload's handle-lifetime cases (S5.9, issue #333; ADR-0071,
//! DECISIONS.md D22, epic #206).
//!
//! PLAN.md §11 risk 6: "handle lifetime across compaction, reload and reconnect
//! (critical)". The workload lives and dies on three handles, and these cases show each
//! side of their lifetime, against the repository's built components, without a clock:
//!
//! - the [`WasmContextPolicy`] instance and the [`SummaryService`] handle it holds:
//!   every `compact_now` call gets its own [`Store`], the call's answer leaves no
//!   [`Store`] behind, concurrent calls park their summaries without crossing, and the
//!   last holder's drop releases the service once the executor's wind-down is scheduled
//!   ([`compaction_stores_live_only_while_their_call_runs`]);
//! - the agent's generation, committed by [`install_candidate`] as one `Environment`
//!   record: a reload swaps the whole generation — context policy, authorization policy
//!   and the generation record — and the old generation's handles are dropped by that
//!   swap itself, never left to a background task
//!   ([`a_reload_drops_the_old_generation_and_its_handles`]);
//! - the websocket lease: a failed response drops its connection, a clean completion
//!   returns it for reuse, and a lease drop takes the connection it is using, so live
//!   connections never outlive the response they carry
//!   ([`three_reconnects_drop_each_connection_once_its_response_ends`]);
//! - and one case interleaves all three across three generation lifetimes
//!   ([`compaction_reload_and_reconnect_drop_every_handle`]).
//!
//! The counts are strong and weak reference counts over the exact allocation a handle
//! points at, so every claim is a fact about one object, not a proxy. Where a drop
//! happens on another task (the executor's wind-down, a summary task's own service
//! handle) the case yields — it never sleeps — until the count settles, and only then
//! asserts the settled number. Nothing here waits on wall-clock time.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock, Weak};
use std::time::Instant;

use p1_assembly::{
    Catalog, EnvironmentFile, ModulesLock, ProviderSpec, Substitutions, ToolServices, assemble,
};
use p1_context::DEFAULT_SUMMARY_OUTPUT_TOKENS;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AssistantBlock, AssistantItem, AuthorizationPolicy, AuthorizationRequest, BoxFuture,
    CancellationToken, Compaction, ContextError, ContextInput, ContextPolicy, Item, JournalRecord,
    ModelOptions, Origin, Prepared, Provider, ProviderError, ProviderRequest, ProviderStream,
    RecordBody, RouteDescription, StopReason,
};
use p1_core::{Agent, AgentParts};
use p1_host::catalog::modules::{ModuleServices, load_locked_modules, register_modules};
use p1_host::policy::{
    AskBridge, Asker, OperatorAnswer, PolicyId, Verdict as HostVerdict, VerdictSource,
};
use p1_host::run::{Candidate, CandidateParts, Generations, install_candidate};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ProcessService, ReleaseManifest, Services, SummaryError,
    SummaryRequest, SummaryResponse, SummaryService, Verdict as WasmVerdict,
    WasmAuthorizationPolicy, WasmContextPolicy,
};
use p1_module_tests::{
    FIXTURE_NAME, FakeProcesses, Release, fake_processes, lock_text, within_deadline,
};
use p1_provider_http::ws::{
    WsConnectError, WsConnection, WsConnector, WsError, WsHandshake, WsNext,
};
use p1_provider_http::ws_session::{WsAuthority, WsHead, WsLease, WsRead, WsSend, WsSession};
use p1_provider_http::{CredentialScheme, CredentialUse};
use p1_testkit::{RecordingEvents, RecordingJournal, ScriptedProvider};
use tokio::sync::{Notify, Semaphore};

/// The built package of the summarizing context policy (S5.2).
const CONTEXT: (&str, &str) = ("p1-module-context", "p1/context/summarizing");
/// The built full-access authorization policy (S5.3), the shipped default.
const FULL_ACCESS: (&str, &str) = ("p1-module-policy-full-access", "p1/policy/full-access");

/// The parent's provider key in every generation's catalog.
const PARENT: &str = "parent";

/// The module name the lock gives the fixture package.
const MODULE: &str = "fixture";

/// How many scheduler turns `eventually` gives another task's drop to land. Yields, not
/// time: a slow machine only makes each turn slower, never fewer.
const TURNS: usize = 10_000;

// ------------------------------------------------------------------ the built packages

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

/// The release manifest of the built `packages`, laid out as a release ships them.
fn release(packages: &[(&str, &str)]) -> ReleaseManifest {
    let components: Vec<Value> = packages.iter().map(|package| entry(*package)).collect();
    let manifest = json!({ "format": "p1-release-manifest/1", "components": components });
    ReleaseManifest::parse(&manifest.to_string()).expect("release manifest")
}

/// The context policy's component, compiled once per process: every generation's policy
/// is a new instance over the one component, PLAN §10's instance strategy.
fn context_module() -> &'static LoadedModule {
    static MODULE: OnceLock<LoadedModule> = OnceLock::new();
    MODULE.get_or_init(|| {
        let loader = Loader::new(release(&[CONTEXT]), built()).expect("loader");
        loader.load(CONTEXT.1).expect("the built package loads")
    })
}

/// `configure`'s settings: the component's own keys and the summary cap.
fn settings() -> String {
    json!({
        "window_tokens": 10_000u64,
        "output_headroom_tokens": 1_000u64,
        "summarize_at_tokens": 500u64,
        "keep_recent_tokens": 80u64,
        "user_verbatim_tokens": 100u64,
        "tool_result_excerpt_chars": 2_000u64,
        "summary_output_tokens": DEFAULT_SUMMARY_OUTPUT_TOKENS,
    })
    .to_string()
}

// ------------------------------------------------------------------ the summary service

/// The scripted summary service: counts its requests, answers every summary with one
/// completed end-turn, and can park the guest's `summary.summarize` import until the
/// case releases it — the one point where a call's [`Store`] is observably alive. The
/// `me` weak reference reads the strong count of the one allocation every holder
/// shares: the case's keeper, the policy's executor, each live call's [`Store`] and
/// each summary task's own handle.
struct ScriptedSummary {
    me: Weak<ScriptedSummary>,
    requests: AtomicUsize,
    entered: Notify,
    gate: Semaphore,
}

impl ScriptedSummary {
    /// A service whose summaries park until [`release`](Self::release) is called once
    /// per parked summary.
    fn parked() -> Arc<Self> {
        Self::new(0)
    }

    /// A service whose summaries never park.
    fn open() -> Arc<Self> {
        Self::new(1 << 20)
    }

    fn new(permits: usize) -> Arc<Self> {
        Arc::new_cyclic(|me| Self {
            me: me.clone(),
            requests: AtomicUsize::new(0),
            entered: Notify::new(),
            gate: Semaphore::new(permits),
        })
    }

    /// How many strong handles to this service are alive.
    fn live(&self) -> usize {
        self.me.strong_count()
    }

    /// The requests answered so far.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// Resolves once a summary reached the service (the guest is inside
    /// `summary.summarize`). `Notify` keeps the permit, so awaiting after the notify is
    /// still immediate.
    async fn entered(&self) {
        self.entered.notified().await;
    }

    /// Lets one parked summary finish.
    fn release(&self) {
        self.gate.add_permits(1);
    }
}

impl SummaryService for ScriptedSummary {
    fn summarize(
        &self,
        _request: SummaryRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>> {
        Box::pin(async move {
            self.requests.fetch_add(1, Ordering::SeqCst);
            self.entered.notify_one();
            // Parked here, the guest's call holds its Store; the summary task holds its
            // own handle to this service until the answer is on its way back. The
            // permit is forgotten, not dropped: dropping would hand it back to the gate
            // and the next summary would sail through without a release.
            let permit = self.gate.acquire().await.expect("the gate is never closed");
            permit.forget();
            Ok(SummaryResponse {
                text: "the earlier turns were summarized".to_owned(),
                // `end-turn` is not truncated: the engine accepts it as it is, so one
                // request per compaction, never the cap-doubling retry.
                stop: StopReason::EndTurn,
                usage: None,
            })
        })
    }
}

/// Waits, without sleeping, until `holds` or the turn budget runs out: a drop that
/// happens on another task (an executor's wind-down, a summary task's own handle)
/// needs only be scheduled, never timed.
async fn eventually(what: &str, holds: impl Fn() -> bool) {
    for _ in 0..TURNS {
        if holds() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!("{what}: never settled within {TURNS} scheduler turns");
}

/// The generation's context policy with a drop counter: the counter says when the last
/// holder dropped the policy, and the strong count says who is still holding it.
struct Counted {
    inner: WasmContextPolicy,
    dropped: Arc<AtomicUsize>,
}

impl Counted {
    /// The built policy over the one compiled component, with the generation's summary
    /// service.
    fn new(service: Arc<ScriptedSummary>) -> Arc<Self> {
        let inner = WasmContextPolicy::new(
            context_module(),
            &settings(),
            service,
            ExecutionLimits::default(),
        )
        .expect("the component accepts the settings");
        Arc::new(Self {
            inner,
            dropped: Arc::new(AtomicUsize::new(0)),
        })
    }

    /// The drop counter to assert on later.
    fn dropped(&self) -> Arc<AtomicUsize> {
        self.dropped.clone()
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        self.dropped.fetch_add(1, Ordering::SeqCst);
    }
}

impl ContextPolicy for Counted {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        self.inner.prepare(input)
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        self.inner.compact_now(input)
    }
}

/// A history with two units: the compaction summarizes the first, keeps the last
/// verbatim, and answers `Replaced` — a single-unit history is the no-op rule's (it
/// would answer `Unchanged`).
fn long_history() -> Vec<Item> {
    fn assistant(text: &str) -> Item {
        Item::Assistant(AssistantItem {
            origin: Origin {
                route: "test/route".to_owned(),
                model: "test-model".to_owned(),
            },
            blocks: vec![AssistantBlock::Text {
                text: text.to_owned(),
            }],
        })
    }
    vec![
        Item::User {
            text: "task: watch the handles across the workload".to_owned(),
        },
        assistant(&"first answer ".repeat(400)),
        Item::User {
            text: "constraint: keep the wall".to_owned(),
        },
        assistant(&"second answer ".repeat(400)),
    ]
}

/// One `compact_now` through `policy`, over a history that must be replaceable. The
/// returned facts are the plan's before and after estimates.
async fn compact(policy: &Counted, history: &[Item]) -> (u64, u64) {
    let cancel = CancellationToken::new();
    match policy
        .compact_now(ContextInput {
            history,
            last_usage: None,
            cancel: &cancel,
        })
        .await
        .expect("the compaction runs")
    {
        Compaction::Replaced {
            tokens_before,
            tokens_after,
            ..
        } => {
            assert!(
                tokens_after < tokens_before,
                "the replacement is smaller: {tokens_before} -> {tokens_after}"
            );
            (tokens_before, tokens_after)
        }
        Compaction::Unchanged { tokens } => {
            panic!("the two-unit history is replaceable, not the no-op rule's {tokens}")
        }
    }
}

// ------------------------------------------------------------------ the session stack

/// The operator: scripted answers in order.
#[derive(Default)]
struct Operator {
    answers: Mutex<VecDeque<OperatorAnswer>>,
}

impl Asker for Operator {
    fn ask<'a>(
        &'a self,
        _request: AuthorizationRequest<'a>,
    ) -> BoxFuture<'a, Option<OperatorAnswer>> {
        let answer = self.answers.lock().unwrap().pop_front();
        Box::pin(async move { answer })
    }
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

/// One generation's authorization policy: the built `package` loaded from its release,
/// behind the host's ask bridge, asking the session's one operator.
fn policy(package: (&str, &str), operator: &Arc<Operator>) -> Arc<dyn AuthorizationPolicy> {
    let manifest = json!({ "format": "p1-release-manifest/1", "components": [entry(package)] });
    let manifest = ReleaseManifest::parse(&manifest.to_string()).expect("release manifest");
    let loader = Loader::new(manifest, built()).expect("loader");
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

/// The provider every generation's catalog serves: an empty script, because no case
/// here runs a turn — the agent exists to hold and swap generations.
#[derive(Clone)]
struct Serves(ScriptedProvider);

impl Provider for Serves {
    fn describe(&self) -> RouteDescription {
        self.0.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.0.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.0.stream(request, cancel)
    }
}

/// The journal facts the reload cases assert: one `Environment` record per install, the
/// sequences dense from zero.
fn environments(records: &[JournalRecord]) -> usize {
    records
        .iter()
        .filter(|record| matches!(record.body, RecordBody::Environment { .. }))
        .count()
}

fn assert_dense(records: &[JournalRecord]) {
    for (index, record) in records.iter().enumerate() {
        assert_eq!(record.seq, index as u64, "{records:#?}");
    }
}

/// The session: one fixture release, the lock selecting it, the one operator every
/// policy asks, and a workspace for the assembly. Each generation's catalog is loaded
/// again from the release through S1.4's loader, exactly as a reload rebuilds one.
struct Stack {
    release: Release,
    lock: ModulesLock,
    services: ModuleServices,
    _processes: FakeProcesses,
    workspace: tempfile::TempDir,
    operator: Arc<Operator>,
}

impl Stack {
    fn new() -> Self {
        let release = Release::with_fixture();
        let lock = ModulesLock::parse(
            &release.root().join("modules.lock"),
            &lock_text(MODULE, &release.fixture_entry(FIXTURE_NAME)),
        )
        .expect("fixture lock");
        let (process, processes) = fake_processes();
        let process: Arc<dyn ProcessService> = process;
        let services: ModuleServices = Arc::new(move |_: &ToolServices| Services {
            process: Some(process.clone()),
            ..Services::default()
        });
        Self {
            release,
            lock,
            services,
            _processes: processes,
            workspace: tempfile::tempdir().expect("workspace"),
            operator: Arc::new(Operator::default()),
        }
    }

    fn operator(&self) -> Arc<Operator> {
        self.operator.clone()
    }

    /// A generation's catalog: the stand-in provider, then every package the lock
    /// selects, verified by the loader.
    fn load_catalog(&self) -> Catalog {
        let mut catalog = Catalog::new();
        let parent = ScriptedProvider::new(Vec::new());
        catalog.provider(
            PARENT,
            Box::new(move |_: &ProviderSpec| {
                Ok(Arc::new(Serves(parent.clone())) as Arc<dyn Provider>)
            }),
        );
        let packages = load_locked_modules(&self.lock, &self.release.manifest_file())
            .expect("the release loads");
        register_modules(&mut catalog, packages, self.services.clone()).expect("registration");
        catalog
    }

    /// A generation's candidate: the catalog loaded again, `authorization`, and the
    /// session's assembly on that catalog, whose context policy is `context`.
    fn generation(
        &self,
        context: Arc<Counted>,
        authorization: Arc<dyn AuthorizationPolicy>,
    ) -> Candidate {
        let catalog = Arc::new(self.load_catalog());
        let assembled = assemble(
            &catalog,
            &EnvironmentFile {
                name: "compaction-workload".into(),
                family: "test".into(),
                provider: PARENT.into(),
                model: "test-model".into(),
                profile: None,
                options: ModelOptions::default(),
                tools: Vec::new(),
                prompt_template: "tools: {{tool_names}}".into(),
                context: None,
                summarize_prompt: None,
            },
            self.workspace.path(),
            &Substitutions {
                workspace: "/work".into(),
                date: "2026-01-01".into(),
                os: "linux".into(),
            },
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
                context,
            },
        }
    }

    /// The parent agent on generation 0, as the start path builds it.
    fn start(&self, first: Candidate, journal: Arc<RecordingJournal>) -> (Agent, Arc<Generations>) {
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

// ------------------------------------------------------------------ the websocket double

/// What the double's connections play back: frames, then a failure or a clean end.
enum Frame {
    Text(String),
    Fail,
}

/// The relay's shared state: how many upgrades it served, how many of its connections
/// are alive, what was sent, and the script one connection consumes per connect.
#[derive(Default)]
struct RelayState {
    handshakes: AtomicUsize,
    live: AtomicUsize,
    sent: Mutex<Vec<String>>,
    script: Mutex<VecDeque<Vec<Frame>>>,
}

/// The connector double behind a [`WsSession`]: one scripted connection per connect,
/// with a live count that says when each connection was dropped.
#[derive(Clone)]
struct RelayConnector {
    state: Arc<RelayState>,
}

impl RelayConnector {
    /// Scripts `script`, consumed one connection per connect in order.
    fn new(script: Vec<Vec<Frame>>) -> Self {
        Self {
            state: Arc::new(RelayState {
                handshakes: AtomicUsize::new(0),
                live: AtomicUsize::new(0),
                sent: Mutex::new(Vec::new()),
                script: Mutex::new(script.into()),
            }),
        }
    }

    fn handshakes(&self) -> usize {
        self.state.handshakes.load(Ordering::SeqCst)
    }

    /// The connections that are open right now: raised by every connect, lowered by
    /// every drop of a connection — a failed response, a lease drop or a session drop.
    fn live(&self) -> usize {
        self.state.live.load(Ordering::SeqCst)
    }

    /// Every frame the connections were sent, in order.
    fn sent(&self) -> Vec<String> {
        self.state.sent.lock().unwrap().clone()
    }
}

impl WsConnector for RelayConnector {
    fn connect<'a>(
        &'a self,
        _request: WsHandshake,
    ) -> BoxFuture<'a, Result<Box<dyn WsConnection>, WsConnectError>> {
        let frames = self
            .state
            .script
            .lock()
            .unwrap()
            .pop_front()
            .expect("the script holds one connection per connect");
        self.state.handshakes.fetch_add(1, Ordering::SeqCst);
        self.state.live.fetch_add(1, Ordering::SeqCst);
        let state = Arc::clone(&self.state);
        Box::pin(async move {
            Ok(Box::new(RelayConnection {
                state,
                frames: frames.into(),
            }) as Box<dyn WsConnection>)
        })
    }
}

/// One open connection. Dropping it lowers the live count, whatever ends it.
struct RelayConnection {
    state: Arc<RelayState>,
    frames: VecDeque<Frame>,
}

impl Drop for RelayConnection {
    fn drop(&mut self) {
        self.state.live.fetch_sub(1, Ordering::SeqCst);
    }
}

impl WsConnection for RelayConnection {
    fn send_text<'a>(&'a mut self, text: String) -> BoxFuture<'a, Result<(), WsError>> {
        Box::pin(async move {
            self.state.sent.lock().unwrap().push(text);
            Ok(())
        })
    }

    fn next_text<'a>(&'a mut self) -> BoxFuture<'a, Result<Option<String>, WsError>> {
        Box::pin(async move {
            Ok(match self.next_bounded().await? {
                WsNext::Text(text) => Some(text),
                WsNext::Closed | WsNext::Timeout(_) => None,
            })
        })
    }

    fn next_bounded<'a>(&'a mut self) -> BoxFuture<'a, Result<WsNext, WsError>> {
        Box::pin(async move {
            match self.frames.pop_front() {
                Some(Frame::Text(text)) => Ok(WsNext::Text(text)),
                Some(Frame::Fail) => {
                    Err(WsError("scripted: the relay died mid-response".to_owned()))
                }
                None => Ok(WsNext::Closed),
            }
        })
    }
}

/// The route binding the leases send under: a fixed endpoint, no credential (the
/// egress proxy would inject it, issue #134).
fn authority() -> WsAuthority<'static> {
    WsAuthority {
        endpoint: "https://relay.test/backend-api/codex/responses",
        credential: None,
    }
}

/// The first send of a request: a head, so the session opens a connection.
fn head_send(frame: &str) -> WsSend {
    WsSend {
        handshake: Some(WsHead {
            path: String::new(),
            headers: Vec::new(),
            credential: CredentialUse {
                scheme: CredentialScheme::Bearer,
                account_id_header: None,
            },
        }),
        frame: frame.to_owned(),
    }
}

/// A continuation on the connection a clean completion returned.
fn continue_send(frame: &str) -> WsSend {
    WsSend {
        handshake: None,
        frame: frame.to_owned(),
    }
}

/// Reads one text frame, failing the case with the actual read otherwise.
async fn next_text(lease: &mut WsLease, cancel: &CancellationToken) -> String {
    match lease.next(cancel).await {
        WsRead::Next(WsNext::Text(frame)) => frame,
        other => panic!("expected a frame, got {other:?}"),
    }
}

// ------------------------------------------------------------------ the cases

/// *stores live only while their call runs*: every `compact_now` gets its own
/// [`Store`], the answer leaves none behind, two calls at once park their summaries
/// without crossing, and dropping the policy leaves exactly the keeper holding the
/// summary service once the executor's wind-down is scheduled.
///
/// The counts: at rest the service is held by the case's keeper and the policy's
/// executor (2). While a summary is parked the call's [`Store`] and the summary task's
/// own handle add two more (4). After the answer returned the count is back to 2 — the
/// [`Store`] is dropped before the reply — and after the policy's drop, 1.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_stores_live_only_while_their_call_runs() {
    within_deadline("compaction stores live only while their call runs", async {
        let summary = ScriptedSummary::parked();
        let policy = Counted::new(summary.clone());
        let history = long_history();

        // At rest: the keeper and the executor, nothing else.
        assert_eq!(summary.live(), 2, "keeper plus executor, no call, no Store");

        // One parked call: inside summary.summarize its Store is alive, exactly two
        // holders beyond the resting two — the Store and the summary task's handle.
        let first = {
            let policy = policy.clone();
            let history = history.clone();
            tokio::spawn(async move { compact(&policy, &history).await })
        };
        summary.entered().await;
        assert_eq!(summary.requests(), 1, "one summary per compaction");
        assert_eq!(
            summary.live(),
            4,
            "keeper, executor, the parked call's Store, the summary task's handle"
        );
        summary.release();
        let (before, after) = first.await.expect("the compaction task joins");
        assert!(after < before, "{before} -> {after}");
        eventually(
            "the Store and the summary handle return with the answer",
            || summary.live() == 2,
        )
        .await;
        assert_eq!(
            summary.live(),
            2,
            "no Store is kept after the call's answer"
        );

        // Two calls at once: each parks on its own Store, on the same policy.
        let first = {
            let policy = policy.clone();
            let history = history.clone();
            tokio::spawn(async move { compact(&policy, &history).await })
        };
        let second = {
            let policy = policy.clone();
            let history = history.clone();
            tokio::spawn(async move { compact(&policy, &history).await })
        };
        summary.entered().await;
        summary.entered().await;
        assert_eq!(summary.requests(), 3, "one summary per call, none shared");
        assert_eq!(
            summary.live(),
            6,
            "two Stores and two summary handles over the resting two"
        );
        summary.release();
        summary.release();
        let (first, second) = tokio::join!(first, second);
        let (before_a, after_a) = first.expect("the first compaction task joins");
        let (before_b, after_b) = second.expect("the second compaction task joins");
        assert!(after_a < before_a && after_b < before_b);
        eventually("both Stores return with their answers", || {
            summary.live() == 2
        })
        .await;
        assert_eq!(summary.live(), 2);

        // The policy's drop releases the executor's handle once the wind-down is
        // scheduled: the keeper is the last holder, and nothing leaks.
        drop(policy);
        eventually(
            "the executor released the service after the policy's drop",
            || summary.live() == 1,
        )
        .await;
        assert_eq!(
            summary.live(),
            1,
            "no handle outlives the policy: the keeper is the last"
        );
    })
    .await;
}

/// *a reload drops the old generation and its handles*: [`install_candidate`] swaps the
/// whole generation — generation record, authorization policy, context policy — and
/// the swap itself drops the old ones. A compaction parked mid-summary on the old
/// generation still finishes on its own policy, and nothing of the old generation is
/// left once the case lets go of its handle.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_reload_drops_the_old_generation_and_its_handles() {
    within_deadline("a reload drops the old generation and its handles", async {
        let stack = Stack::new();
        let journal = Arc::new(RecordingJournal::new());

        // Generation 0: its summaries park, so the case can hold a Store open across
        // the reload that replaces the generation around it.
        let summary0 = ScriptedSummary::parked();
        let context0 = Counted::new(summary0.clone());
        let dropped0 = context0.dropped();
        let policy0 = policy(FULL_ACCESS, &stack.operator());
        let (mut agent, generations) = stack.start(
            stack.generation(context0.clone(), policy0.clone()),
            journal.clone(),
        );
        drop(policy0); // the agent and the generation record hold it now
        let generation0 = Arc::downgrade(&generations.current());
        let policy0_weak = Arc::downgrade(&generations.current().authorization());

        // A compaction parked mid-summary while the reload happens around it.
        let history = long_history();
        let parked = {
            let context = context0.clone();
            let history = history.clone();
            tokio::spawn(async move { compact(&context, &history).await })
        };
        summary0.entered().await;
        assert_eq!(summary0.live(), 4, "the parked call's Store is alive");

        // The reload: generation 1, its own context policy and its own summary service.
        let summary1 = ScriptedSummary::open();
        let context1 = Counted::new(summary1.clone());
        let installed = install_candidate(
            &generations,
            &mut agent,
            stack.generation(context1, policy(FULL_ACCESS, &stack.operator())),
        )
        .await
        .expect("the candidate installs");
        assert_eq!(installed.number(), 1);
        assert_eq!(generations.current().number(), 1);

        // The swap dropped the old generation record and its authorization policy —
        // deterministically, inside install_candidate. The old context policy is held
        // by the case and by the parked call (a generation's end is not a started
        // call's), so the agent's own handle is already gone from the count.
        assert!(
            generation0.upgrade().is_none(),
            "the old generation record went with the swap"
        );
        assert!(
            policy0_weak.upgrade().is_none(),
            "the old authorization policy went with its last user"
        );
        assert_eq!(
            Arc::strong_count(&context0),
            2,
            "the case's clone and the parked call's clone; the agent dropped its own"
        );
        assert_eq!(
            dropped0.load(Ordering::SeqCst),
            0,
            "the case still holds the policy, so it has not dropped"
        );

        // The parked compaction finishes on the policy the swap retired.
        summary0.release();
        let (before, after) = parked.await.expect("the parked compaction joins");
        assert!(after < before, "{before} -> {after}");

        // The parked call returned its handle: only the case's clone is left, so the
        // agent's handle went with the swap and nothing else took one.
        assert_eq!(
            Arc::strong_count(&context0),
            1,
            "the agent dropped the old context policy; the case's clone is the last"
        );

        // Nothing of generation 0 is left: drop the case's handle, then the executor.
        drop(context0);
        assert_eq!(dropped0.load(Ordering::SeqCst), 1);
        eventually("the old generation's executor released the service", || {
            summary0.live() == 1
        })
        .await;
        assert_eq!(
            summary0.live(),
            1,
            "no handle of the old generation outlives it"
        );

        // One reload, one Environment record, and the journal stays dense.
        let records = journal.records();
        assert_eq!(environments(&records), 1, "{records:#?}");
        assert_dense(&records);
    })
    .await;
}

/// *three reconnects drop each connection once its response ends*: a failed response
/// drops its connection, the next request reconnects with a new handshake, a clean
/// completion returns its connection for a continuation without a new handshake, and a
/// lease drop takes the connection it is using. The live count never drifts from the
/// responses that are actually running.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn three_reconnects_drop_each_connection_once_its_response_ends() {
    within_deadline(
        "three reconnects drop each connection once its response ends",
        async {
            let relay = RelayConnector::new(vec![
                vec![Frame::Text("response-1".into()), Frame::Fail],
                vec![Frame::Text("response-2".into()), Frame::Fail],
                vec![Frame::Text("response-3".into()), Frame::Fail],
                vec![Frame::Text("response-4".into())],
            ]);
            let session = WsSession::new(Arc::new(relay.clone()), Arc::new(Instant::now));
            let cancel = CancellationToken::new();

            for round in 0..3u32 {
                let mut lease = session.try_lease().expect("the session is free");
                assert!(
                    !lease.state().open,
                    "round {round}: the previous response's failure dropped its connection"
                );
                lease
                    .send(authority(), head_send(&format!("request-{round}")), &cancel)
                    .await
                    .expect("the head opens a connection");
                assert_eq!(relay.handshakes(), round as usize + 1, "one per request");
                assert_eq!(relay.live(), 1);
                assert_eq!(
                    next_text(&mut lease, &cancel).await,
                    format!("response-{}", round + 1)
                );
                assert!(
                    matches!(lease.next(&cancel).await, WsRead::Failed(_)),
                    "the scripted failure ends the read"
                );
                assert_eq!(
                    relay.live(),
                    0,
                    "the failed response dropped its connection"
                );
                drop(lease);
            }

            // A clean completion returns its connection: the next request sends without
            // a head and reuses it.
            let mut lease = session.try_lease().expect("the session is free");
            lease
                .send(authority(), head_send("request-3"), &cancel)
                .await
                .expect("the head opens a connection");
            assert_eq!(next_text(&mut lease, &cancel).await, "response-4");
            lease.completed(Some("resp_4".to_owned()));
            drop(lease);
            assert_eq!(
                relay.live(),
                1,
                "the clean completion returned its connection to the session"
            );

            let mut lease = session.try_lease().expect("the session is free");
            let state = lease.state();
            assert!(state.open);
            assert_eq!(state.last_clean_response.as_deref(), Some("resp_4"));
            assert!(!state.failed_before_output);
            lease
                .send(authority(), continue_send("request-4"), &cancel)
                .await
                .expect("the open connection carries the continuation");
            assert_eq!(
                relay.handshakes(),
                4,
                "the continuation reuses the connection: no new handshake"
            );
            drop(lease);
            assert_eq!(relay.live(), 0, "the lease drop took its connection");
            drop(session);
            assert_eq!(relay.live(), 0);
            assert_eq!(
                relay.sent(),
                vec![
                    "request-0",
                    "request-1",
                    "request-2",
                    "request-3",
                    "request-4"
                ],
            );
        },
    )
    .await;
}

/// *compaction, reload and reconnect drop every handle*: three rounds, each one
/// compaction on the current generation's policy, one forced reconnect, one
/// [`install_candidate`] to the next generation. After every round the previous
/// generation's record, authorization policy, context policy, executor and summary
/// service are all gone, and the wire holds no connection.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn compaction_reload_and_reconnect_drop_every_handle() {
    within_deadline(
        "compaction, reload and reconnect drop every handle",
        async {
            let stack = Stack::new();
            let journal = Arc::new(RecordingJournal::new());
            let relay = RelayConnector::new(vec![
                vec![Frame::Text("response-1".into()), Frame::Fail],
                vec![Frame::Text("response-2".into()), Frame::Fail],
                vec![Frame::Text("response-3".into()), Frame::Fail],
            ]);
            let wire = WsSession::new(Arc::new(relay.clone()), Arc::new(Instant::now));
            let cancel = CancellationToken::new();
            let history = long_history();

            let mut summary = ScriptedSummary::open();
            let mut context = Counted::new(summary.clone());
            let (mut agent, generations) = stack.start(
                stack.generation(context.clone(), policy(FULL_ACCESS, &stack.operator())),
                journal.clone(),
            );
            let mut generation_weak = Arc::downgrade(&generations.current());
            let mut policy_weak = Arc::downgrade(&generations.current().authorization());

            for round in 0..3u32 {
                // The compaction: on the generation that is current in this round, its
                // Store returned with its answer.
                let (before, after) = compact(&context, &history).await;
                assert_eq!(summary.requests(), 1, "round {round}: one summary");
                eventually("the call's Store returned with its answer", || {
                    summary.live() == 2
                })
                .await;
                assert_eq!(summary.live(), 2, "round {round}: keeper plus executor");
                assert!(after < before, "round {round}: {before} -> {after}");

                // The reconnect: the response fails and its connection goes with it.
                let mut lease = wire.try_lease().expect("the wire is free");
                assert!(!lease.state().open, "round {round}: nothing held over");
                lease
                    .send(authority(), head_send(&format!("request-{round}")), &cancel)
                    .await
                    .expect("the head opens a connection");
                assert_eq!(
                    next_text(&mut lease, &cancel).await,
                    format!("response-{}", round + 1)
                );
                assert!(matches!(lease.next(&cancel).await, WsRead::Failed(_)));
                assert_eq!(relay.live(), 0, "round {round}: the connection went");
                drop(lease);

                // The reload: the next generation takes over, the old one is dropped by
                // the swap itself.
                let next_summary = ScriptedSummary::open();
                let next_context = Counted::new(next_summary.clone());
                let installed = install_candidate(
                    &generations,
                    &mut agent,
                    stack.generation(next_context.clone(), policy(FULL_ACCESS, &stack.operator())),
                )
                .await
                .expect("the candidate installs");
                assert_eq!(installed.number(), round as u64 + 1);
                assert_eq!(generations.current().number(), round as u64 + 1);
                assert!(
                    generation_weak.upgrade().is_none(),
                    "round {round}: the generation record went with the swap"
                );
                assert!(
                    policy_weak.upgrade().is_none(),
                    "round {round}: the authorization policy went with its last user"
                );
                assert_eq!(
                    Arc::strong_count(&context),
                    1,
                    "round {round}: the agent dropped the old context policy"
                );
                let dropped = context.dropped();
                drop(context);
                assert_eq!(
                    dropped.load(Ordering::SeqCst),
                    1,
                    "round {round}: the case's handle was the last"
                );
                eventually("the old generation's executor released the service", || {
                    summary.live() == 1
                })
                .await;
                assert_eq!(
                    summary.live(),
                    1,
                    "round {round}: the keeper is the last holder"
                );

                // The next round runs on the generation this reload installed.
                summary = next_summary;
                context = next_context;
                generation_weak = Arc::downgrade(&generations.current());
                policy_weak = Arc::downgrade(&generations.current().authorization());
            }

            // The journal: one Environment record per reload, the sequences dense; the
            // wire holds no connection and the last generation is the current one.
            let records = journal.records();
            assert_eq!(environments(&records), 3, "{records:#?}");
            assert_dense(&records);
            assert_eq!(relay.handshakes(), 3);
            assert_eq!(relay.live(), 0);
            assert_eq!(generations.current().number(), 3);
        },
    )
    .await;
}
