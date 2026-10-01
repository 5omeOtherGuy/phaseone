//! The host's execution authorization policy and its native ask bridge.
//!
//! Full access is the DEFAULT: without `--ask` every call is permitted, headless
//! and interactive (ADR-0038). `--ask` opts into the restrictive policy: headless
//! permits `ReadOnly` and denies the rest; interactive permits `ReadOnly` silently
//! and asks on the terminal for anything else, racing the turn's cancellation.
//!
//! "Ask" lives HERE, inside the policy, so the core never sees UI (D18). A policy
//! (a `p1/policy/*` component) answers a [`Verdict`]: `Permit`, `Deny` or `Ask`. The
//! [`AskBridge`] resolves `Ask` through the front end's [`Asker`] (the line prompt,
//! or the TUI's approval view), so only `Permit` or `Deny` reaches the core
//! (ADR-0024).
//!
//! The verdict source is the loaded component ([`ShippedPolicy`], over
//! `p1_module_runtime::WasmAuthorizationPolicy`): the packages `p1/policy/full-access`
//! and `p1/policy/ask` are official-release HOST ENTRIES (D083b 2), loaded by package
//! name from the release manifest and verified against that same manifest, never
//! selected by `modules.lock`. With the summarizing context (`summary.rs`) they are
//! the three [`HOST_ENTRIES`]: a release missing one of them, or shipping one that
//! does not verify, fails startup naming it; there is no native default.

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision, ToolIdentity,
};
use p1_module_runtime::{
    ExecutionLimits, LoadedModule, Loader, ModuleKind, ReleaseManifest,
    Verdict as ComponentVerdict, WasmAuthorizationPolicy,
};

use crate::LineSource;
use crate::SharedWriter;
use crate::catalog::modules::{BuildLoaders, ModulesError, official_release_manifest};
use crate::render::summarize_input;
use crate::summary::CONTEXT_POLICY;

/// The exact headless refusal.
pub const HEADLESS_DENY: &str = "Not permitted in headless mode with --ask.";
/// The exact interactive refusal for anything but `y`/`a`.
pub const USER_DENY: &str = "Denied by the user.";
/// Returned when the turn is cancelled while an ask is outstanding.
pub const CANCEL_DENY: &str = "Cancelled while awaiting authorization.";

/// The manifest name of the shipped default policy (ADR-0038).
pub const FULL_ACCESS_POLICY: &str = "p1/policy/full-access";
/// The manifest name of the restrictive policy `--ask` selects.
pub const ASK_POLICY: &str = "p1/policy/ask";

/// The official-release host entries every session loads (D083b 2): the two shipped
/// authorization policies and the summarizing context policy.
pub const HOST_ENTRIES: [&str; 3] = [FULL_ACCESS_POLICY, ASK_POLICY, CONTEXT_POLICY];

/// A policy's answer, before the host resolves `Ask`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The call may run.
    Permit,
    /// Nothing is executed; the reason is shown to the model.
    Deny(String),
    /// The host decides: it asks through the front end, or denies headless.
    Ask,
}

/// Which policy decided: its package name and the digest of its component.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PolicyId {
    /// The manifest name, e.g. `p1/policy/ask`.
    pub package: String,
    /// The digest of the component's verified bytes (`sha256:…`).
    pub digest: String,
}

/// What answers the verdicts the bridge resolves: a loaded policy component.
pub trait VerdictSource: Send + Sync {
    /// The policy that answers now; part of every remembered grant's key.
    fn policy(&self) -> PolicyId;
    /// The policy's answer for `request`.
    fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict>;
}

// ------------------------------------------------------------------ the host entries

/// The verified host entries of one release, by package name.
pub type HostEntries = HashMap<&'static str, Arc<LoadedModule>>;

/// Loads every [`HOST_ENTRIES`] package from the release manifest `release_manifest`,
/// verifying each against that same manifest (official source, class, world, protocol,
/// digest, grants: `p1_module_runtime::Loader`). The first package that is missing or does
/// not verify is the error, and the error names it.
pub fn load_host_entries(release_manifest: &Path) -> Result<HostEntries, String> {
    load_host_entries_in(&BuildLoaders::default(), release_manifest)
}

/// As [`load_host_entries`], through the loader `loaders` holds for `release_manifest` (and
/// so its manifest snapshot), creating it when the build has none yet.
pub(crate) fn load_host_entries_in(
    loaders: &BuildLoaders,
    release_manifest: &Path,
) -> Result<HostEntries, String> {
    let loader = host_entry_loader(loaders, release_manifest)?;
    let mut entries = HashMap::new();
    for package in HOST_ENTRIES {
        let module = load_with(&loader, release_manifest, package)?;
        entries.insert(package, Arc::new(module));
    }
    Ok(entries)
}

