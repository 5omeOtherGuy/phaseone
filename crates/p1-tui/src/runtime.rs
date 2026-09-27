//! The runtime bridges: how the agent core reaches the screen and how the
//! screen answers back. `TuiSink` is the `EventSink` (observation only, never
//! blocks — events go into an unbounded channel stamped with millisecond
//! time). `TuiPolicy` is the screen's asker: the host's ask bridge decides
//! which calls ask (full access by default, ADR-0038; `--ask` opts in) and
//! remembers `always` grants; `TuiPolicy` only parks the question on the
//! screen until the operator answers or the turn is cancelled.
//!
//! Both live here (not in the host) so the host's `tui.rs` driver is thin
//! wiring: it owns the terminal, the agent task, and the render tick.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use p1_contracts::{
    AgentEvent, AuthorizationRequest, BoxFuture, CancellationToken, Effect, EventSink, ToolCall,
    ToolIdentity,
};
use tokio::sync::{mpsc, oneshot};
// The sink's epoch is the RUNTIME's clock, not `std`'s: outside a paused
// runtime the two are the same clock, but under `tokio::time::pause` every
// screen clock (event stamps and `now_ms`) then moves with simulated time, so a
// paused-time test drives the whole screen — the §5 PEEK countdown's expiry is
// compared against `now_ms` (issue #141).
use tokio::time::Instant;

/// One agent event stamped with milliseconds since the frontend's epoch, so
/// the screen's fake-time discipline holds in production too: the clock is
/// injected at the seam, never read inside the renderers. `worker` is `Some`
/// for a delegated child's events (the WORKERS pane keys on it).
#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub at_ms: u64,
    pub worker: Option<String>,
    pub event: AgentEvent,
}

/// Everything the UI loop can receive from agents: events, and the marker that
/// a worker's agent was actually built (a failed start never reports).
#[derive(Debug)]
pub enum UiEvent {
    Agent(Stamped),
    WorkerStarted(String),
    /// A workflow event for the WORKERS tree (ADR-0075), stamped on the same clock.
    Workflow {
        at_ms: u64,
        event: crate::workflow::WorkflowEvent,
    },
}

/// The `EventSink` for a TUI agent. Created before the agent; the receiving
/// end feeds the screen. Child sinks share the parent's channel, tagged.
pub struct TuiSink {
    epoch: Instant,
    /// Compensates for clock granularity collisions so ordering survives.
    /// Shared with every child sink: one clock, one claim order, so an
    /// interleaved parent/child stream keeps a single monotonic sequence.
    tick: Arc<AtomicU64>,
    worker: Option<String>,
    tx: mpsc::UnboundedSender<UiEvent>,
}

impl TuiSink {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<UiEvent>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                epoch: Instant::now(),
                tick: Arc::new(AtomicU64::new(0)),
                worker: None,
                tx,
            },
            rx,
        )
    }

    /// The sink for one worker's agent: same channel, tagged with its id.
    pub fn child(&self, worker_id: &str) -> Self {
        Self {
            epoch: self.epoch,
            // The parent's tick, not a fresh one: a child's events share the
            // parent's arrival-order sequence (the epoch already is shared).
            tick: self.tick.clone(),
            worker: Some(worker_id.to_string()),
            tx: self.tx.clone(),
        }
    }

    /// A worker's agent was built and started (the host calls this only after
    /// a successful build, so a failed start never shows).
    pub fn worker_started(&self, worker_id: &str) {
        let _ = self.tx.send(UiEvent::WorkerStarted(worker_id.to_string()));
    }

    /// A workflow event for the WORKERS tree (ADR-0075): stamped and ordered with the
    /// agents' events, through the same channel.
    pub fn workflow(&self, event: crate::workflow::WorkflowEvent) {
        let at_ms = self.stamp();
        let _ = self.tx.send(UiEvent::Workflow { at_ms, event });
    }

    /// Claim the next stamp: the clock now, strictly after every earlier claim.
    fn stamp(&self) -> u64 {
        let now = self.epoch.elapsed().as_millis() as u64;
        // Monotonic even for concurrent emits (a sink is Send+Sync): claim a
        // stamp strictly greater than every earlier claim. `fetch_update`
        // retries its compare-exchange until it publishes `max(now, prev+1)`,
        // so no two events — parent or child — ever share a stamp.
        match self
            .tick
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |prev| {
                Some(now.max(prev + 1))
            }) {
            Ok(previous) => now.max(previous + 1),
            // Unreachable: the closure above always returns `Some`.
            Err(_) => now,
        }
    }

    /// Milliseconds since this sink's epoch — the ONE clock the screen runs on
    /// (events arrive stamped on it; the render tick reads it directly). The
    /// epoch is the runtime's `Instant`, so this is real time in production and
    /// simulated time under a paused test runtime.
    pub fn now_ms(&self) -> u64 {
        self.epoch.elapsed().as_millis() as u64
    }
}

