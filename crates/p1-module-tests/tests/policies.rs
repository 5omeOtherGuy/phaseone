//! The policies suite (S5.4, issue #307): the frozen cases of the policy boundary driven
//! through the components this repository builds and the adapters S5.2 and S5.3 landed.
//!
//! The four frozen case names and where each is asserted:
//!
//! - *summary recursion avoidance*: [`summary_recursion_avoidance`], over
//!   `p1/context/summarizing` through `WasmContextPolicy` and a test `SummaryService`:
//!   one summary request per attempt, no export of the policy entered from within a
//!   summary, and a second call started while the first one's summary is parked runs on
//!   its own fresh instance instead of deadlocking.
//! - *the ask bridge and cancellation*: [`the_ask_bridge_over_the_component`] and
//!   [`cancellation`], over `p1/policy/ask` and `p1/policy/full-access` through
//!   `WasmAuthorizationPolicy` behind an `AskBridge`: read-only is permitted silently,
//!   headless denies the component's `ask`, `y`/`n`/`a` mean what ADR-0038 says, and an
//!   open question resolves to the cancel deny when the active turn's token fires.
//! - *a policy trap yields a conservative decision*: [`a_policy_trap_yields_a_conservative_decision`],
//!   an out-of-fuel stop of a shipped policy and the deadline stop beside it.
//! - *a changed approval key does not carry an old approval*: [`a_changed_approval_key_does_not_carry_an_old_approval`].
//!
//! How the trap is produced, and why: no trap fixture package is shipped and neither
//! `p1/policy/ask` nor `p1/policy/full-access` traps on its own, so the suite makes a
//! policy trap through its [`ExecutionLimits`]: a call with `fuel: 1` runs out of fuel in
//! its first instructions and the executor reports `FuelExhausted`, which the adapter
//! turns into a `Deny` naming the package. That is the same mapping a guest trap takes
//! (`executor::failure`), and it is the only stop an authorization policy can be made to
//! take here: the shipped policies call no host import, so nothing parks one of their
//! calls for the manual epoch clock to end. The deadline stop is therefore shown on the
//! context policy, whose call parks in `summary.summarize` — the one point where
//! [`ManualEpochs`] ends a policy call deterministically.
//!
//! The components are read from `modules/target/p1-modules/<package>/`; a missing build
//! fails the case with how to build it, and no case is skipped. Every case runs under the
//! harness's deadlock guard and synchronizes on explicit events: no sleep, no network.

use std::future::Future;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_context::{ContextConfig, DEFAULT_SUMMARY_OUTPUT_TOKENS, estimate_tokens};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AssistantBlock, AssistantItem, AuthorizationPolicy, AuthorizationRequest, BoxFuture,
    CancellationToken, Compaction, ContextError, ContextInput, ContextPolicy, Decision, Effect,
    Item, Origin, Prepared, StopReason, ToolCall, ToolIdentity, ToolInput,
};
use p1_host::policy::{
    ASK_POLICY, AskBridge, CANCEL_DENY, FULL_ACCESS_POLICY, HEADLESS_DENY, PolicyId, USER_DENY,
    Verdict as HostVerdict, VerdictSource,
};
use p1_host::{LineSource, SharedWriter};
use p1_module_runtime::loader::EPOCH_TICK;
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ManualEpochs, ReleaseManifest, SummaryError,
    SummaryRequest, SummaryResponse, SummaryService, Verdict as WasmVerdict,
    WasmAuthorizationPolicy, WasmContextPolicy,
};
use p1_module_tests::within_deadline;
use tokio::sync::{Notify, mpsc};
use tokio::time::timeout;

/// The built package of the summarizing context policy (S5.2).
const CONTEXT: (&str, &str) = ("p1-module-context", "p1/context/summarizing");
/// The built restrictive authorization policy (S5.3).
const ASK: (&str, &str) = ("p1-module-policy-ask", "p1/policy/ask");
/// The built full-access authorization policy (S5.3), the shipped default.
const FULL_ACCESS: (&str, &str) = ("p1-module-policy-full-access", "p1/policy/full-access");