/// The loader `loaders` holds for the release manifest `release_manifest` and the directory
/// it describes.
fn host_entry_loader(
    loaders: &BuildLoaders,
    release_manifest: &Path,
) -> Result<Arc<Loader>, String> {
    let unreadable = |error: String| {
        format!(
            "cannot load the host entries {}: {}: {error}",
            HOST_ENTRIES.join(", "),
            release_manifest.display()
        )
    };
    let manifest = loaders
        .manifest_for(release_manifest)
        .map_err(|error| unreadable(error.to_string()))?;
    manifest
        .check_unique_digests()
        .map_err(|error| unreadable(error.to_string()))?;
    loaders
        .for_release(release_manifest, manifest)
        .map_err(|error| unreadable(error.to_string()))
}

/// The host entry `package` through `loader`, its refusal naming the package.
fn load_with(
    loader: &Loader,
    release_manifest: &Path,
    package: &str,
) -> Result<LoadedModule, String> {
    loader.load(package).map_err(|error| {
        format!(
            "host entry {package} from {}: {error}",
            release_manifest.display()
        )
    })
}

/// The official release's manifest: the share tree's, or in a debug build the built set's
/// (D080, `official_release_manifest`).
fn official_manifest() -> Result<PathBuf, String> {
    official_release_manifest().ok_or_else(|| ModulesError::NoRelease.to_string())
}

/// The host entry `package` of the official release. Load afresh so an in-place
/// replacement at the same manifest path cannot retain a previous generation's bytes.
pub fn host_entry(package: &str) -> Result<Arc<LoadedModule>, String> {
    build_host_entry(&BuildLoaders::default(), package)
}

/// The host entry `package` of the official release as the catalog build that `loaders`
/// belongs to loaded it: one loader, one engine and one compile per entry for the whole
/// build. A `/modules reload` builds with new loaders, so it still reads an in-place
/// replacement at the same manifest path afresh.
pub(crate) fn build_host_entry(
    loaders: &BuildLoaders,
    package: &str,
) -> Result<Arc<LoadedModule>, String> {
    let release = official_manifest()?;
    loaders
        .host_entries(&release)?
        .get(package)
        .cloned()
        .ok_or_else(|| format!("{package} is not one of p1's host entries"))
}

/// The package the session's mode selects: full access by default (ADR-0038), the
/// restrictive policy with `--ask`.
pub fn shipped_package(ask: bool) -> &'static str {
    if ask { ASK_POLICY } else { FULL_ACCESS_POLICY }
}

// ------------------------------------------------------------------ the shipped policy

/// The release one catalog build loaded from: its manifest path and the build's own loader,
/// which holds the manifest snapshot every package of that build was verified against.
#[derive(Clone)]
pub(crate) struct BuildRelease {
    pub(crate) path: PathBuf,
    pub(crate) loader: Arc<Loader>,
}

/// One loaded authorization-policy component and the id grants are keyed by.
struct PolicyComponent {
    id: PolicyId,
    module: Arc<LoadedModule>,
    limits: ExecutionLimits,
    /// Built on first use when the component was loaded outside a Tokio runtime, whose
    /// executor it needs; inside one it is built at once.
    adapter: OnceLock<Result<WasmAuthorizationPolicy, String>>,
}

impl PolicyComponent {
    fn new(module: Arc<LoadedModule>, limits: ExecutionLimits) -> Result<Self, String> {
        if module.kind() != ModuleKind::AuthorizationPolicy {
            return Err(format!(
                "host entry {} is a {} package, not an authorization policy",
                module.name(),
                module.kind().name()
            ));
        }
        let component = Self {
            id: PolicyId {
                package: module.name().to_owned(),
                digest: module.digest().to_string(),
            },
            module,
            limits,
            adapter: OnceLock::new(),
        };
        // A policy that cannot be built fails where it is loaded, when a runtime is there
        // to build it on.
        if tokio::runtime::Handle::try_current().is_ok() {
            component.adapter().map_err(Clone::clone)?;
        }
        Ok(component)
    }

    fn adapter(&self) -> Result<&WasmAuthorizationPolicy, &String> {
        self.adapter
            .get_or_init(|| {
                WasmAuthorizationPolicy::new(&self.module, self.limits)
                    .map_err(|error| error.to_string())
            })
            .as_ref()
    }
}

/// A shipped authorization policy as the bridge's [`VerdictSource`]: the host entry
/// `p1/policy/full-access` or `p1/policy/ask`, answering through its component. Its
/// [`PolicyId`] is the package name and the digest of the verified component bytes, so a
/// grant remembered under one artifact never answers for another (F8). A trap, a fuel or
/// deadline stop, or a component that could not be built answers a `Deny` naming the
/// policy, never a `Permit`. A `/modules reload` loads the package again from the release
/// ([`ShippedPolicy::reload`]); the component is swapped only once the reload installed.
pub struct ShippedPolicy {
    current: Mutex<Arc<PolicyComponent>>,
}