impl EventSink for TuiSink {
    fn emit(&self, event: AgentEvent) {
        let at_ms = self.stamp();
        // Unbounded: observation must never block the agent loop (contract).
        let _ = self.tx.send(UiEvent::Agent(Stamped {
            at_ms,
            worker: self.worker.clone(),
            event,
        }));
    }
}

/// One parked authorization, delivered to the screen.
pub struct AuthRequest {
    pub call: ToolCall,
    pub identity: ToolIdentity,
    pub effect: Effect,
    reply: oneshot::Sender<Answer>,
}

/// The operator's answer to a parked request (the host's bridge turns it into a
/// decision and keeps the `Always` grants).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    /// Permit this call.
    Yes,
    /// Deny this call.
    No,
    /// Permit this call and remember the grant.
    Always,
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
    /// Answer the parked request. Dropping without an answer is `No`.
    pub fn answer(self, answer: Answer) {
        let _ = self.reply.send(answer);
    }
}

/// The TUI's asker: parks each question on the screen. The rules (which calls
/// ask, which grants are remembered) live in the host's ask bridge.
pub struct TuiPolicy {
    cancel: CancellationToken,
    /// The live turn's token, swapped by the driver at turn start/end: an
    /// approval parked when its turn is cancelled resolves CANCEL_DENY instead
    /// of waiting for a decision about a dead turn.
    turn: Mutex<Option<CancellationToken>>,
    tx: mpsc::UnboundedSender<AuthRequest>,
}

/// The exact refusal when the operator answers `n` (matches the host's line
/// policy so the model sees one vocabulary).
pub const USER_DENY: &str = "Denied by the user.";
/// The refusal when cancellation wins over a parked prompt.
pub const CANCEL_DENY: &str = "Cancelled while awaiting authorization.";