/// Every effect of the authorization world, in the order ADR-0038 treats them.
const EFFECTS: [Effect; 4] = [
    Effect::ReadOnly,
    Effect::WritesFiles,
    Effect::Executes,
    Effect::Delegates,
];

// ------------------------------------------------------------------ the built packages

/// Where `scripts/build-modules.sh` publishes the built packages.
fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

/// The built package manifest of `package`, or the harness's instruction to build first.
fn package_manifest(package: &str) -> Value {
    let path = built()
        .join(package)
        .join(format!("{package}.manifest.json"));
    let text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
        panic!(
            "the build output {} is missing ({error}): run scripts/build-modules.sh first",
            path.display()
        )
    });
    serde_json::from_str(&text).expect("the package manifest is JSON")
}

/// A release manifest entry for the built `package`, as the build published it.
fn entry(package: (&str, &str)) -> Value {
    let manifest = package_manifest(package.0);
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

/// A loader over the built packages, whose epochs advance on the production ticker.
fn loader(packages: &[(&str, &str)]) -> Loader {
    Loader::new(release(packages), built()).expect("loader")
}

/// As [`loader`], with epochs only the returned [`ManualEpochs`] advances.
fn manual_loader(packages: &[(&str, &str)]) -> (Loader, ManualEpochs) {
    Loader::with_manual_epochs(release(packages), built()).expect("loader")
}

/// The built package `package`, verified and compiled through a release manifest.
fn load(loader: &Loader, package: (&str, &str)) -> LoadedModule {
    loader.load(package.1).expect("the built package loads")
}

/// The authorization policy adapter over the built `package`.
fn authorization_policy(package: (&str, &str), limits: ExecutionLimits) -> WasmAuthorizationPolicy {
    WasmAuthorizationPolicy::new(&load(&loader(&[package]), package), limits)
        .expect("the policy adapter builds")
}

// ------------------------------------------------------------------ the context inputs

/// `configure`'s settings for `config`: the component's own keys and the summary cap.
fn settings(config: &ContextConfig) -> String {
    json!({
        "window_tokens": config.window_tokens,
        "output_headroom_tokens": config.output_headroom_tokens,
        "summarize_at_tokens": config.summarize_at_tokens,
        "keep_recent_tokens": config.keep_recent_tokens,
        "user_verbatim_tokens": config.user_verbatim_tokens,
        "tool_result_excerpt_chars": config.tool_result_excerpt_chars,
        "summary_output_tokens": DEFAULT_SUMMARY_OUTPUT_TOKENS,
    })
    .to_string()
}

/// A config that summarizes as soon as the history is above one token, with a wall far
/// above, and still leaves a replacement below `summarize_at_tokens`.
fn summarize_soon(history: &[Item]) -> ContextConfig {
    let total = estimate_tokens(history);
    ContextConfig {
        window_tokens: total + 5_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: total.max(2) - 1,
        keep_recent_tokens: 20,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

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

/// A history above any threshold below: a prelude the summary replaces, two units so the
/// tail rule has something to keep (a manual compaction summarizes only then), and a
/// trailing user item outside every unit.
fn history() -> Vec<Item> {
    vec![
        Item::User {
            text: "task: fix the parser ".repeat(100),
        },
        assistant(&"first answer ".repeat(50)),
        Item::User {
            text: "constraint: keep the wall".to_owned(),
        },
        assistant(&"second answer ".repeat(50)),
        Item::User {
            text: "tail".to_owned(),
        },
    ]
}

fn input<'a>(history: &'a [Item], cancel: &'a CancellationToken) -> ContextInput<'a> {
    ContextInput {
        history,
        last_usage: None,
        cancel,
    }
}

// ------------------------------------------------------------------ the recording summary

/// What the test's summary service records: how many requests it answered, how deep it
/// got, and whether the policy was entered while one of its summaries was in flight. The
/// gate parks every summary until the test releases it, so a case can act on a call that
/// waits inside `summary.summarize`; `entered` is the explicit event for that state.
#[derive(Default)]
struct Recorder {
    requests: AtomicUsize,
    in_flight: AtomicUsize,
    deepest: AtomicUsize,
    exports_during_summary: AtomicUsize,
    gated: AtomicBool,
    entered: Notify,
    released: Notify,
}

impl Recorder {
    /// The requests answered so far.
    fn requests(&self) -> usize {
        self.requests.load(Ordering::SeqCst)
    }

    /// The most summaries that ever ran at once.
    fn deepest(&self) -> usize {
        self.deepest.load(Ordering::SeqCst)
    }

    /// Exports of the policy entered while a summary was in flight.
    fn overlapping_exports(&self) -> usize {
        self.exports_during_summary.load(Ordering::SeqCst)
    }
}

/// A `SummaryService` that records what it sees and answers one completed summary.
struct RecordingSummary {
    recorder: Arc<Recorder>,
}

impl SummaryService for RecordingSummary {
    fn summarize(
        &self,
        _request: SummaryRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<SummaryResponse, SummaryError>> {
        Box::pin(async move {
            let recorder = &self.recorder;
            recorder.requests.fetch_add(1, Ordering::SeqCst);
            let depth = recorder.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            recorder.deepest.fetch_max(depth, Ordering::SeqCst);
            recorder.entered.notify_one();
            if recorder.gated.load(Ordering::SeqCst) {
                recorder.released.notified().await;
            }
            recorder.in_flight.fetch_sub(1, Ordering::SeqCst);
            // `end-turn` is not truncated: the engine accepts it as it is, so one request
            // per attempt, never the cap-doubling retry.
            Ok(SummaryResponse {
                text: "the summary".to_owned(),
                stop: StopReason::EndTurn,
                usage: None,
            })
        })
    }
}

/// The component with its export entries watched: every entry that happens while a
/// summary is in flight is recorded, which is how a case shows the policy is not
/// re-entered from within one.
struct WatchedPolicy {
    inner: WasmContextPolicy,
    recorder: Arc<Recorder>,
}

impl WatchedPolicy {
    fn watch(&self) {
        if self.recorder.in_flight.load(Ordering::SeqCst) > 0 {
            self.recorder
                .exports_during_summary
                .fetch_add(1, Ordering::SeqCst);
        }
    }
}

impl ContextPolicy for WatchedPolicy {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            self.watch();
            self.inner.prepare(input).await
        })
    }

    fn compact_now<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Compaction, ContextError>> {
        Box::pin(async move {
            self.watch();
            self.inner.compact_now(input).await
        })
    }
}

/// The context policy over `module` with `config`, recording what its summaries see.
fn watched_context(
    module: &LoadedModule,
    config: &ContextConfig,
    recorder: &Arc<Recorder>,
    limits: ExecutionLimits,
) -> WatchedPolicy {
    let service = Arc::new(RecordingSummary {
        recorder: recorder.clone(),
    });
    WatchedPolicy {
        inner: WasmContextPolicy::new(module, &settings(config), service, limits)
            .expect("the component accepts the settings"),
        recorder: recorder.clone(),
    }
}

// ------------------------------------------------------------------ the ask bridge harness

/// The component's verdicts with the host's shape, under the policy id the bridge keys
/// remembered approvals by: the package name and the digest of the loaded bytes. The test
/// can switch the digest, as a reload to a different artifact would.
struct ComponentSource {
    policy: WasmAuthorizationPolicy,
    id: Mutex<PolicyId>,
}

impl ComponentSource {
    fn new(policy: WasmAuthorizationPolicy) -> Arc<Self> {
        let id = PolicyId {
            package: policy.name().to_owned(),
            digest: policy.digest().to_string(),
        };
        Arc::new(Self {
            policy,
            id: Mutex::new(id),
        })
    }

    /// The policy id the bridge is keying under now.
    fn id(&self) -> PolicyId {
        self.id.lock().unwrap().clone()
    }

    /// Reloads the same package name to other bytes.
    fn switch_digest(&self, digest: &str) {
        self.id.lock().unwrap().digest = digest.to_owned();
    }
}

impl VerdictSource for ComponentSource {
    fn policy(&self) -> PolicyId {
        self.id()
    }

    fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, HostVerdict> {
        Box::pin(async move {
            match self.policy.verdict(request).await {
                WasmVerdict::Permit => HostVerdict::Permit,
                WasmVerdict::Deny(reason) => HostVerdict::Deny(reason),
                WasmVerdict::Ask => HostVerdict::Ask,
            }
        })
    }
}

/// Lines the test sends; each read announces itself on `asked` first, so a case knows the
/// question is open without sleeping, and `reads` counts the questions actually asked.
struct ScriptedLines {
    lines: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
    asked: mpsc::UnboundedSender<()>,
    reads: AtomicUsize,
}

impl LineSource for ScriptedLines {
    fn next_line<'a>(&'a self) -> Pin<Box<dyn Future<Output = Option<String>> + Send + 'a>> {
        Box::pin(async move {
            self.reads.fetch_add(1, Ordering::SeqCst);
            let _ = self.asked.send(());
            self.lines.lock().await.recv().await
        })
    }
}