impl ShippedPolicy {
    /// The official release's host entry for the session's mode: full access, or `ask`
    /// with `--ask`.
    pub fn official(ask: bool) -> Result<Arc<Self>, String> {
        Self::from_module(host_entry(shipped_package(ask))?)
    }

    /// The policy over an already verified authorization-policy `module`.
    pub fn from_module(module: Arc<LoadedModule>) -> Result<Arc<Self>, String> {
        Self::with_limits(module, ExecutionLimits::default())
    }

    /// As [`ShippedPolicy::from_module`], each call bounded by `limits` instead of the
    /// executor's defaults.
    pub fn with_limits(
        module: Arc<LoadedModule>,
        limits: ExecutionLimits,
    ) -> Result<Arc<Self>, String> {
        Ok(Arc::new(Self {
            current: Mutex::new(Arc::new(PolicyComponent::new(module, limits)?)),
        }))
    }

    /// The verified component currently answering; journal provenance uses its actual digest.
    pub(crate) fn loaded_module(&self) -> Arc<LoadedModule> {
        self.current().module.clone()
    }

    fn current(&self) -> Arc<PolicyComponent> {
        self.current
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    /// Loads this policy's package again from the official release and verifies it. Nothing
    /// changes until [`PolicyReload::install`]; a failure leaves the current component
    /// answering.
    pub fn reload(self: &Arc<Self>) -> Result<PolicyReload, String> {
        let release = official_manifest()?;
        let loader = host_entry_loader(&BuildLoaders::default(), &release)?;
        self.reload_through(&loader, &release)
    }

    /// As [`ShippedPolicy::reload`], but through the loader (and so the manifest snapshot)
    /// the catalog build of the same reload used, so one generation never mixes packages of
    /// two states of one installation.
    pub(crate) fn reload_from(
        self: &Arc<Self>,
        release: &BuildRelease,
    ) -> Result<PolicyReload, String> {
        self.reload_through(&release.loader, &release.path)
    }

    fn reload_through(
        self: &Arc<Self>,
        loader: &Loader,
        release: &Path,
    ) -> Result<PolicyReload, String> {
        let current = self.current();
        let module = load_with(loader, release, &current.id.package)?;
        Ok(PolicyReload {
            policy: self.clone(),
            component: Arc::new(PolicyComponent::new(Arc::new(module), current.limits)?),
        })
    }
}

impl VerdictSource for ShippedPolicy {
    fn policy(&self) -> PolicyId {
        self.current().id.clone()
    }

    fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict> {
        let component = self.current();
        Box::pin(async move {
            match component.adapter() {
                Ok(adapter) => match adapter.verdict(request).await {
                    ComponentVerdict::Permit => Verdict::Permit,
                    ComponentVerdict::Deny(reason) => Verdict::Deny(reason),
                    ComponentVerdict::Ask => Verdict::Ask,
                },
                Err(reason) => Verdict::Deny(format!(
                    "authorization policy {} failed: {reason}",
                    component.id.package
                )),
            }
        })
    }
}

/// A shipped policy's package loaded again, not yet answering.
pub struct PolicyReload {
    policy: Arc<ShippedPolicy>,
    component: Arc<PolicyComponent>,
}

impl PolicyReload {
    /// The id the policy answers under once installed.
    pub fn policy(&self) -> PolicyId {
        self.component.id.clone()
    }

    /// The verified candidate module, before its generation is installed.
    pub(crate) fn loaded_module(&self) -> Arc<LoadedModule> {
        self.component.module.clone()
    }

    /// A generation-scoped policy; installing it cannot replace the policy of a running child.
    pub(crate) fn fresh(&self) -> Arc<ShippedPolicy> {
        Arc::new(ShippedPolicy {
            current: Mutex::new(self.component.clone()),
        })
    }

    /// The reloaded component answers from now on.
    pub fn install(self) {
        *self
            .policy
            .current
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = self.component;
    }
}

/// An `always` grant binds the tool's name, identity and verified package digest, plus
/// the deciding policy's package name and digest. Native tools have no package digest.
type GrantKey = (String, ToolIdentity, PolicyId, Option<String>);

/// The operator's answer to one ask.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OperatorAnswer {
    /// Permit this call.
    Yes,
    /// Deny this call ([`USER_DENY`]).
    No,
    /// Permit this call and remember the grant for this process.
    Always,
}

