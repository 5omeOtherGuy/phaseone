//! The host's execution authorization policy and its native ask bridge.
//!
//! Full access is the DEFAULT: without `--ask` every call is permitted, headless
//! and interactive (ADR-0038). `--ask` opts into the restrictive policy: headless
//! permits `ReadOnly` and denies the rest; interactive permits `ReadOnly` silently
//! and asks on the terminal for anything else, racing the turn's cancellation.
//!
//! "Ask" lives HERE, inside the policy, so the core never sees UI (D18). A policy
//! (a `p1/policy/*` component, or its native twin below) answers a [`Verdict`]:
//! `Permit`, `Deny` or `Ask`. The [`AskBridge`] resolves `Ask` through the line
//! front end, so only `Permit` or `Deny` reaches the core (ADR-0024).
//!
//! The verdict sources are native for now: the host has no module loader yet, so
//! [`NativeFullAccess`] and [`NativeAsk`] carry exactly the rules the packages
//! `p1/policy/full-access` and `p1/policy/ask` implement. The loaded components
//! (`p1_module_runtime::WasmAuthorizationPolicy`, whose `name`, `digest` and
//! `verdict` match [`VerdictSource`]) replace them when the host loads modules.

use std::collections::HashSet;
use std::io::Write;
use std::sync::{Arc, Mutex};

use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision, Effect,
    ToolIdentity,
};

use crate::LineSource;
use crate::SharedWriter;
use crate::render::summarize_input;

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
/// The digest a native verdict source reports: it has no component bytes.
pub const NATIVE_DIGEST: &str = "native";

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
    /// The component digest (`sha256:…`), or [`NATIVE_DIGEST`].
    pub digest: String,
}

/// What answers the verdicts the bridge resolves: a loaded policy component, or a
/// native one with the same rules.
pub trait VerdictSource: Send + Sync {
    /// The policy that answers now; part of every remembered grant's key.
    fn policy(&self) -> PolicyId;
    /// The policy's answer for `request`.
    fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict>;
}

/// `p1/policy/full-access`, natively: every call is permitted.
pub struct NativeFullAccess;

impl VerdictSource for NativeFullAccess {
    fn policy(&self) -> PolicyId {
        PolicyId {
            package: FULL_ACCESS_POLICY.to_string(),
            digest: NATIVE_DIGEST.to_string(),
        }
    }

    fn verdict<'a>(&'a self, _request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict> {
        Box::pin(async { Verdict::Permit })
    }
}

/// `p1/policy/ask`, natively: `ReadOnly` is permitted, every other effect asks.
pub struct NativeAsk;

impl VerdictSource for NativeAsk {
    fn policy(&self) -> PolicyId {
        PolicyId {
            package: ASK_POLICY.to_string(),
            digest: NATIVE_DIGEST.to_string(),
        }
    }

    fn verdict<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict> {
        Box::pin(async move {
            if request.effect == Effect::ReadOnly {
                Verdict::Permit
            } else {
                Verdict::Ask
            }
        })
    }
}

/// A grant remembered with `a`lways: the tool name, its `ToolIdentity`, and the
/// deciding policy's package name and digest.
type GrantKey = (String, ToolIdentity, PolicyId);

/// The native ask bridge: a [`VerdictSource`] plus the line front end's asker.
///
/// Authorization is bound to the active turn's cancellation scope: the bridge
/// holds the turn's token, which the front end sets with [`AskBridge::set_turn`];
/// until then (and after `set_turn(None)`) the constructor's token is the scope.
pub struct AskBridge {
    source: Arc<dyn VerdictSource>,
    headless: bool,
    lines: Arc<dyn LineSource>,
    stderr: SharedWriter,
    /// The scope when no turn token is set.
    scope: CancellationToken,
    /// The live turn's token, set by the front end.
    turn: Mutex<Option<CancellationToken>>,
    /// Grants answered with `a`lways for this process.
    always: Mutex<HashSet<GrantKey>>,
}

impl AskBridge {
    pub fn new(
        source: Arc<dyn VerdictSource>,
        headless: bool,
        lines: Arc<dyn LineSource>,
        stderr: SharedWriter,
        cancel: CancellationToken,
    ) -> Self {
        Self {
            source,
            headless,
            lines,
            stderr,
            scope: cancel,
            turn: Mutex::new(None),
            always: Mutex::new(HashSet::new()),
        }
    }

    /// The front end marks the live turn's token; `None` returns to the
    /// constructor's token.
    pub fn set_turn(&self, token: Option<CancellationToken>) {
        *self.turn.lock().unwrap() = token;
    }

    /// The token the current authorization races.
    fn active_turn(&self) -> CancellationToken {
        self.turn
            .lock()
            .unwrap()
            .clone()
            .unwrap_or_else(|| self.scope.clone())
    }

    fn ask(&self, tool: &str, summary: &str) {
        let prompt = format!("allow {tool} {summary}? [y]es / [n]o / [a]lways for this tool: ");
        let mut writer = self.stderr.lock().unwrap();
        let _ = writer.write_all(prompt.as_bytes());
        let _ = writer.flush();
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

            let key = (request.call.name.clone(), request.identity.clone(), policy);
            if self.always.lock().unwrap().contains(&key) {
                return Decision::Permit;
            }

            let summary = summarize_input(request.call.input.raw());
            self.ask(&request.call.name, &summary);
            let line = tokio::select! {
                biased;
                _ = turn.cancelled() => return cancelled(),
                line = self.lines.next_line() => line,
            };
            match line.as_deref().map(str::trim) {
                Some("y") | Some("yes") => Decision::Permit,
                Some("a") | Some("always") => {
                    self.always.lock().unwrap().insert(key);
                    Decision::Permit
                }
                _ => Decision::Deny {
                    reason: USER_DENY.to_string(),
                },
            }
        })
    }
}

/// The line front end's authorization policy: the [`AskBridge`] over the native
/// verdict source of its mode, full access without `--ask` and the restrictive
/// policy with it.
pub struct HostPolicy {
    bridge: AskBridge,
}

impl HostPolicy {
    pub fn new(
        ask: bool,
        headless: bool,
        lines: Arc<dyn LineSource>,
        stderr: SharedWriter,
        cancel: CancellationToken,
    ) -> Self {
        let source: Arc<dyn VerdictSource> = if ask {
            Arc::new(NativeAsk)
        } else {
            Arc::new(NativeFullAccess)
        };
        Self {
            bridge: AskBridge::new(source, headless, lines, stderr, cancel),
        }
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

    use p1_contracts::{ToolCall, ToolInput};
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
}