/// A writer into a buffer the test reads back: what the bridge prompted on stderr.
#[derive(Clone, Default)]
struct Buffer(Arc<Mutex<Vec<u8>>>);

impl Write for Buffer {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The ask bridge over one component, with the line front end and the turn token the case
/// controls.
struct Harness {
    bridge: AskBridge,
    source: Arc<ComponentSource>,
    lines: Arc<ScriptedLines>,
    send: mpsc::UnboundedSender<String>,
    asked: tokio::sync::Mutex<mpsc::UnboundedReceiver<()>>,
    stderr: Buffer,
}

impl Harness {
    fn new(source: Arc<ComponentSource>, headless: bool, scope: CancellationToken) -> Self {
        let (send, receiver) = mpsc::unbounded_channel();
        let (asked_tx, asked) = mpsc::unbounded_channel();
        let lines = Arc::new(ScriptedLines {
            lines: tokio::sync::Mutex::new(receiver),
            asked: asked_tx,
            reads: AtomicUsize::new(0),
        });
        let stderr = Buffer::default();
        let stderr_writer: SharedWriter = Arc::new(Mutex::new(Box::new(stderr.clone())));
        let bridge = AskBridge::new(
            source.clone(),
            headless,
            lines.clone(),
            stderr_writer,
            scope,
        );
        Self {
            bridge,
            source,
            lines,
            send,
            asked: tokio::sync::Mutex::new(asked),
            stderr,
        }
    }