/// How the bridge asks the operator: the line front end's prompt ([`LineAsker`]) or
/// the TUI's approval view. The asker only asks; the rules (which verdict asks, which
/// grant is remembered) stay in the bridge.
///
/// The turn's cancellation race stays in the bridge too: it races `ask` against the
/// active turn's token and drops the question when the turn is cancelled
/// ([`CANCEL_DENY`]).
pub trait Asker: Send + Sync {
    /// Ask the operator about `request`. `None` when no operator can answer any more
    /// (the UI is gone): the bridge denies with [`CANCEL_DENY`], nothing runs unanswered.
    fn ask<'a>(
        &'a self,
        request: AuthorizationRequest<'a>,
    ) -> BoxFuture<'a, Option<OperatorAnswer>>;

    /// The front end's live turn token, forwarded by [`AskBridge::set_turn`] for an
    /// asker that races the turn itself as well.
    fn set_turn(&self, _token: Option<CancellationToken>) {}
}

/// The line front end's asker: the prompt on stderr, the answer from the next line.
pub struct LineAsker {
    lines: Arc<dyn LineSource>,
    stderr: SharedWriter,
}

impl LineAsker {
    pub fn new(lines: Arc<dyn LineSource>, stderr: SharedWriter) -> Self {
        Self { lines, stderr }
    }
}

impl Asker for LineAsker {
    fn ask<'a>(
        &'a self,
        request: AuthorizationRequest<'a>,
    ) -> BoxFuture<'a, Option<OperatorAnswer>> {
        // The prompt is written when the question is asked, before the bridge races
        // the line against the turn's cancellation.
        let tool = &request.call.name;
        let summary = summarize_input(request.call.input.raw());
        let prompt = format!("allow {tool} {summary}? [y]es / [n]o / [a]lways for this tool: ");
        {
            let mut writer = self.stderr.lock().unwrap();
            let _ = writer.write_all(prompt.as_bytes());
            let _ = writer.flush();
        }
        Box::pin(async move {
            let line = self.lines.next_line().await;
            Some(match line.as_deref().map(str::trim) {
                Some("y") | Some("yes") => OperatorAnswer::Yes,
                Some("a") | Some("always") => OperatorAnswer::Always,
                _ => OperatorAnswer::No,
            })
        })
    }
}

/// The native ask bridge: a [`VerdictSource`] plus the front end's [`Asker`].
///
/// Authorization is bound to the active turn's cancellation scope: the bridge
/// holds the turn's token, which the front end sets with [`AskBridge::set_turn`];
/// until then (and after `set_turn(None)`) the constructor's token is the scope.
pub struct AskBridge {
    source: Arc<dyn VerdictSource>,
    headless: bool,
    asker: Arc<dyn Asker>,
    /// The scope when no turn token is set.
    scope: CancellationToken,
    /// The live turn's token, set by the front end.
    turn: Arc<Mutex<Option<CancellationToken>>>,
    /// Grants answered with `a`lways for this process.
    always: Arc<Mutex<HashSet<GrantKey>>>,
    /// This bridge's generation, not a live process-wide map of the latest release.
    tool_sources: Mutex<Option<Arc<crate::catalog::modules::VerifiedSources>>>,
}

impl AskBridge {
    /// The line front end's bridge: [`AskBridge::with_asker`] over a [`LineAsker`].
    pub fn new(
        source: Arc<dyn VerdictSource>,
        headless: bool,
        lines: Arc<dyn LineSource>,
        stderr: SharedWriter,
        cancel: CancellationToken,
    ) -> Self {
        Self::with_asker(
            source,
            headless,
            Arc::new(LineAsker::new(lines, stderr)),
            cancel,
        )
    }

    /// A bridge asking through `asker`.
    pub fn with_asker(
        source: Arc<dyn VerdictSource>,
        headless: bool,
        asker: Arc<dyn Asker>,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            source,
            headless,
            asker,
            scope: cancel,
            turn: Arc::new(Mutex::new(None)),
            always: Arc::new(Mutex::new(HashSet::new())),
            tool_sources: Mutex::new(None),
        }
    }

    /// Rebind a new generation to its own verified policy while preserving the UI asker and
    /// grants; old bridges keep their old verdict source for in-flight workers.
    pub(crate) fn with_source(&self, source: Arc<ShippedPolicy>) -> Self {
        Self {
            source,
            headless: self.headless,
            asker: self.asker.clone(),
            scope: self.scope.clone(),
            turn: self.turn.clone(),
            always: self.always.clone(),
            tool_sources: Mutex::new(self.tool_sources.lock().unwrap().clone()),
        }
    }

    pub(crate) fn bind_sources(&self, sources: Arc<crate::catalog::modules::VerifiedSources>) {
        *self.tool_sources.lock().unwrap() = Some(sources);
    }

    /// The front end marks the live turn's token; `None` returns to the
    /// constructor's token. The asker is told as well.
    pub fn set_turn(&self, token: Option<CancellationToken>) {
        *self.turn.lock().unwrap() = token.clone();
        self.asker.set_turn(token);
    }

    /// The token the current authorization races.
    fn active_turn(&self) -> CancellationToken {
        self.turn
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.scope.clone())
    }
}

