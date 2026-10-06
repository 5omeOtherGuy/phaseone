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
    /// Claim order across parent and child sinks, independent of clock granularity.
    pub sequence: u64,
    pub worker: Option<String>,
    pub event: AgentEvent,
}

/// Everything the UI loop can receive from agents: events, and the marker that
/// a worker's agent was actually built (a failed start never reports).
#[derive(Debug)]
pub enum UiEvent {
    Agent(Stamped),
    Questions(QuestionRequest),
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
    /// Shared claim order; never advances the runtime clock on event bursts.
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

    pub fn questions(&self, request: QuestionRequest) -> bool {
        self.tx.send(UiEvent::Questions(request)).is_ok()
    }

    /// A worker's agent was built and started (the host calls this only after
    /// a successful build, so a failed start never shows).
    pub fn worker_started(&self, worker_id: &str) {
        let _ = self.tx.send(UiEvent::WorkerStarted(worker_id.to_string()));
    }

    /// A workflow event for the WORKERS tree (ADR-0075): stamped and ordered with the
    /// agents' events, through the same channel.
    pub fn workflow(&self, event: crate::workflow::WorkflowEvent) {
        let (at_ms, _) = self.stamp();
        let _ = self.tx.send(UiEvent::Workflow { at_ms, event });
    }

    /// Claim ordering separately from elapsed time: a burst consumes no time.
    fn stamp(&self) -> (u64, u64) {
        let sequence = self.tick.fetch_add(1, Ordering::Relaxed);
        (self.now_ms(), sequence)
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
        let (at_ms, sequence) = self.stamp();
        // Unbounded: observation must never block the agent loop (contract).
        let _ = self.tx.send(UiEvent::Agent(Stamped {
            at_ms,
            sequence,
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
    pub fn is_closed(&self) -> bool {
        self.reply.is_closed()
    }

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

    #[tokio::test(start_paused = true)]
    async fn event_bursts_stay_on_the_clock_and_peeks_expire_on_time() {
        use crate::state::{PEEK_MS, Promotion, Screen};
        use p1_contracts::{ToolResultItem, ToolStatus};
        use std::time::Duration;

        let (sink, mut rx) = TuiSink::new();
        let mut screen = Screen::new(false);
        sink.emit(AgentEvent::TurnStarted);
        for _ in 0..4_096 {
            sink.emit(AgentEvent::TextDelta { text: "x".into() });
        }
        sink.emit(AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id: "c1".into(),
                name: "shell".into(),
                status: ToolStatus::Error,
                content: "failed".into(),
            },
        });
        while let Ok(UiEvent::Agent(stamped)) = rx.try_recv() {
            assert_eq!(stamped.at_ms, sink.now_ms(), "bursts consume no clock time");
            screen.apply(&stamped.event, stamped.at_ms);
        }
        assert_eq!(screen.working.as_ref().unwrap().started_ms, sink.now_ms());
        assert!(matches!(screen.promotion, Promotion::Peek { .. }));
        tokio::time::advance(Duration::from_millis(PEEK_MS - 1)).await;
        screen.tick(sink.now_ms());
        assert!(matches!(screen.promotion, Promotion::Peek { .. }));
        tokio::time::advance(Duration::from_millis(1)).await;
        screen.tick(sink.now_ms());
        assert_eq!(screen.promotion, Promotion::None);
    }

    #[test]
    fn parent_and_child_sinks_share_order_and_clock_bound_timestamps() {
        let (parent, mut rx) = TuiSink::new();
        let child = parent.child("w1");
        // Interleave the two sinks; claim order must match emission order,
        // while timestamps stay on the runtime clock.
        for _ in 0..16 {
            parent.emit(AgentEvent::TurnStarted);
            child.emit(AgentEvent::TurnStarted);
        }
        let mut last: Option<(u64, u64)> = None;
        let mut seen = 0;
        while let Ok(UiEvent::Agent(stamped)) = rx.try_recv() {
            if let Some((previous_sequence, previous_ms)) = last {
                assert!(
                    stamped.sequence > previous_sequence,
                    "sequence {} did not increase past {previous_sequence}",
                    stamped.sequence
                );
                assert!(stamped.at_ms >= previous_ms, "clock never goes backwards");
            }
            assert!(stamped.at_ms <= parent.now_ms(), "stamp never exceeds now");
            last = Some((stamped.sequence, stamped.at_ms));
            seen += 1;
        }
        assert_eq!(seen, 32, "every emit reached the channel");
    }
}

pub type QuestionAnswers = Vec<(Vec<String>, Option<String>)>;

/// A host-owned question set; dropping the UI request never invents answers.
#[derive(Debug)]
pub struct QuestionRequest {
    pub view: crate::render::permission::QuestionView,
    pub reply: oneshot::Sender<Option<QuestionAnswers>>,
    pub cancel: CancellationToken,
}