    /// Scripts the next line the front end reads.
    fn answer(&self, line: &str) {
        self.send.send(line.to_owned()).expect("the test reads");
    }

    /// How many questions were asked.
    fn reads(&self) -> usize {
        self.lines.reads.load(Ordering::SeqCst)
    }

    /// Every prompt the bridge wrote, in order.
    fn prompts(&self) -> String {
        String::from_utf8(self.stderr.0.lock().unwrap().clone()).expect("the prompt is UTF-8")
    }

    /// The policy that decides now.
    fn policy_id(&self) -> PolicyId {
        self.source.id()
    }

    /// Reloads the deciding package to other bytes, under the same name.
    fn switch_digest(&self, digest: &str) {
        self.source.switch_digest(digest);
    }

    /// Marks the live turn's token, as the front end does.
    fn set_turn(&self, token: Option<CancellationToken>) {
        self.bridge.set_turn(token);
    }

    /// Waits until a question is open (the line front end has been read).
    async fn wait_for_question(&self) {
        self.asked
            .lock()
            .await
            .recv()
            .await
            .expect("the line source is gone");
    }

    /// Drops the announcements of questions that were answered with a line the case had
    /// already scripted. Call it only while no call is in flight, so the only outstanding
    /// announcements belong to reads that finished.
    async fn forget_answered_questions(&self) {
        let mut asked = self.asked.lock().await;
        while asked.try_recv().is_ok() {}
    }