fn cancelled() -> Decision {
    Decision::Deny {
        reason: CANCEL_DENY.to_string(),
    }
}

impl AuthorizationPolicy for AskBridge {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            let turn = self.active_turn();
            let policy = self.source.policy();
            // A verdict that is ready answers first; one still computing loses to the
            // turn's cancellation.
            let verdict = tokio::select! {
                biased;
                verdict = self.source.verdict(request.clone()) => verdict,
                _ = turn.cancelled() => return cancelled(),
            };
            match verdict {
                Verdict::Permit => return Decision::Permit,
                Verdict::Deny(reason) => return Decision::Deny { reason },
                Verdict::Ask => {}
            }
            if self.headless {
                return Decision::Deny {
                    reason: HEADLESS_DENY.to_string(),
                };
            }

            let digest = self
                .tool_sources
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|sources| {
                    sources.digest_for_implementation(&request.identity.implementation)
                });
            let key = (
                request.call.name.clone(),
                request.identity.clone(),
                policy,
                digest,
            );
            if self.always.lock().unwrap().contains(&key) {
                return Decision::Permit;
            }

            let asking = self.asker.ask(request);
            let answer = tokio::select! {
                biased;
                _ = turn.cancelled() => return cancelled(),
                answer = asking => answer,
            };
            match answer {
                Some(OperatorAnswer::Yes) => Decision::Permit,
                Some(OperatorAnswer::Always) => {
                    self.always.lock().unwrap().insert(key);
                    Decision::Permit
                }
                Some(OperatorAnswer::No) => Decision::Deny {
                    reason: USER_DENY.to_string(),
                },
                None => cancelled(),
            }
        })
    }
}

/// The line front end's authorization policy: the [`AskBridge`] over the shipped
/// policy of its mode ([`ShippedPolicy::official`]), full access without `--ask` and
/// the restrictive policy with it.
pub struct HostPolicy {
    bridge: AskBridge,
    shipped: Arc<ShippedPolicy>,
}

impl HostPolicy {
    /// Fails, naming the package, when the official release lacks a host entry or ships
    /// one that does not verify.
    pub fn new(
        ask: bool,
        headless: bool,
        lines: Arc<dyn LineSource>,
        stderr: SharedWriter,
        cancel: CancellationToken,
    ) -> Result<Self, String> {
        let shipped = ShippedPolicy::official(ask)?;
        Ok(Self {
            bridge: AskBridge::new(shipped.clone(), headless, lines, stderr, cancel),
            shipped,
        })
    }

    pub(crate) fn bind_sources(&self, sources: Arc<crate::catalog::modules::VerifiedSources>) {
        self.bridge.bind_sources(sources);
    }

    /// Clone the bridge for one freshly loaded policy generation.
    pub(crate) fn with_shipped(&self, shipped: Arc<ShippedPolicy>) -> Self {
        Self {
            bridge: self.bridge.with_source(shipped.clone()),
            shipped,
        }
    }

    /// The shipped policy the bridge asks: what a `/modules reload` loads again.
    pub fn shipped(&self) -> &Arc<ShippedPolicy> {
        &self.shipped
    }

    /// Marks the live turn's token, as [`AskBridge::set_turn`].
    pub fn set_turn(&self, token: Option<CancellationToken>) {
        self.bridge.set_turn(token);
    }
}

impl AuthorizationPolicy for HostPolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        self.bridge.authorize(request)
    }
}

#[cfg(test)]
mod tests {
    use std::future::Future;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use p1_contracts::{Effect, ToolCall, ToolInput};
    use tokio::sync::mpsc;

    use super::*;

    /// A verdict source answering one scripted verdict under a policy id the test
    /// can change, as a reload would.
    struct ScriptedSource {
        verdict: Verdict,
        policy: Mutex<PolicyId>,
    }

    impl ScriptedSource {
        fn new(verdict: Verdict) -> Arc<Self> {
            Arc::new(Self {
                verdict,
                policy: Mutex::new(PolicyId {
                    package: ASK_POLICY.to_string(),
                    digest: "sha256:one".to_string(),
                }),
            })
        }

        fn set_digest(&self, digest: &str) {
            self.policy.lock().unwrap().digest = digest.to_string();
        }
    }

    impl VerdictSource for ScriptedSource {
        fn policy(&self) -> PolicyId {
            self.policy.lock().unwrap().clone()
        }

        fn verdict<'a>(&'a self, _request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict> {
            Box::pin(async move { self.verdict.clone() })
        }
    }

    /// Lines the test sends; each read announces itself on `asked` first, so a
    /// case knows the question is open without sleeping.
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

    /// A writer into a buffer the test reads back.
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

