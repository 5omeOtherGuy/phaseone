//! The runtime bridges: how the agent core reaches the screen and how the
//! screen answers back. `TuiSink` is the `EventSink` (observation only, never
//! blocks — events go into an unbounded channel stamped with millisecond
//! time). `TuiPolicy` is the `AuthorizationPolicy`: under full access (the
//! default, ADR-0038) it permits everything without a whisper; under `--ask`
//! it permits read-only calls and parks the rest on the screen until the
//! operator decides or the turn is cancelled.
//!
//! Both live here (not in the host) so the host's `tui.rs` driver is thin
//! wiring: it owns the terminal, the agent task, and the render tick.

use std::collections::HashSet;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Instant;

use p1_contracts::{
    AgentEvent, AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision,
    Effect, EventSink, ToolCall, ToolIdentity,
};
use tokio::sync::{mpsc, oneshot};

/// One agent event stamped with milliseconds since the frontend's epoch, so
/// the screen's fake-time discipline holds in production too: the clock is
/// injected at the seam, never read inside the renderers.
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub at_ms: u64,
    pub event: AgentEvent,
}

/// The `EventSink` for a TUI agent. Created before the agent; the receiving
/// end feeds the screen.
pub struct TuiSink {
    epoch: Instant,
    /// Compensates for clock granularity collisions so ordering survives.
    tick: AtomicU64,
    tx: mpsc::UnboundedSender<Stamped>,
}

impl TuiSink {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Stamped>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                epoch: Instant::now(),
                tick: AtomicU64::new(0),
                tx,
            },
            rx,
        )
    }

    /// Milliseconds since this sink's epoch — the ONE clock the screen runs on
    /// (events arrive stamped on it; the render tick reads it directly).
    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
}

impl EventSink for TuiSink {
    fn emit(&self, event: AgentEvent) {
        let at_ms = self.epoch.elapsed().as_millis() as u64;
        // Two events inside one clock tick keep their arrival order.
        let at_ms = at_ms.max(self.tick.fetch_add(1, Ordering::Relaxed));
        self.tick.store(at_ms + 1, Ordering::Relaxed);
        // Unbounded: observation must never block the agent loop (contract).
        let _ = self.tx.send(Stamped { at_ms, event });
    }
}

/// One parked authorization, delivered to the screen.
pub struct AuthRequest {
    pub call: ToolCall,
    pub identity: ToolIdentity,
    pub effect: Effect,
    reply: oneshot::Sender<Decision>,
}

impl std::fmt::Debug for AuthRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AuthRequest")
            .field("call", &self.call)
            .field("effect", &self.effect)
            .finish_non_exhaustive()
    }
}

impl AuthRequest {
    /// Answer the parked request. Dropping without an answer denies.
    pub fn answer(self, decision: Decision) {
        let _ = self.reply.send(decision);
    }
}

/// The TUI's authorization policy. Grant scopes: `session` is remembered in
/// memory; `project` is the same in-memory set for now — p1 has no trust store
/// yet (flagged on issue #12; when one lands, `project` persists).
pub struct TuiPolicy {
    ask: bool,
    cancel: CancellationToken,
    tx: mpsc::UnboundedSender<AuthRequest>,
    /// (tool name, identity) granted for the session.
    granted: Mutex<HashSet<(String, ToolIdentity)>>,
}

/// The exact refusal when the operator answers `n` (matches the host's line
/// policy so the model sees one vocabulary).
pub const USER_DENY: &str = "Denied by the user.";
/// The refusal when cancellation wins over a parked prompt.
pub const CANCEL_DENY: &str = "Cancelled while awaiting authorization.";

impl TuiPolicy {
    pub fn new(
        ask: bool,
        cancel: CancellationToken,
    ) -> (Self, mpsc::UnboundedReceiver<AuthRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                ask,
                cancel,
                tx,
                granted: Mutex::new(HashSet::new()),
            },
            rx,
        )
    }

    /// The operator's `a` answer: remember the grant, then permit this call.
    pub fn grant_session(&self, call: &ToolCall, identity: &ToolIdentity) {
        self.granted
            .lock()
            .unwrap()
            .insert((call.name.clone(), identity.clone()));
    }
}