    /// Asks the bridge to authorize `effect` for the tool `identity` names.
    async fn authorize(&self, identity: &ToolIdentity, effect: Effect) -> Decision {
        let call = call();
        self.bridge
            .authorize(AuthorizationRequest {
                call: &call,
                identity,
                effect,
            })
            .await
    }
}

fn call() -> ToolCall {
    ToolCall {
        call_id: "c1".to_owned(),
        name: "shell".to_owned(),
        input: ToolInput::Text("ls -la".to_owned()),
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: "p1/shell".to_owned(),
        variant: variant.to_owned(),
    }
}

fn deny(reason: &str) -> Decision {
    Decision::Deny {
        reason: reason.to_owned(),
    }
}

/// The exact question the bridge asks for [`call`].
const PROMPT: &str = "allow shell ls -la? [y]es / [n]o / [a]lways for this tool: ";

/// A digest of another artifact of the same package, as a reload would install.
fn other_digest() -> String {
    format!("sha256:{}", "0".repeat(64))
}

// ------------------------------------------------------------------ the cases

/// Frozen case 1: summary recursion avoidance.
///
/// The component summarizes on a fresh instance per call; the summary itself runs
/// outside the Store that owns the call (ADR-0036, ADR-0084 §1), so one attempt is one
/// summary request, no export of the policy is entered from within a summary, and a second
/// call started while the first one's summary is parked on a gate runs on its own fresh
/// instance rather than deadlocking behind it.
#[tokio::test]
async fn summary_recursion_avoidance() {
    within_deadline("summary recursion avoidance", async {
        let module = load(&loader(&[CONTEXT]), CONTEXT);
        let history = history();
        let config = summarize_soon(&history);
        let cancel = CancellationToken::new();

        // One attempt of each export, below a wall far above: exactly one summary each.
        let recorder = Arc::new(Recorder::default());
        let policy = watched_context(&module, &config, &recorder, ExecutionLimits::default());
        match policy.prepare(input(&history, &cancel)).await {
            Ok(Some(_)) => {}
            Ok(None) => panic!("the history is above the threshold: a replacement is due"),
            Err(error) => panic!("prepare failed: {error}"),
        }
        assert_eq!(recorder.requests(), 1, "one summary request per attempt");
        assert_eq!(recorder.deepest(), 1, "a summary ran inside another");
        assert_eq!(
            recorder.overlapping_exports(),
            0,
            "the policy was entered from within a summary"
        );

        match policy.compact_now(input(&history, &cancel)).await {
            Ok(Compaction::Replaced { .. }) => {}
            Ok(Compaction::Unchanged { tokens }) => {
                panic!("a manual compaction must replace the tail, not keep {tokens} tokens")
            }
            Err(error) => panic!("compact-now failed: {error}"),
        }
        assert_eq!(recorder.requests(), 2, "one summary request per attempt");
        assert_eq!(recorder.deepest(), 1);
        assert_eq!(recorder.overlapping_exports(), 0);

        // The gate: the first call parks inside `summary.summarize` while the case runs a
        // second `prepare` on the SAME policy. A fresh instance each call is what lets the
        // second one answer; a store shared behind a lock would deadlock here.
        let parked = Arc::new(Recorder::default());
        parked.gated.store(true, Ordering::SeqCst);
        let policy = watched_context(&module, &config, &parked, ExecutionLimits::default());
        let short = vec![Item::User {
            text: "hi".to_owned(),
        }];
        let (first, second) = tokio::join!(policy.prepare(input(&history, &cancel)), async {
            parked.entered.notified().await;
            // Nothing of the policy has run since the summary began.
            assert_eq!(parked.overlapping_exports(), 0);
            let second = policy.prepare(input(&short, &cancel)).await;
            parked.released.notify_one();
            second
        });
        assert!(
            matches!(first, Ok(Some(_))),
            "the first call is parked on the gate's summary, not answered early"
        );
        assert!(
            matches!(second, Ok(None)),
            "the short history needs no summary"
        );
        assert_eq!(parked.requests(), 1, "one summary request per attempt");
        assert_eq!(parked.deepest(), 1);
        // The one overlapping entry is the second call this case started itself, never a
        // re-entrant one: the recorder is not vacuously zero.
        assert_eq!(parked.overlapping_exports(), 1);
    })
    .await;
}

/// Frozen case 2, first half: the ask bridge over the component.
///
/// `p1/policy/ask` permits read-only calls and answers `ask` for every other effect; the
/// bridge resolves that through the front end (or headless), so only `Permit` or `Deny`
/// reaches the core (ADR-0024, ADR-0038, ADR-0084 §2).
#[tokio::test]
async fn the_ask_bridge_over_the_component() {
    within_deadline("the ask bridge over the component", async {
        let interactive = Harness::new(
            ComponentSource::new(authorization_policy(ASK, ExecutionLimits::default())),
            false,
            CancellationToken::new(),
        );
        assert_eq!(interactive.policy_id().package, ASK_POLICY);
        assert!(
            interactive.policy_id().digest.starts_with("sha256:"),
            "{:?}",
            interactive.policy_id()
        );

        // Read-only: the component permits it and no question is asked.
        assert_eq!(
            interactive
                .authorize(&identity("default"), Effect::ReadOnly)
                .await,
            Decision::Permit
        );
        assert_eq!(interactive.reads(), 0);
        assert_eq!(interactive.prompts(), "");

        // Interactive: `y` permits, anything else is the user's refusal, `a` permits and
        // remembers the tool, its identity and the deciding policy.
        interactive.answer("y");
        assert_eq!(
            interactive
                .authorize(&identity("default"), Effect::Executes)
                .await,
            Decision::Permit
        );
        interactive.answer("n");
        assert_eq!(
            interactive
                .authorize(&identity("default"), Effect::Executes)
                .await,
            deny(USER_DENY)
        );
        interactive.answer("a");
        assert_eq!(
            interactive
                .authorize(&identity("default"), Effect::Executes)
                .await,
            Decision::Permit
        );
        assert_eq!(interactive.reads(), 3, "one question per ask");
        // Remembered: the same call is permitted without reading a line.
        assert_eq!(
            interactive
                .authorize(&identity("default"), Effect::Executes)
                .await,
            Decision::Permit
        );
        assert_eq!(interactive.reads(), 3);
        assert_eq!(interactive.prompts(), PROMPT.repeat(3));

        // Headless: the component's ask is the headless deny, and no line is read.
        let headless = Harness::new(
            ComponentSource::new(authorization_policy(ASK, ExecutionLimits::default())),
            true,
            CancellationToken::new(),
        );
        assert_eq!(
            headless
                .authorize(&identity("default"), Effect::Executes)
                .await,
            deny(HEADLESS_DENY)
        );
        assert_eq!(headless.reads(), 0);
        assert_eq!(headless.prompts(), "");
        assert_eq!(
            headless
                .authorize(&identity("default"), Effect::ReadOnly)
                .await,
            Decision::Permit
        );
        assert_eq!(headless.reads(), 0);

        // Full access: every effect is permitted, nothing is ever asked.
        let full = Harness::new(
            ComponentSource::new(authorization_policy(
                FULL_ACCESS,
                ExecutionLimits::default(),
            )),
            false,
            CancellationToken::new(),
        );
        assert_eq!(full.policy_id().package, FULL_ACCESS_POLICY);
        for effect in EFFECTS {
            assert_eq!(
                full.authorize(&identity("default"), effect).await,
                Decision::Permit,
                "{effect:?}"
            );
        }
        assert_eq!(full.reads(), 0);
        assert_eq!(full.prompts(), "");
    })
    .await;
}

/// Frozen case 2, second half: cancellation.
///
/// Authorization is bound to the active turn's cancellation scope (ADR-0024, ADR-0084 §2):
/// a turn cancelled while the question is open resolves to the cancel deny, `set_turn`
/// moves that scope, and a token that is already cancelled answers at once without asking.
#[tokio::test]
async fn cancellation() {
    within_deadline("cancellation", async {
        let scope = CancellationToken::new();
        let harness = Harness::new(
            ComponentSource::new(authorization_policy(ASK, ExecutionLimits::default())),
            false,
            scope.clone(),
        );

        // The turn is cancelled while the question is open.
        let default = identity("default");
        let first = CancellationToken::new();
        harness.set_turn(Some(first.clone()));
        let (decision, ()) = tokio::join!(harness.authorize(&default, Effect::Executes), async {
            harness.wait_for_question().await;
            first.cancel();
        });
        assert_eq!(decision, deny(CANCEL_DENY));
        assert_eq!(harness.prompts(), PROMPT, "the question was open");

        // After `set_turn(Some(new))` the constructor's token is no longer the scope: the
        // old one being cancelled does not affect a new ask, and the new one does.
        let second = CancellationToken::new();
        harness.set_turn(Some(second.clone()));
        scope.cancel();
        harness.answer("y");
        assert_eq!(
            harness.authorize(&default, Effect::Executes).await,
            Decision::Permit,
            "the cancelled old token is not the scope"
        );
        // That answer was scripted, so its read announced itself to nobody: forget it
        // before waiting for the next question.
        harness.forget_answered_questions().await;
        let (decision, ()) = tokio::join!(harness.authorize(&default, Effect::Executes), async {
            harness.wait_for_question().await;
            second.cancel();
        });
        assert_eq!(decision, deny(CANCEL_DENY));
        assert_eq!(harness.prompts(), PROMPT.repeat(3));

        // A cancel before `authorize` returns promptly: the bridge asks, then races the
        // turn's token ahead of the line source, so an already-cancelled turn reads no
        // line and never waits for one.
        let cancelled = CancellationToken::new();
        harness.set_turn(Some(cancelled.clone()));
        cancelled.cancel();
        let decision = timeout(
            Duration::from_secs(10),
            harness.authorize(&default, Effect::Executes),
        )
        .await
        .expect("a cancelled turn must not wait for a question");
        assert_eq!(decision, deny(CANCEL_DENY));
        assert_eq!(harness.reads(), 3, "the line source was not read");
    })
    .await;
}

/// Frozen case 3: a policy trap yields a conservative decision.
///
/// Neither shipped policy traps on its own and S5.3 left the guest trap to this suite, so
/// the trap is produced through the call's limits (see the module documentation): a policy
/// with `fuel: 1` cannot instantiate, and the executor's `FuelExhausted` reaches the core
/// as a `Deny` naming the package, never as a `Permit`. The deadline stop is shown beside
/// it on the context policy, the one policy call a manual epoch clock can end.
#[tokio::test]
async fn a_policy_trap_yields_a_conservative_decision() {
    within_deadline("a policy trap yields a conservative decision", async {
        let policy = authorization_policy(
            ASK,
            ExecutionLimits {
                fuel: 1,
                ..ExecutionLimits::default()
            },
        );
        let (tool_call, tool_identity) = (call(), identity("default"));
        for effect in EFFECTS {
            let verdict = policy
                .verdict(AuthorizationRequest {
                    call: &tool_call,
                    identity: &tool_identity,
                    effect,
                })
                .await;
            match verdict {
                WasmVerdict::Deny(reason) => {
                    assert!(
                        reason.starts_with(&format!("authorization policy {} failed: ", ASK.1)),
                        "{reason}"
                    );
                    assert!(
                        reason.ends_with("module call exhausted its fuel"),
                        "not the out-of-fuel stop: {reason}"
                    );
                }
                other => panic!("{effect:?}: a stopped call must deny, got {other:?}"),
            }
        }

        // Through the bridge the same deny reaches the core unchanged, and no question is
        // asked: a failed policy is never resolved by the operator.
        let harness = Harness::new(
            ComponentSource::new(policy),
            false,
            CancellationToken::new(),
        );
        harness.answer("y");
        let decision = harness
            .authorize(&identity("default"), Effect::Executes)
            .await;
        match decision {
            Decision::Deny { reason } => {
                assert!(
                    reason.starts_with(&format!("authorization policy {} failed: ", ASK.1)),
                    "{reason}"
                );
                assert!(
                    reason.ends_with("module call exhausted its fuel"),
                    "the deny is not the policy's own stop: {reason}"
                );
            }
            other => panic!("a stopped call must deny, got {other:?}"),
        }
        assert_eq!(harness.reads(), 0, "no question was asked");
        assert_eq!(harness.prompts(), "");

        // A deadline stop, on the context policy: its call parks in `summary.summarize`,
        // so advancing the manual epoch clock past the call's deadline ends it.
        let (loader, epochs) = manual_loader(&[CONTEXT]);
        let module = load(&loader, CONTEXT);
        let history = history();
        let config = summarize_soon(&history);
        let recorder = Arc::new(Recorder::default());
        recorder.gated.store(true, Ordering::SeqCst);
        let policy = watched_context(
            &module,
            &config,
            &recorder,
            ExecutionLimits {
                deadline: EPOCH_TICK * 2,
                ..ExecutionLimits::default()
            },
        );
        let cancel = CancellationToken::new();
        let (answer, ()) = tokio::join!(policy.prepare(input(&history, &cancel)), async {
            recorder.entered.notified().await;
            epochs.advance(2);
        });
        match answer {
            Err(ContextError::Failed(reason)) => {
                assert!(
                    reason.starts_with(&format!("context policy {} failed: ", CONTEXT.1)),
                    "{reason}"
                );
                assert!(
                    reason.ends_with("module call exceeded its deadline"),
                    "not the deadline stop: {reason}"
                );
            }
            Err(ContextError::Cancelled) => panic!("a deadline stop is not a cancellation"),
            Ok(_) => panic!("a stopped call must fail, not answer"),
        }
    })
    .await;
}

/// Frozen case 4: a changed approval key does not carry an old approval (F8, ADR-0084 §4).
///
/// A remembered `a`lways is keyed by the tool name, its identity and the deciding policy's
/// package name and digest. Reloading the same package name to other bytes is a different
/// policy, so the old approval does not transfer; another identity of the same tool is a
/// different key too.
#[tokio::test]
async fn a_changed_approval_key_does_not_carry_an_old_approval() {
    within_deadline(
        "a changed approval key does not carry an old approval",
        async {
            let harness = Harness::new(
                ComponentSource::new(authorization_policy(ASK, ExecutionLimits::default())),
                false,
                CancellationToken::new(),
            );

            // `a` under the installed digest: remembered for this tool and identity.
            harness.answer("a");
            assert_eq!(
                harness
                    .authorize(&identity("default"), Effect::Executes)
                    .await,
                Decision::Permit
            );
            assert_eq!(harness.reads(), 1);
            assert_eq!(
                harness
                    .authorize(&identity("default"), Effect::Executes)
                    .await,
                Decision::Permit
            );
            assert_eq!(harness.reads(), 1, "the grant answered without a question");

            // A reload to other bytes under the same name: the old approval does not carry.
            harness.switch_digest(&other_digest());
            harness.answer("n");
            assert_eq!(
                harness
                    .authorize(&identity("default"), Effect::Executes)
                    .await,
                deny(USER_DENY)
            );
            assert_eq!(harness.reads(), 2, "the replacement policy asks again");

            // Under the replacement: remembered again for this identity...
            harness.answer("a");
            assert_eq!(
                harness
                    .authorize(&identity("default"), Effect::Executes)
                    .await,
                Decision::Permit
            );
            assert_eq!(harness.reads(), 3);
            // ... and not for another variant of the same tool.
            harness.answer("n");
            assert_eq!(
                harness
                    .authorize(&identity("other"), Effect::Executes)
                    .await,
                deny(USER_DENY)
            );
            assert_eq!(harness.reads(), 4, "another identity asks again");
            assert_eq!(
                harness
                    .authorize(&identity("default"), Effect::Executes)
                    .await,
                Decision::Permit
            );
            assert_eq!(harness.reads(), 4, "the remembered identity still answers");
            assert_eq!(harness.prompts(), PROMPT.repeat(4));
        },
    )
    .await;
}