    struct Harness {
        bridge: AskBridge,
        source: Arc<ScriptedSource>,
        lines: Arc<ScriptedLines>,
        send: mpsc::UnboundedSender<String>,
        asked: mpsc::UnboundedReceiver<()>,
        stderr: Buffer,
        scope: CancellationToken,
    }

    impl Harness {
        fn new(verdict: Verdict, headless: bool) -> Self {
            let source = ScriptedSource::new(verdict);
            let (send, receiver) = mpsc::unbounded_channel();
            let (asked_tx, asked) = mpsc::unbounded_channel();
            let lines = Arc::new(ScriptedLines {
                lines: tokio::sync::Mutex::new(receiver),
                asked: asked_tx,
                reads: AtomicUsize::new(0),
            });
            let stderr = Buffer::default();
            let scope = CancellationToken::new();
            let bridge = AskBridge::new(
                source.clone(),
                headless,
                lines.clone(),
                Arc::new(Mutex::new(Box::new(stderr.clone()))),
                scope.clone(),
            );
            Self {
                bridge,
                source,
                lines,
                send,
                asked,
                stderr,
                scope,
            }
        }

        fn answer(&self, line: &str) {
            self.send.send(line.to_string()).unwrap();
        }

        fn reads(&self) -> usize {
            self.lines.reads.load(Ordering::SeqCst)
        }

        fn prompts(&self) -> String {
            String::from_utf8(self.stderr.0.lock().unwrap().clone()).unwrap()
        }