impl TuiPolicy {
    pub fn new(cancel: CancellationToken) -> (Self, mpsc::UnboundedReceiver<AuthRequest>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                cancel,
                turn: Mutex::new(None),
                tx,
            },
            rx,
        )
    }

    /// The driver marks the live turn's token (None between turns).
    pub fn set_turn(&self, token: Option<CancellationToken>) {
        *self.turn.lock().unwrap() = token;
    }

    /// Park `request` on the screen and wait for the operator. `None` when the
    /// turn is cancelled first or the UI is gone (CANCEL_DENY): nothing may run
    /// unanswered. A dropped request is `No`.
    pub fn ask<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Option<Answer>> {
        Box::pin(async move {
            let (reply, answer) = oneshot::channel();
            let parked = AuthRequest {
                call: request.call.clone(),
                identity: request.identity.clone(),
                effect: request.effect,
                reply,
            };
            if self.tx.send(parked).is_err() {
                // The UI is gone: nothing may run unanswered.
                return None;
            }
            let turn = self.turn.lock().unwrap().clone();
            let turn_cancelled = async move {
                match turn {
                    Some(turn) => turn.cancelled().await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => None,
                _ = turn_cancelled => None,
                answer = answer => Some(answer.unwrap_or(Answer::No)),
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
        // Alt screen first: if raw mode then fails, the terminal is still
        // restored (raw-first would strand the terminal on alt-screen failure).
        crossterm::execute!(std::io::stdout(), crossterm::terminal::EnterAlternateScreen)?;
        if let Err(error) = crossterm::terminal::enable_raw_mode() {
            let _ =
                crossterm::execute!(std::io::stdout(), crossterm::terminal::LeaveAlternateScreen);
            return Err(error);
        }
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

    // The rules moved to the host's ask bridge (issue #308): full access never
    // parking and `always` grants skipping the prompt are the bridge's cases now
    // (`p1-host`'s `policy` and `tui` tests). `TuiPolicy` only asks.

    #[tokio::test]
    async fn a_gone_screen_is_no_answer() {
        let (policy, rx) = TuiPolicy::new(CancellationToken::new());
        drop(rx);
        let (call, identity) = request();
        let answer = policy
            .ask(AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect: Effect::Executes,
            })
            .await;
        assert_eq!(answer, None, "nothing may run unanswered");
    }

    #[tokio::test]
    async fn ask_parks_and_the_screen_answers() {
        for reply in [Answer::Yes, Answer::No, Answer::Always] {
            let (policy, mut rx) = TuiPolicy::new(CancellationToken::new());
            let (call, identity) = request();
            let pending = tokio::spawn({
                let (call, identity) = (call.clone(), identity.clone());
                async move {
                    policy
                        .ask(AuthorizationRequest {
                            call: &call,
                            identity: &identity,
                            effect: Effect::WritesFiles,
                        })
                        .await
                }
            });
            let parked = rx.recv().await.expect("the request parked");
            parked.answer(reply);
            assert_eq!(pending.await.unwrap(), Some(reply));
        }
    }

    #[tokio::test]
    async fn a_dropped_answer_denies() {
        let (policy, mut rx) = TuiPolicy::new(CancellationToken::new());
        let (call, identity) = request();
        let pending = tokio::spawn(async move {
            policy
                .ask(AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect: Effect::WritesFiles,
                })
                .await
        });
        let parked = rx.recv().await.unwrap();
        drop(parked);
        assert_eq!(pending.await.unwrap(), Some(Answer::No));
    }

    #[tokio::test]
    async fn a_cancelled_turn_resolves_the_parked_ask() {
        let (policy, mut rx) = TuiPolicy::new(CancellationToken::new());
        let turn = CancellationToken::new();
        policy.set_turn(Some(turn.clone()));
        let (call, identity) = request();
        let pending = tokio::spawn(async move {
            policy
                .ask(AuthorizationRequest {
                    call: &call,
                    identity: &identity,
                    effect: Effect::Executes,
                })
                .await
        });
        let _parked = rx.recv().await.expect("the request parked");
        turn.cancel();
        assert_eq!(pending.await.unwrap(), None);
    }

    #[test]
    fn parent_and_child_sinks_share_one_strictly_increasing_clock() {
        let (parent, mut rx) = TuiSink::new();
        let child = parent.child("w1");
        // Interleave the two sinks; one shared claim clock must keep the
        // combined emission order and the stamp order identical.
        for _ in 0..16 {
            parent.emit(AgentEvent::TurnStarted);
            child.emit(AgentEvent::TurnStarted);
        }
        let mut last: Option<u64> = None;
        let mut seen = 0;
        while let Ok(UiEvent::Agent(stamped)) = rx.try_recv() {
            if let Some(previous) = last {
                assert!(
                    stamped.at_ms > previous,
                    "stamp {} did not increase past {previous}",
                    stamped.at_ms
                );
            }
            last = Some(stamped.at_ms);
            seen += 1;
        }
        assert_eq!(seen, 32, "every emit reached the channel");
    }
}