impl AuthorizationPolicy for TuiPolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            if !self.ask {
                return Decision::Permit;
            }
            if request.effect == Effect::ReadOnly {
                return Decision::Permit;
            }
            let key = (request.call.name.clone(), request.identity.clone());
            if self.granted.lock().unwrap().contains(&key) {
                return Decision::Permit;
            }
            let (reply, answer) = oneshot::channel();
            let parked = AuthRequest {
                call: request.call.clone(),
                identity: request.identity.clone(),
                effect: request.effect,
                reply,
            };
            if self.tx.send(parked).is_err() {
                // The UI is gone: nothing may run unanswered.
                return Decision::Deny {
                    reason: CANCEL_DENY.to_string(),
                };
            }
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Decision::Deny { reason: CANCEL_DENY.to_string() },
                answer = answer => answer.unwrap_or(Decision::Deny { reason: USER_DENY.to_string() }),
            }
        })
    }
}

/// The terminal guard: raw mode + alternate screen, restored on drop. The
/// whole TUI lives inside one of these; a panic still hands back a sane
/// terminal.
pub struct TerminalGuard;

impl TerminalGuard {
    pub fn enter() -> std::io::Result<Self> {
        crossterm::terminal::enable_raw_mode()?;
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        Ok(Self)
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        let _ = crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
        let _ = crossterm::terminal::disable_raw_mode();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::ToolInput;

    fn request() -> (ToolCall, ToolIdentity) {
        (
            ToolCall {
                call_id: "c1".into(),
                name: "shell".into(),
                input: ToolInput::Json("{}".into()),
            },
            ToolIdentity {
                implementation: "shell".into(),
                variant: String::new(),
            },
        )
    }

    #[tokio::test]
    async fn full_access_never_parks() {
        let (policy, mut rx) = TuiPolicy::new(false, CancellationToken::new());
        let (call, identity) = request();
        let decision = policy
            .authorize(AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect: Effect::Executes,
            })
            .await;
        assert_eq!(decision, Decision::Permit);
        assert!(rx.try_recv().is_err(), "no request reached the screen");
    }

    #[tokio::test]
    async fn ask_parks_and_the_screen_answers() {
        let (policy, mut rx) = TuiPolicy::new(true, CancellationToken::new());
        let (call, identity) = request();
        let pending = tokio::spawn({
            let (call, identity) = (call.clone(), identity.clone());
            async move {
                policy
                    .authorize(AuthorizationRequest {
                        call: &call,
                        identity: &identity,
                        effect: Effect::WritesFiles,
                    })
                    .await
            }
        });
        let parked = rx.recv().await.expect("the request parked");
        parked.answer(Decision::Permit);
        assert_eq!(pending.await.unwrap(), Decision::Permit);
    }

    #[tokio::test]
    async fn a_dropped_answer_denies() {
        let (policy, mut rx) = TuiPolicy::new(true, CancellationToken::new());
        let (call, identity) = request();
        let pending = tokio::spawn(async move {
            policy
                .authorize(AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect: Effect::WritesFiles,
                })
                .await
        });
        let parked = rx.recv().await.unwrap();
        drop(parked);
        assert_eq!(
            pending.await.unwrap(),
            Decision::Deny {
                reason: USER_DENY.into()
            }
        );
    }

    #[tokio::test]
    async fn session_grants_skip_the_prompt() {
        let (policy, mut rx) = TuiPolicy::new(true, CancellationToken::new());
        let (call, identity) = request();
        policy.grant_session(&call, &identity);
        let decision = policy
            .authorize(AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect: Effect::WritesFiles,
            })
            .await;
        assert_eq!(decision, Decision::Permit);
        assert!(rx.try_recv().is_err());
    }
}