        async fn authorize(&self, effect: Effect) -> Decision {
            let call = call();
            let identity = identity();
            self.bridge
                .authorize(AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect,
                })
                .await
        }
    }

    fn call() -> ToolCall {
        ToolCall {
            call_id: "c1".to_string(),
            name: "shell".to_string(),
            input: ToolInput::Text("ls -la".to_string()),
        }
    }

    fn identity() -> ToolIdentity {
        ToolIdentity {
            implementation: "p1-tool-shell".to_string(),
            variant: "default".to_string(),
        }
    }

    const PROMPT: &str = "allow shell ls -la? [y]es / [n]o / [a]lways for this tool: ";

    fn deny(reason: &str) -> Decision {
        Decision::Deny {
            reason: reason.to_string(),
        }
    }

    #[tokio::test]
    async fn permit_and_deny_pass_through_untouched() {
        for headless in [false, true] {
            let harness = Harness::new(Verdict::Permit, headless);
            assert_eq!(harness.authorize(Effect::Executes).await, Decision::Permit);
            let harness = Harness::new(Verdict::Deny("policy says no".to_string()), headless);
            assert_eq!(
                harness.authorize(Effect::Executes).await,
                deny("policy says no")
            );
            assert_eq!(harness.reads(), 0);
            assert_eq!(harness.prompts(), "");
        }
    }

    #[tokio::test]
    async fn ask_headless_denies_without_reading_a_line() {
        let harness = Harness::new(Verdict::Ask, true);
        assert_eq!(
            harness.authorize(Effect::WritesFiles).await,
            deny(HEADLESS_DENY)
        );
        assert_eq!(harness.reads(), 0);
        assert_eq!(harness.prompts(), "");
    }

    #[tokio::test]
    async fn ask_interactive_y_permits_and_n_denies() {
        let harness = Harness::new(Verdict::Ask, false);
        harness.answer("y");
        assert_eq!(harness.authorize(Effect::Executes).await, Decision::Permit);
        assert_eq!(harness.prompts(), PROMPT);
        harness.answer("n");
        assert_eq!(harness.authorize(Effect::Executes).await, deny(USER_DENY));
        harness.answer("whatever");
        assert_eq!(harness.authorize(Effect::Executes).await, deny(USER_DENY));
        assert_eq!(harness.reads(), 3);
        assert_eq!(harness.prompts(), PROMPT.repeat(3));
    }

    #[tokio::test]
    async fn ask_interactive_a_permits_and_remembers() {
        let harness = Harness::new(Verdict::Ask, false);
        harness.answer("a");
        assert_eq!(harness.authorize(Effect::Executes).await, Decision::Permit);
        assert_eq!(harness.authorize(Effect::Executes).await, Decision::Permit);
        assert_eq!(harness.reads(), 1);
        assert_eq!(harness.prompts(), PROMPT);
    }

    #[tokio::test]
    async fn a_grant_is_not_reused_for_another_policy_digest() {
        let harness = Harness::new(Verdict::Ask, false);
        harness.answer("a");
        assert_eq!(harness.authorize(Effect::Executes).await, Decision::Permit);
        harness.source.set_digest("sha256:two");
        harness.answer("n");
        assert_eq!(harness.authorize(Effect::Executes).await, deny(USER_DENY));
        assert_eq!(harness.reads(), 2);
    }

    #[tokio::test]
    async fn a_turn_cancelled_while_asking_is_cancel_deny() {
        let mut harness = Harness::new(Verdict::Ask, false);
        let scope = harness.scope.clone();
        let mut asked = std::mem::replace(&mut harness.asked, mpsc::unbounded_channel().1);
        let (decision, ()) = tokio::join!(harness.authorize(Effect::Executes), async {
            asked.recv().await.unwrap();
            scope.cancel();
        });
        assert_eq!(decision, deny(CANCEL_DENY));
        assert_eq!(harness.prompts(), PROMPT);
    }

    #[tokio::test]
    async fn set_turn_selects_the_token_raced() {
        let mut harness = Harness::new(Verdict::Ask, false);
        let turn = CancellationToken::new();
        harness.bridge.set_turn(Some(turn.clone()));
        // The constructor's token is no longer the scope.
        harness.scope.cancel();
        harness.answer("y");
        assert_eq!(harness.authorize(Effect::Executes).await, Decision::Permit);

        let mut asked = std::mem::replace(&mut harness.asked, mpsc::unbounded_channel().1);
        asked.recv().await.unwrap();
        let (decision, ()) = tokio::join!(harness.authorize(Effect::Executes), async {
            asked.recv().await.unwrap();
            turn.cancel();
        });
        assert_eq!(decision, deny(CANCEL_DENY));

        // Back to the constructor's token, which is cancelled.
        harness.bridge.set_turn(None);
        assert_eq!(harness.authorize(Effect::Executes).await, deny(CANCEL_DENY));
    }

    /// An asker answering scripted answers in order, counting every question.
    struct ScriptedAsker {
        answers: Mutex<Vec<OperatorAnswer>>,
        asked: AtomicUsize,
    }

    impl ScriptedAsker {
        fn new(answers: &[OperatorAnswer]) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.iter().rev().copied().collect()),
                asked: AtomicUsize::new(0),
            })
        }

        fn asked(&self) -> usize {
            self.asked.load(Ordering::SeqCst)
        }
    }

    impl Asker for ScriptedAsker {
        fn ask<'a>(
            &'a self,
            _request: AuthorizationRequest<'a>,
        ) -> BoxFuture<'a, Option<OperatorAnswer>> {
            self.asked.fetch_add(1, Ordering::SeqCst);
            let answer = self.answers.lock().unwrap().pop();
            Box::pin(async move { answer })
        }
    }

    async fn authorize_with(bridge: &AskBridge, effect: Effect) -> Decision {
        let (call, identity) = (call(), identity());
        bridge
            .authorize(AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect,
            })
            .await
    }

    #[tokio::test]
    async fn the_asker_is_never_asked_for_permit_or_deny() {
        for verdict in [Verdict::Permit, Verdict::Deny("policy says no".to_string())] {
            let asker = ScriptedAsker::new(&[OperatorAnswer::No]);
            let bridge = AskBridge::with_asker(
                ScriptedSource::new(verdict.clone()),
                false,
                asker.clone(),
                CancellationToken::new(),
            );
            let expected = match verdict {
                Verdict::Deny(reason) => Decision::Deny { reason },
                _ => Decision::Permit,
            };
            for effect in [Effect::ReadOnly, Effect::Executes] {
                assert_eq!(authorize_with(&bridge, effect).await, expected);
            }
            assert_eq!(asker.asked(), 0);
        }
    }

    #[tokio::test]
    async fn always_keys_the_grant_with_the_policy_id() {
        let source = ScriptedSource::new(Verdict::Ask);
        let asker = ScriptedAsker::new(&[
            OperatorAnswer::Always,
            OperatorAnswer::Always,
            OperatorAnswer::No,
        ]);
        let bridge = AskBridge::with_asker(
            source.clone(),
            false,
            asker.clone(),
            CancellationToken::new(),
        );
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            Decision::Permit
        );
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            Decision::Permit
        );
        assert_eq!(asker.asked(), 1);
        let key = (call().name, identity(), source.policy(), None);
        assert!(bridge.always.lock().unwrap().contains(&key));

        // Another digest is another policy: asked again, granted under the new id.
        source.set_digest("sha256:two");
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            Decision::Permit
        );
        assert_eq!(asker.asked(), 2);
        assert_eq!(bridge.always.lock().unwrap().len(), 2);
        let key = (call().name, identity(), source.policy(), None);
        assert!(bridge.always.lock().unwrap().contains(&key));

        // Another package with the first digest is not granted either.
        source.set_digest("sha256:one");
        source.policy.lock().unwrap().package = FULL_ACCESS_POLICY.to_string();
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            deny(USER_DENY)
        );
        assert_eq!(asker.asked(), 3);
    }

    #[tokio::test]
    async fn no_operator_left_is_cancel_deny() {
        let asker = ScriptedAsker::new(&[]);
        let bridge = AskBridge::with_asker(
            ScriptedSource::new(Verdict::Ask),
            false,
            asker.clone(),
            CancellationToken::new(),
        );
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            deny(CANCEL_DENY)
        );
        assert_eq!(asker.asked(), 1);
    }

    #[tokio::test]
    async fn replacing_verified_tool_digest_voids_an_always_grant() {
        let source = ScriptedSource::new(Verdict::Ask);
        let asker = ScriptedAsker::new(&[OperatorAnswer::Always, OperatorAnswer::Always]);
        let bridge = AskBridge::with_asker(source, false, asker.clone(), CancellationToken::new());
        let sources = Arc::new(crate::catalog::modules::VerifiedSources::default());
        sources.set_digest_for_test("shell", &identity().implementation, "sha256:old");
        bridge.bind_sources(sources.clone());
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            Decision::Permit
        );
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            Decision::Permit
        );
        assert_eq!(asker.asked(), 1);
        sources.set_digest_for_test("shell", &identity().implementation, "sha256:new");
        assert_eq!(
            authorize_with(&bridge, Effect::Executes).await,
            Decision::Permit
        );
        assert_eq!(
            asker.asked(),
            2,
            "new verified bytes cannot reuse prior approval"
        );
    }

    #[test]
    fn a_reloaded_policy_generation_does_not_mutate_the_old_verdict_source() {
        let old = ShippedPolicy::official(false).expect("old release policy");
        let old_module = old.loaded_module();
        let candidate = old.reload().expect("reload candidate");
        let fresh = candidate.fresh();
        assert!(!Arc::ptr_eq(&old, &fresh));
        assert!(Arc::ptr_eq(&old.loaded_module(), &old_module));
        assert!(!Arc::ptr_eq(&fresh.loaded_module(), &old_module));
        let asker = ScriptedAsker::new(&[OperatorAnswer::No]);
        let bridge = AskBridge::with_asker(old.clone(), false, asker, CancellationToken::new());
        let rebound = bridge.with_source(fresh);
        assert_eq!(bridge.source.policy(), old.policy());
        assert_eq!(rebound.source.policy(), candidate.policy());
        assert!(Arc::ptr_eq(&bridge.always, &rebound.always));
    }

    #[tokio::test]
    async fn host_policy_selects_full_access_by_default_and_ask_with_the_flag() {
        let policy = |ask| {
            HostPolicy::new(
                ask,
                true,
                Arc::new(crate::StdinLines::new()),
                Arc::new(Mutex::new(Box::new(std::io::sink()))),
                CancellationToken::new(),
            )
            .expect("the official release ships both policies")
        };
        let (call, identity) = (call(), identity());
        let request = |effect| AuthorizationRequest {
            call: &call,
            identity: &identity,
            effect,
        };
        let full = policy(false);
        assert_eq!(full.bridge.source.policy().package, FULL_ACCESS_POLICY);
        let ask = policy(true);
        assert_eq!(ask.bridge.source.policy().package, ASK_POLICY);
        // The grant key carries the digest of the component the release pins, never a
        // placeholder.
        for (host, package) in [(&full, FULL_ACCESS_POLICY), (&ask, ASK_POLICY)] {
            let module = host_entry(package).expect("the host entry loads");
            assert_eq!(
                host.bridge.source.policy().digest,
                module.digest().to_string()
            );
            assert_eq!(host.shipped().policy(), host.bridge.source.policy());
        }
        for effect in [
            Effect::ReadOnly,
            Effect::WritesFiles,
            Effect::Executes,
            Effect::Delegates,
        ] {
            assert_eq!(full.authorize(request(effect)).await, Decision::Permit);
            let expected = if effect == Effect::ReadOnly {
                Decision::Permit
            } else {
                deny(HEADLESS_DENY)
            };
            assert_eq!(ask.authorize(request(effect)).await, expected);
        }
    }

    #[test]
    fn one_build_loads_its_host_entries_once_and_a_new_build_again() {
        let loaders = BuildLoaders::default();
        let first = build_host_entry(&loaders, CONTEXT_POLICY).expect("the host entry loads");
        let again = build_host_entry(&loaders, CONTEXT_POLICY).expect("the host entry loads");
        assert!(Arc::ptr_eq(&first, &again), "one build compiles it once");
        // A new build (a reload, `BuildLoaders::clear`) reads the release again.
        loaders.clear();
        let reloaded = build_host_entry(&loaders, CONTEXT_POLICY).expect("the host entry loads");
        assert!(!Arc::ptr_eq(&first, &reloaded));
        assert_eq!(first.digest(), reloaded.digest());
        assert!(!Arc::ptr_eq(&host_entry(CONTEXT_POLICY).unwrap(), &first));
    }
}
