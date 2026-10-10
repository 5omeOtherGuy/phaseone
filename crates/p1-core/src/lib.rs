//! The p1 agent core: one agent's request → stream → tool calls → repeat loop.
//!
//! The core knows only `p1-contracts`. It never names a provider, a tool, a file
//! format, a prompt template or a UI. Behaviour is specified in
//! `docs/design/core.md`; that note is authoritative.

use std::collections::{HashSet, VecDeque};
use std::num::NonZeroUsize;
use std::ops::Range;
use std::sync::{Arc, Mutex, Weak};

use futures_util::StreamExt;
use p1_contracts::{
    AgentEvent, AuthorizationPolicy, AuthorizationRequest, CancellationToken, Clock, CommitError,
    CommitSink, Compaction, CompletedResponse, Concurrency, ContextError, ContextInput,
    ContextPolicy, Decision, Effect, EventSink, InboxKind, InterruptionReason, Item, JournalRecord,
    ModelOptions, Outcome, Prepared, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    ProviderStream, RecordBody, StopReason, StreamEvent, SystemClock, Tool, ToolCall, ToolContext,
    ToolOutcome, ToolResultItem, ToolStatus, TurnEnd, Usage, Wait,
};
use tokio::sync::Notify;

mod resume;
pub use resume::{Projection, ResumeError, ResumeReport, UnresolvedCall, project};

/// R5: exact model-visible content for a call whose `ToolStarted` was committed but
/// whose outcome was never recorded.
const UNKNOWN_OUTCOME: &str = "Interrupted: this call was started before the session stopped and its outcome is unknown. Check the current state before retrying.";

/// §4 row 1: the result of a call that was never executed because the turn was cancelled.
const CANCELLED_BEFORE_EXECUTION: &str = "Cancelled before execution.";

/// ADR-0118 Decision 7: at most this many calls of one response execute at once unless the
/// host sets another bound ([`Agent::set_max_parallel_tools`]); Claude Code's default.
pub const DEFAULT_MAX_PARALLEL_TOOLS: NonZeroUsize = NonZeroUsize::new(10).unwrap();

/// How many times one request is re-sent after a provider-confirmed context overflow
/// unless the host sets another bound ([`Agent::set_max_overflow_retries`]).
pub const DEFAULT_MAX_OVERFLOW_RETRIES: u32 = 1;

/// Everything one agent is assembled from. The agent owns exactly these tools:
/// a tool that is not in `tools` does not exist for it.
pub struct AgentParts {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub options: ModelOptions,
    pub context: Arc<dyn ContextPolicy>,
    pub authorization: Arc<dyn AuthorizationPolicy>,
    pub journal: Arc<dyn CommitSink>,
    pub events: Arc<dyn EventSink>,
}

/// The parts of [`AgentParts`] a running agent can be switched to between turns
/// (ADR-0049, ADR-0084). Journal and events belong to the session, not to the
/// assembly, so a switch never replaces them.
pub struct Reconfiguration {
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub options: ModelOptions,
    pub context: Arc<dyn ContextPolicy>,
    /// `None` keeps the policy the agent holds. An `Option` rather than a required
    /// `Arc`: a model switch does not own the session's policy and the core hands
    /// none of its parts out, so "keep" must be expressible without holding it.
    pub authorization: Option<Arc<dyn AuthorizationPolicy>>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("two assembled tools share the call name `{0}`")]
    DuplicateToolName(String),
    #[error("the provider rejected this environment: {0}")]
    ProviderRejected(ProviderError),
}

/// Why [`Agent::reconfigure`] left the current assembly in place.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum ReconfigureError {
    /// The candidate failed validation against the current history; nothing was
    /// written.
    #[error(transparent)]
    Rejected(BuildError),
    /// The candidate's `Environment` record could not be committed; nothing was
    /// installed and no sequence number was used.
    #[error("the new environment could not be committed: {0}")]
    CommitFailed(String),
}

/// State shared between the `Agent` and every `Inbox` handle.
struct InboxShared {
    queue: Mutex<VecDeque<(InboxKind, String)>>,
    notify: Notify,
}

/// Clonable, `Send` handle for delivering messages to a (possibly running) agent.
/// Messages are handed to the model at the next safe boundary.
#[derive(Clone)]
pub struct Inbox {
    /// `Weak` so `send` can detect that the owning `Agent` has been dropped.
    shared: Weak<InboxShared>,
}

impl Inbox {
    /// Queue a message. Never blocks. Returns `false` if the agent no longer exists.
    pub fn send(&self, kind: InboxKind, text: impl Into<String>) -> bool {
        let Some(shared) = self.shared.upgrade() else {
            return false;
        };
        shared.queue.lock().unwrap().push_back((kind, text.into()));
        shared.notify.notify_one();
        true
    }
}

/// One agent. Single owner of its state: `run_turn` takes `&mut self`.
pub struct Agent {
    parts: AgentParts,
    history: Vec<Item>,
    /// The sequence number of the next record to commit. Advances only on a
    /// successful commit, so it is dense across turns (spec §2, R2, invariant 5c).
    next_seq: u64,
    /// False until the first `Environment` record has been committed successfully.
    environment_committed: bool,
    /// R5: call ids whose `ToolStarted` was committed but whose `ToolFinished` has
    /// not been. Used to tell a never-started call (`Cancelled`) from one that ran
    /// with an unrecorded outcome (`Unknown`) when a later turn reconciles them.
    started_calls: HashSet<String>,
    /// Usage of the most recent COMMITTED response, `None` when it reported none.
    /// Handed to the context policy and restored by `resume` (§3b, context.md §1).
    last_usage: Option<Usage>,
    /// Wall-clock time source (ADR-0121). Stamps `RequestTiming` for each request;
    /// the system clock by default, [`Agent::set_clock`] for a fake one in tests.
    clock: Arc<dyn Clock>,
    /// ADR-0118: how many calls of one group may execute at once. 1 runs every call
    /// alone, in block order, exactly as before parallel execution.
    max_parallel_tools: NonZeroUsize,
    /// Overflow recovery: re-sends allowed per request after a compaction. 0 is off.
    max_overflow_retries: u32,
    inbox: Arc<InboxShared>,
}

/// One call of a group after step (a) of §4 (ADR-0118 Decision 5).
enum Admitted {
    /// Recorded without executing: cancelled, unavailable or denied.
    Settled(ToolStatus, String),
    /// `{ToolStarted}` is committed; the call executes in step (b).
    Started(Arc<dyn Tool>),
}

/// What one executed call returned, with the answers read right after its own `execute`.
struct Executed {
    outcome: ToolOutcome,
    /// ADR-0120, asked of the tool for THIS call's outcome before any other call can run
    /// (#592): concurrent calls of one tool never mix their answers.
    ends_turn: bool,
    exit_code: Option<i32>,
}

/// What one request-loop iteration wants next.
enum Flow {
    Continue,
    End(TurnEnd),
    /// The provider rejected the request as too long; the failure is already journalled.
    /// The loop may compact and re-send, else this is the turn's end.
    Overflow(TurnEnd),
}

impl Flow {
    /// A request's provider failure: an overflow is recoverable, anything else ends.
    fn from_failure(end: TurnEnd) -> Self {
        match &end {
            TurnEnd::ProviderFailed { error }
                if error.kind == ProviderErrorKind::ContextWindowExceeded =>
            {
                Flow::Overflow(end)
            }
            _ => Flow::End(end),
        }
    }
}

/// Terminal shape of a consumed provider stream.
enum StreamOutcome {
    Completed(CompletedResponse),
    Cancelled(String),
    Failed(String, ProviderError),
}

/// Result of one raced `stream.next()`.
enum StreamStep {
    Event(StreamEvent),
    Ended,
    Cancelled,
}

/// Wall-clock timing of ONE request (ADR-0121), collected as its stream runs.
/// Turned into a `RecordBody::RequestTiming` and committed right after the request's
/// `AssistantCompleted`/`AssistantInterrupted`.
struct Timing {
    request_index: u32,
    sent_ms: u64,
    first_event_ms: Option<u64>,
    first_output_ms: Option<u64>,
    ended_ms: Option<u64>,
    waits: Vec<Wait>,
}

impl Timing {
    fn new(request_index: u32, sent_ms: u64) -> Self {
        Self {
            request_index,
            sent_ms,
            first_event_ms: None,
            first_output_ms: None,
            ended_ms: None,
            waits: Vec::new(),
        }
    }

    /// Account for one stream event. A `Wait` is collected into `waits` and is NOT
    /// the "first event": the example in ADR-0121's brief places a `Wait` between
    /// send and the first event, so a wait never stands in for a first byte. A wait
    /// also forgets an earlier first event: what came before it belonged to the
    /// attempt that failed (the adapter's back-off `Activity`), not to the answer.
    /// A `Notice` is the adapter's own text, never a byte from the provider.
    fn record_event(&mut self, clock: &dyn Clock, event: &StreamEvent) {
        if let StreamEvent::Wait {
            reason,
            attempt,
            delay_ms,
        } = event
        {
            self.waits.push(Wait {
                reason: *reason,
                attempt: *attempt,
                delay_ms: *delay_ms,
            });
            self.first_event_ms = None;
            return;
        }
        if matches!(event, StreamEvent::Notice { .. }) {
            return;
        }
        let first_event = self.first_event_ms.is_none();
        let first_output = self.first_output_ms.is_none()
            && matches!(
                event,
                StreamEvent::TextDelta { .. }
                    | StreamEvent::ReasoningDelta { .. }
                    | StreamEvent::ToolInputDelta { .. }
            );
        let terminal = matches!(event, StreamEvent::Finished(_)) && self.ended_ms.is_none();
        // Read the clock at most once per event, so the fields a single event fills
        // agree and a scripted clock sees one tick per event.
        if !(first_event || first_output || terminal) {
            return;
        }
        let now = clock.now_ms();
        if first_event {
            self.first_event_ms = Some(now);
        }
        if first_output {
            self.first_output_ms = Some(now);
        }
        if terminal {
            self.ended_ms = Some(now);
        }
    }

    /// Stamp the end of the request, unless a terminal event already did.
    fn finish(&mut self, clock: &dyn Clock) {
        if self.ended_ms.is_none() {
            self.ended_ms = Some(clock.now_ms());
        }
    }

    /// The record this request commits. `ended_ms` falls back to `sent_ms` only when
    /// no event ever ended the stream, so the field is never absent.
    fn into_body(self) -> RecordBody {
        let ended_ms = self.ended_ms.unwrap_or(self.sent_ms);
        RecordBody::RequestTiming {
            request_index: self.request_index,
            sent_ms: self.sent_ms,
            first_event_ms: self.first_event_ms,
            first_output_ms: self.first_output_ms,
            ended_ms,
            waits: self.waits,
        }
    }
}

impl Agent {
    /// Fails before anything runs if the environment is incoherent.
    pub fn new(parts: AgentParts) -> Result<Self, BuildError> {
        Self::assemble(parts, Vec::new(), 0, false, HashSet::new(), None)
    }

    /// Switch this agent to another assembly between turns (ADR-0049, ADR-0084,
    /// model-selection.md §3). Callable only between turns (`&mut self`), never
    /// inside a tool loop: a boundary has no thinking blocks pending.
    ///
    /// Runs exactly [`Agent::new`]'s checks, but `provider.validate` gets the
    /// CURRENT history, so a route that cannot carry this transcript is refused
    /// before anything is sent or committed. The candidate's `Environment` record
    /// is then committed, and only once that commit returned are the parts
    /// installed, with no await in between: a caller that drops this future, or a
    /// journal that fails, leaves the old assembly answering. A dropped future
    /// leaves the commit's outcome unknown, though — a store whose commit is a
    /// blocking write its runtime does not cancel may still have written the record
    /// and advanced its own sequence — so such a caller must resume, not retry (ADR
    /// draft, item 2.3). Journal and events are never replaced. A candidate equal to
    /// the current environment still commits its record: the call is an explicit
    /// change and the journal says so.
    pub async fn reconfigure(&mut self, next: Reconfiguration) -> Result<(), ReconfigureError> {
        let Reconfiguration {
            provider,
            tools,
            system_prompt,
            options,
            context,
            authorization,
        } = next;
        check_environment(&provider, &tools, &system_prompt, &options, &self.history)
            .map_err(ReconfigureError::Rejected)?;
        let body = environment_record(&provider, &tools, &system_prompt, &options);
        self.commit(body)
            .await
            .map_err(|error| ReconfigureError::CommitFailed(error.0))?;
        // Nothing below awaits: once the record is durable the candidate is the
        // agent's, whole, before any other code can observe the agent.
        self.parts.provider = provider;
        self.parts.tools = tools;
        self.parts.system_prompt = system_prompt;
        self.parts.options = options;
        self.parts.context = context;
        if let Some(authorization) = authorization {
            self.parts.authorization = authorization;
        }
        self.environment_committed = true;
        Ok(())
    }

    /// Validate the parts (exactly as [`Agent::new`] does) and install `history`,
    /// `next_seq`, `environment_committed`, `started_calls` and `last_usage`.
    /// Shared with `resume`, so construction and its `BuildError`s exist once. The
    /// parts are validated against `history` — the transcript this agent will send
    /// — so a resumed session is checked against what it projects (ADR-0049).
    pub(crate) fn assemble(
        parts: AgentParts,
        history: Vec<Item>,
        next_seq: u64,
        environment_committed: bool,
        started_calls: HashSet<String>,
        last_usage: Option<Usage>,
    ) -> Result<Self, BuildError> {
        check_environment(
            &parts.provider,
            &parts.tools,
            &parts.system_prompt,
            &parts.options,
            &history,
        )?;
        Ok(Self {
            parts,
            history,
            next_seq,
            environment_committed,
            started_calls,
            last_usage,
            clock: Arc::new(SystemClock),
            max_parallel_tools: DEFAULT_MAX_PARALLEL_TOOLS,
            max_overflow_retries: DEFAULT_MAX_OVERFLOW_RETRIES,
            inbox: Arc::new(InboxShared {
                queue: Mutex::new(VecDeque::new()),
                notify: Notify::new(),
            }),
        })
    }

    /// Replace the agent's wall-clock source (ADR-0121). The system clock is the
    /// default; a test installs a fake one so `RequestTiming` is deterministic.
    pub fn set_clock(&mut self, clock: Arc<dyn Clock>) {
        self.clock = clock;
    }

    /// Bound how many calls of one response execute at once (ADR-0118; the environment's
    /// `[tool_concurrency] max_parallel`). 1 runs every call alone, in block order, with the
    /// records and authorization order of sequential execution. A host sets it again after
    /// a switch to another environment; [`Agent::reconfigure`] keeps it.
    pub fn set_max_parallel_tools(&mut self, limit: NonZeroUsize) {
        self.max_parallel_tools = limit;
    }

    /// Bound the overflow retries of one request (the environment's
    /// `[context] max_overflow_retries`). 0 turns overflow recovery off. A host sets it
    /// again after a switch to another environment; [`Agent::reconfigure`] keeps it.
    pub fn set_max_overflow_retries(&mut self, limit: u32) {
        self.max_overflow_retries = limit;
    }

    pub fn inbox(&self) -> Inbox {
        Inbox {
            shared: Arc::downgrade(&self.inbox),
        }
    }

    /// True if inbox messages are waiting to be delivered.
    pub fn has_pending_inbox(&self) -> bool {
        !self.inbox.queue.lock().unwrap().is_empty()
    }

    /// Resolves once at least one inbox message is pending (immediately if one is).
    /// Lets a host sleep until a notification arrives instead of polling.
    pub async fn inbox_ready(&self) {
        // Create `notified()` BEFORE the emptiness check: a `send` between the
        // check and the wait then leaves a permit the wait consumes (invariant 6).
        loop {
            let notified = self.inbox.notify.notified();
            let pending = !self.inbox.queue.lock().unwrap().is_empty();
            if pending {
                return;
            }
            notified.await;
        }
    }

    /// Run one turn started by user input.
    pub async fn run_turn(&mut self, input: String, cancel: CancellationToken) -> TurnEnd {
        let end = self.run_inner(Some(input), &cancel).await;
        // §3.4: `TurnFinished` is the last event of every turn.
        self.parts
            .events
            .emit(AgentEvent::TurnFinished { end: end.clone() });
        end
    }

    /// Run one turn started by pending inbox messages only (no user input).
    /// Returns `None` without doing anything if the inbox is empty.
    pub async fn run_inbox_turn(&mut self, cancel: CancellationToken) -> Option<TurnEnd> {
        // §5: an empty inbox-only turn is an exact no-op (no event, no record).
        if !self.has_pending_inbox() {
            return None;
        }
        let end = self.run_inner(None, &cancel).await;
        self.parts
            .events
            .emit(AgentEvent::TurnFinished { end: end.clone() });
        Some(end)
    }

    /// Resume the retained transcript after a host-selected provider change, without
    /// appending user input or replaying already committed tool calls.
    pub async fn resume_turn(&mut self, cancel: CancellationToken) -> TurnEnd {
        let end = self.run_inner(None, &cancel).await;
        self.parts
            .events
            .emit(AgentEvent::TurnFinished { end: end.clone() });
        end
    }

    /// The current model-visible history (the journal's projection).
    pub fn history(&self) -> &[Item] {
        &self.history
    }

    // ---------------------------------------------------------------- turn

    /// §3 steps 1–2 then the request loop. `input: None` is an inbox-only turn.
    async fn run_inner(&mut self, input: Option<String>, cancel: &CancellationToken) -> TurnEnd {
        self.parts.events.emit(AgentEvent::TurnStarted);
        // §2 / R2: the first turn commits `Environment` at seq 0 before its input.
        if let Err(message) = self.commit_environment_if_needed().await {
            return TurnEnd::CommitFailed { message };
        }
        // R5: resolve calls left without a result by an earlier failed commit,
        // before this turn's own records, so every request history is well-formed.
        if let Err(end) = self.reconcile_unresolved_calls().await {
            return end;
        }
        if let Some(text) = input {
            let body = RecordBody::UserInput { text: text.clone() };
            // Invariant 5b: the record is committed before its history side effect.
            if let Err(error) = self.commit(body).await {
                return TurnEnd::CommitFailed { message: error.0 };
            }
            self.history.push(Item::User { text });
        }
        self.request_loop(cancel).await
    }

    async fn commit_environment_if_needed(&mut self) -> Result<(), String> {
        if self.environment_committed {
            return Ok(());
        }
        let body = environment_record(
            &self.parts.provider,
            &self.parts.tools,
            &self.parts.system_prompt,
            &self.parts.options,
        );
        self.commit(body).await.map_err(|error| error.0)?;
        self.environment_committed = true;
        Ok(())
    }

    async fn request_loop(&mut self, cancel: &CancellationToken) -> TurnEnd {
        let mut request_index: u32 = 0;
        let mut overflow_retries: u32 = 0;
        loop {
            match self.one_request(request_index, cancel).await {
                Flow::Continue => {
                    request_index = request_index.wrapping_add(1);
                    overflow_retries = 0;
                }
                // The request was rejected as too long and the history has since shrunk:
                // send it again, at most `max_overflow_retries` times in a row.
                Flow::Overflow(end) => {
                    if overflow_retries >= self.max_overflow_retries
                        || !self.compact_after_overflow(cancel).await
                    {
                        return end;
                    }
                    overflow_retries += 1;
                    request_index = request_index.wrapping_add(1);
                }
                Flow::End(end) => return end,
            }
        }
    }

    /// Overflow recovery: one compaction through the context policy, the same engine and
    /// the same `ContextReplaced` record as the threshold and manual paths. True only when
    /// it really shrank the history; `Unchanged`, an error or a cancel is false and the
    /// caller ends the turn with the original provider error.
    async fn compact_after_overflow(&mut self, cancel: &CancellationToken) -> bool {
        matches!(
            self.compact_now(cancel).await,
            Ok(Compaction::Replaced { .. })
        )
    }

    /// §3 steps 3a–3f/3g for one request index.
    async fn one_request(&mut self, request_index: u32, cancel: &CancellationToken) -> Flow {
        // 3a: deliver every pending inbox message, in arrival order.
        if let Err(message) = self.deliver_inbox().await {
            return Flow::End(TurnEnd::CommitFailed { message });
        }
        // 3b: context preparation. `Ok(Some)` replaces the history from now on.
        // R1: cancellation is checked FIRST and wins even when `prepare` is ready at
        // the same moment, so a turned cancelled mid-preparation stops waiting here.
        let context = self.parts.context.clone();
        let prepared = {
            let input = ContextInput {
                history: &self.history,
                last_usage: self.last_usage.as_ref(),
                cancel,
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                result = context.prepare(input) => Some(result),
            }
        };
        let items_before = self.history.len();
        match prepared {
            // Cancel fired before `prepare` returned: same records as a cancel
            // before the request, and nothing abandoned is committed.
            None => {
                let end = self
                    .end_interrupted(InterruptionReason::Cancelled, String::new(), None, None)
                    .await;
                return Flow::End(end);
            }
            Some(Ok(None)) => {}
            Some(Ok(Some(prepared))) => {
                if let Err(end) = self.install_replacement(prepared, items_before).await {
                    return Flow::End(end);
                }
            }
            // The policy itself gave up, with or without the turn's token firing.
            Some(Err(ContextError::Cancelled)) => {
                let end = self
                    .end_interrupted(InterruptionReason::Cancelled, String::new(), None, None)
                    .await;
                return Flow::End(end);
            }
            Some(Err(ContextError::Failed(message))) => {
                return Flow::End(TurnEnd::ContextFailed { message });
            }
        }
        // 3c: request. R1: check `cancel` before waiting on the provider at all.
        self.parts
            .events
            .emit(AgentEvent::RequestStarted { request_index });
        if cancel.is_cancelled() {
            let end = self
                .end_interrupted(InterruptionReason::Cancelled, String::new(), None, None)
                .await;
            return Flow::End(end);
        }
        let provider = self.parts.provider.clone();
        let request = self.build_request();
        let cancel_child = cancel.child_token();
        // ADR-0121: `sent_ms` is the clock just before the request is handed to the
        // provider; the timing of this request is collected from here on.
        let sent_ms = self.clock.now_ms();
        let mut timing = Timing::new(request_index, sent_ms);
        // Invariant 5a: the `stream()` setup future itself races `cancel` (R1).
        let stream_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = provider.stream(request, cancel_child) => Some(result),
        };
        let stream = match stream_result {
            None => {
                timing.finish(self.clock.as_ref());
                let end = self
                    .end_interrupted(
                        InterruptionReason::Cancelled,
                        String::new(),
                        None,
                        Some(timing),
                    )
                    .await;
                return Flow::End(end);
            }
            Some(Err(error)) => {
                timing.finish(self.clock.as_ref());
                let end = self
                    .end_interrupted(
                        InterruptionReason::ProviderFailed,
                        String::new(),
                        Some(error),
                        Some(timing),
                    )
                    .await;
                return Flow::from_failure(end);
            }
            Some(Ok(stream)) => stream,
        };
        // 3d: consume the stream to its terminal event.
        let outcome = self.consume_stream(stream, cancel, &mut timing).await;
        let response = match outcome {
            StreamOutcome::Completed(response) => response,
            StreamOutcome::Cancelled(partial) => {
                let end = self
                    .end_interrupted(InterruptionReason::Cancelled, partial, None, Some(timing))
                    .await;
                return Flow::End(end);
            }
            StreamOutcome::Failed(partial, error) => {
                let end = self
                    .end_interrupted(
                        InterruptionReason::ProviderFailed,
                        partial,
                        Some(error),
                        Some(timing),
                    )
                    .await;
                return Flow::from_failure(end);
            }
        };
        // 3e: commit the completed response before anything can observe it.
        let CompletedResponse { item, stop, usage } = response;
        let calls: Vec<ToolCall> = item.tool_calls().cloned().collect();
        let model = item.origin.model.clone();
        let body = RecordBody::AssistantCompleted {
            item: item.clone(),
            stop,
            usage,
        };
        if let Err(error) = self.commit(body).await {
            return Flow::End(TurnEnd::CommitFailed { message: error.0 });
        }
        // ADR-0121: the request's timing follows its `AssistantCompleted`.
        if let Err(error) = self.commit_request_timing(timing.into_body()).await {
            return Flow::End(TurnEnd::CommitFailed { message: error.0 });
        }
        self.history.push(Item::Assistant(item));
        // §3b: remember this response's usage for the next preparation, resetting to
        // `None` when the response reported none.
        self.last_usage = usage;
        // Invariant 5g: `usage: None` is forwarded verbatim, never turned into zeros.
        self.parts
            .events
            .emit(AgentEvent::ResponseCompleted { model, stop, usage });
        // 3f: no tool calls in the item.
        if calls.is_empty() {
            if stop == StopReason::Paused {
                return Flow::Continue;
            }
            if self.has_pending_inbox() {
                return Flow::Continue;
            }
            return Flow::End(TurnEnd::Completed { stop });
        }
        // 3g: tool calls, in groups (ADR-0118 Decision 4): a run of consecutive Shared
        // calls is one group, every other call a group of its own; groups run in block order.
        let tools: Vec<Option<Arc<dyn Tool>>> =
            calls.iter().map(|call| self.assembled_tool(call)).collect();
        let mut ends_turn = false;
        for group in groups(&calls, &tools, self.max_parallel_tools) {
            // Invariant 5d: each call gets its result here, before the next request.
            // If a commit inside fails, R5 reconciles the leftovers next turn.
            match self
                .run_group(&calls[group.clone()], &tools[group], cancel)
                .await
            {
                Ok(group_ends_turn) => ends_turn |= group_ends_turn,
                Err(end) => return Flow::End(end),
            }
        }
        if cancel.is_cancelled() {
            return Flow::End(TurnEnd::Cancelled);
        }
        // ADR-0120 point 3: every call above ran and recorded its result; now a call whose
        // outcome ended the turn (an accepted `finish`) ends it here, with the response's
        // own stop reason, even while inbox messages are pending. The core names no tool.
        if ends_turn {
            return Flow::End(TurnEnd::Completed { stop });
        }
        Flow::Continue
    }

    /// §3b: install a policy's replacement — validated, journalled as
    /// `ContextReplaced`, then the history, then the event (R6). The ONE install,
    /// shared by the threshold path and [`Agent::compact_now`] (ADR-0076), so both
    /// write the same record.
    async fn install_replacement(
        &mut self,
        prepared: Prepared,
        items_before: usize,
    ) -> Result<(), TurnEnd> {
        let Prepared { items, usage } = prepared;
        // A policy bug must not become a provider 400 three requests later.
        if let Err(message) = validate_replacement(&items) {
            return Err(TurnEnd::ContextFailed { message });
        }
        let items_after = items.len();
        let body = RecordBody::ContextReplaced {
            items: items.clone(),
            usage,
        };
        if let Err(error) = self.commit(body).await {
            return Err(TurnEnd::CommitFailed { message: error.0 });
        }
        self.history = items;
        // R6: the event announces the committed record, so it comes after.
        self.parts.events.emit(AgentEvent::ContextReplaced {
            items_before,
            items_after,
            usage,
        });
        Ok(())
    }

    /// Manual compaction (ADR-0076): ask the context policy for one summary of the
    /// current history NOW ([`ContextPolicy::compact_now`]) and install it exactly
    /// as a threshold replacement is installed — the same `ContextReplaced` record
    /// and event. Callable only between turns (`&mut self`), like
    /// [`Agent::reconfigure`]. A pending `Environment` is committed first, as a
    /// turn would before its own records.
    ///
    /// On a replacement the last usage is forgotten: it measured the history that
    /// was just replaced, so the next preparation estimates the new one instead of
    /// adding to a number that no longer applies. `Unchanged` changes nothing. On
    /// an error nothing is installed; the error names why.
    pub async fn compact_now(
        &mut self,
        cancel: &CancellationToken,
    ) -> Result<Compaction, ContextError> {
        let context = self.parts.context.clone();
        let input = ContextInput {
            history: &self.history,
            last_usage: self.last_usage.as_ref(),
            cancel,
        };
        let compaction = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Err(ContextError::Cancelled),
            result = context.compact_now(input) => result?,
        };
        let Compaction::Replaced {
            prepared,
            tokens_before,
            tokens_after,
        } = compaction
        else {
            return Ok(compaction);
        };
        self.commit_environment_if_needed()
            .await
            .map_err(ContextError::Failed)?;
        let kept = Prepared {
            items: prepared.items.clone(),
            usage: prepared.usage,
        };
        let items_before = self.history.len();
        self.install_replacement(prepared, items_before)
            .await
            .map_err(|end| {
                ContextError::Failed(match end {
                    TurnEnd::ContextFailed { message } | TurnEnd::CommitFailed { message } => {
                        message
                    }
                    other => format!("{other:?}"),
                })
            })?;
        self.last_usage = None;
        Ok(Compaction::Replaced {
            prepared: kept,
            tokens_before,
            tokens_after,
        })
    }

    /// §3d: consume one provider stream, racing every wait on `cancel`.
    async fn consume_stream(
        &self,
        mut stream: ProviderStream,
        cancel: &CancellationToken,
        timing: &mut Timing,
    ) -> StreamOutcome {
        let mut partial = String::new();
        loop {
            // Invariant 5a: each `next()` races `cancel`, and `biased` makes
            // cancellation win when a terminal event is ready at the same moment (R1).
            let step = tokio::select! {
                biased;
                _ = cancel.cancelled() => StreamStep::Cancelled,
                item = stream.next() => match item {
                    Some(event) => StreamStep::Event(event),
                    None => StreamStep::Ended,
                },
            };
            match step {
                StreamStep::Cancelled => return StreamOutcome::Cancelled(partial),
                StreamStep::Ended => {
                    // §3d: EOF without a terminal event is an exact transport failure.
                    return StreamOutcome::Failed(
                        partial,
                        ProviderError::new(
                            ProviderErrorKind::Transport,
                            "stream ended without a terminal event",
                        ),
                    );
                }
                StreamStep::Event(event) => {
                    // ADR-0121: stamp this stream event's wall-clock position before it
                    // is dispatched; a `Wait` is collected, never history, and a terminal
                    // `Finished` marks the request's end.
                    timing.record_event(self.clock.as_ref(), &event);
                    match event {
                        StreamEvent::TextDelta { text, .. } => {
                            partial.push_str(&text);
                            self.parts.events.emit(AgentEvent::TextDelta { text });
                        }
                        StreamEvent::ReasoningDelta { text, .. } => {
                            self.parts.events.emit(AgentEvent::ReasoningDelta { text });
                        }
                        StreamEvent::ToolInputDelta {
                            call_id,
                            name,
                            text,
                        } => {
                            self.parts.events.emit(AgentEvent::ToolInputDelta {
                                call_id,
                                name,
                                text,
                            });
                        }
                        // ADR-0048: a display-only notice is forwarded in order with
                        // the other events and touches nothing else — no history, no
                        // journal, no partial text.
                        StreamEvent::Notice { text } => {
                            self.parts.events.emit(AgentEvent::ProviderNotice { text });
                        }
                        StreamEvent::Activity => {}
                        // ADR-0121: a provider wait is timing only. `timing.record_event`
                        // already collected it; it is never history, model input or a UI
                        // event (no TUI change in this ADR).
                        StreamEvent::Wait { .. } => {}
                        StreamEvent::Finished(Outcome::Completed(response)) => {
                            // The stream is dropped here; later events are never read.
                            return StreamOutcome::Completed(response);
                        }
                        StreamEvent::Finished(Outcome::Failed(error)) => {
                            return StreamOutcome::Failed(partial, error);
                        }
                        StreamEvent::Finished(Outcome::Cancelled) => {
                            return StreamOutcome::Cancelled(partial);
                        }
                    }
                }
            }
        }
    }

    // ---------------------------------------------------------------- inbox

    /// §3a / §5: commit and install every pending inbox message, in arrival order.
    async fn deliver_inbox(&mut self) -> Result<(), String> {
        // Take the messages pending at this boundary; new arrivals wait for the
        // next boundary. Nothing here holds the lock across an `.await` (5i).
        let messages: Vec<(InboxKind, String)> = {
            let mut queue = self.inbox.queue.lock().unwrap();
            queue.drain(..).collect()
        };
        if messages.is_empty() {
            return Ok(());
        }
        let count = messages.len();
        let mut remaining = messages.into_iter();
        while let Some((kind, text)) = remaining.next() {
            let body = RecordBody::Inbox {
                kind,
                text: text.clone(),
            };
            let record = JournalRecord {
                seq: self.next_seq,
                body,
            };
            let journal = self.parts.journal.clone();
            // Invariant 5b: the record is committed before the message is visible.
            if let Err(error) = journal.commit(&record).await {
                // The failed message was never delivered: put it (and every later
                // one) back, in order, so each is still delivered exactly once.
                let mut queue = self.inbox.queue.lock().unwrap();
                for message in std::iter::once((kind, text)).chain(remaining).rev() {
                    queue.push_front(message);
                }
                return Err(error.0);
            }
            self.next_seq = self.next_seq.saturating_add(1);
            self.history.push(Item::Inbox { kind, text });
        }
        self.parts.events.emit(AgentEvent::InboxDelivered { count });
        Ok(())
    }

    // ---------------------------------------------------------------- tools

    /// Invariant 5e: dispatch is by exact assembled declaration name only.
    fn assembled_tool(&self, call: &ToolCall) -> Option<Arc<dyn Tool>> {
        self.parts
            .tools
            .iter()
            .find(|tool| tool.declaration().name == call.name)
            .cloned()
    }

    /// §4 for one group of calls (ADR-0118 Decision 5): (a) admit each call in block
    /// order, (b) execute the admitted ones concurrently, at most `max_parallel_tools` at
    /// once, starting them in block order, (c) record every result in block order. All
    /// commits happen here, on the turn's own future, so `seq` is dense and the record
    /// order never depends on which call finished first. `Ok(true)` means a call's outcome
    /// ends the turn (ADR-0120); `Err` ends the turn because a commit failed.
    async fn run_group(
        &mut self,
        calls: &[ToolCall],
        tools: &[Option<Arc<dyn Tool>>],
        cancel: &CancellationToken,
    ) -> Result<bool, TurnEnd> {
        // (a) A failed `{ToolStarted}` ends the turn before any call of the group runs.
        let mut admitted = Vec::with_capacity(calls.len());
        for (call, tool) in calls.iter().zip(tools) {
            admitted.push(self.admit(call, tool.as_ref(), cancel).await?);
        }
        // (b) An ordered, buffered join on this future: no task is spawned (§8). A call
        // already executing is awaited, never dropped, whatever `cancel` does (D18).
        // A plain loop, not an iterator adapter: the turn future holds only the call
        // futures, so it stays `Send` for every lifetime the caller picks.
        let limit = self.max_parallel_tools.get();
        let mut running = Vec::new();
        for (index, (admitted, call)) in admitted.iter().zip(calls).enumerate() {
            if let Admitted::Started(tool) = admitted {
                // The first `limit` futures start on the join's first poll, as a sequential
                // call would; only a call behind them waits for a slot.
                let queued = running.len() >= limit;
                running.push(execute(index, call, tool.clone(), cancel, queued));
            }
        }
        let mut executed: Vec<Option<Executed>> = calls.iter().map(|_| None).collect();
        let finished: Vec<(usize, Executed)> = futures_util::stream::iter(running)
            .buffer_unordered(limit)
            .collect()
            .await;
        for (index, result) in finished {
            executed[index] = Some(result);
        }
        // (c) Every result in block order, whatever order the calls finished in.
        let mut ends_turn = false;
        for ((call, admitted), executed) in calls.iter().zip(admitted).zip(executed) {
            match (admitted, executed) {
                (Admitted::Started(_), Some(executed)) => {
                    ends_turn |= executed.ends_turn;
                    self.finish_started(call, executed).await?;
                }
                (Admitted::Settled(status, content), _) => {
                    self.finish_tool(call, status, content).await?;
                }
                // Every started call is in `running` and the join awaits all of them, so
                // this cannot happen; the core stays panic-free (5h) and says so.
                (Admitted::Started(_), None) => {
                    self.finish_tool(call, ToolStatus::Unknown, UNKNOWN_OUTCOME.into())
                        .await?;
                }
            }
        }
        Ok(ends_turn)
    }

    /// Step (a) of §4 for one call: cancellation first (R1), then lookup, then the
    /// authorization raced against `cancel`; a permitted call has its `{ToolStarted}`
    /// committed (before `execute` is called, invariant 5b) and announced.
    async fn admit(
        &mut self,
        call: &ToolCall,
        tool: Option<&Arc<dyn Tool>>,
        cancel: &CancellationToken,
    ) -> Result<Admitted, TurnEnd> {
        // §4 row 1 / R1: cancellation is checked before lookup or authorization.
        if cancel.is_cancelled() {
            return Ok(Admitted::Settled(
                ToolStatus::Cancelled,
                CANCELLED_BEFORE_EXECUTION.into(),
            ));
        }
        let Some(tool) = tool else {
            let name = call.name.as_str();
            return Ok(Admitted::Settled(
                ToolStatus::Unavailable,
                format!("Tool `{name}` is not available."),
            ));
        };
        let identity = tool.identity().clone();
        let effect = tool.effect(call);
        let decision = {
            let authorization = self.parts.authorization.clone();
            let request = AuthorizationRequest {
                call,
                identity: &identity,
                effect,
            };
            tokio::select! {
                biased;
                _ = cancel.cancelled() => None,
                decision = authorization.authorize(request) => Some(decision),
            }
        };
        match decision {
            None => Ok(Admitted::Settled(
                ToolStatus::Cancelled,
                CANCELLED_BEFORE_EXECUTION.into(),
            )),
            Some(Decision::Deny { reason }) => Ok(Admitted::Settled(ToolStatus::Denied, reason)),
            Some(Decision::Permit) => {
                let body = RecordBody::ToolStarted {
                    call_id: call.call_id.clone(),
                    identity,
                };
                if let Err(error) = self.commit(body).await {
                    return Err(TurnEnd::CommitFailed { message: error.0 });
                }
                // R5: remember this call started, so a later turn can report its
                // outcome as `Unknown` if the result never commits.
                self.started_calls.insert(call.call_id.clone());
                self.parts
                    .events
                    .emit(AgentEvent::ToolStarted { call: call.clone() });
                Ok(Admitted::Started(tool.clone()))
            }
        }
    }

    /// Step (c) of §4 for a call that executed: its `{ToolFinished}` with the exit the
    /// host observed, then the history, then the event (R6).
    async fn finish_started(&mut self, call: &ToolCall, executed: Executed) -> Result<(), TurnEnd> {
        let result = ToolResultItem {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            status: executed.outcome.status,
            content: executed.outcome.content,
        };
        // Invariant 5b: `ToolFinished` is committed before the next request.
        if let Err(error) = self
            .commit(RecordBody::ToolFinished {
                result: result.clone(),
                exit_code: Some(executed.exit_code),
            })
            .await
        {
            return Err(TurnEnd::CommitFailed { message: error.0 });
        }
        self.started_calls.remove(&call.call_id);
        self.history.push(Item::ToolResult(result.clone()));
        self.parts.events.emit(AgentEvent::ToolFinished { result });
        Ok(())
    }

    // ------------------------------------------------- R5 reconciliation

    /// R5: every tool call of the history's last assistant item that has no result
    /// yet is resolved here, in block order, before the turn's own records. Nothing
    /// is re-executed and authorization is never asked. A commit failure ends the
    /// turn exactly like any other commit failure.
    async fn reconcile_unresolved_calls(&mut self) -> Result<(), TurnEnd> {
        for call in self.unresolved_calls_of_last_assistant() {
            let started = self.started_calls.contains(&call.call_id);
            let (status, content) = if started {
                (ToolStatus::Unknown, UNKNOWN_OUTCOME.to_string())
            } else {
                (
                    ToolStatus::Cancelled,
                    CANCELLED_BEFORE_EXECUTION.to_string(),
                )
            };
            let result = ToolResultItem {
                call_id: call.call_id,
                name: call.name,
                status,
                content,
            };
            if let Err(error) = self
                .commit(RecordBody::ToolFinished {
                    result: result.clone(),
                    // The host observed no process exit for this call; an explicit
                    // `null`, not a legacy absence.
                    exit_code: Some(None),
                })
                .await
            {
                return Err(TurnEnd::CommitFailed { message: error.0 });
            }
            self.history.push(Item::ToolResult(result.clone()));
            self.parts.events.emit(AgentEvent::ToolFinished { result });
        }
        // The resolved calls have all been answered; nothing is left to track.
        self.started_calls.clear();
        Ok(())
    }

    /// The tool calls of the history's last assistant item that have no result.
    fn unresolved_calls_of_last_assistant(&self) -> Vec<ToolCall> {
        let Some((index, item)) =
            self.history
                .iter()
                .enumerate()
                .rev()
                .find_map(|(index, item)| match item {
                    Item::Assistant(item) => Some((index, item)),
                    _ => None,
                })
        else {
            return Vec::new();
        };
        // An earlier occurrence of a reused id cannot answer this call.
        let resolved: HashSet<&str> = self.history[index + 1..]
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult(result) => Some(result.call_id.as_str()),
                _ => None,
            })
            .collect();
        item.tool_calls()
            .filter(|call| !resolved.contains(call.call_id.as_str()))
            .cloned()
            .collect()
    }

    /// §4 rows 2/3 and the pre-execution cancellation result: record and emit a
    /// result with no preceding `ToolStarted`.
    async fn finish_tool(
        &mut self,
        call: &ToolCall,
        status: ToolStatus,
        content: String,
    ) -> Result<(), TurnEnd> {
        let result = ToolResultItem {
            call_id: call.call_id.clone(),
            name: call.name.clone(),
            status,
            content,
        };
        if let Err(error) = self
            .commit(RecordBody::ToolFinished {
                result: result.clone(),
                // The host observed no process exit for this call; an explicit
                // `null`, not a legacy absence.
                exit_code: Some(None),
            })
            .await
        {
            return Err(TurnEnd::CommitFailed { message: error.0 });
        }
        self.history.push(Item::ToolResult(result.clone()));
        self.parts.events.emit(AgentEvent::ToolFinished { result });
        Ok(())
    }

    // ---------------------------------------------------------------- records

    /// §3d/§7: record an interrupted response and return the turn end it implies.
    /// A failed interruption commit becomes `CommitFailed`.
    async fn end_interrupted(
        &mut self,
        reason: InterruptionReason,
        partial_text: String,
        error: Option<ProviderError>,
        timing: Option<Timing>,
    ) -> TurnEnd {
        let end = match (&reason, &error) {
            (InterruptionReason::Cancelled, _) => TurnEnd::Cancelled,
            (InterruptionReason::ProviderFailed, Some(error)) => TurnEnd::ProviderFailed {
                error: error.clone(),
            },
            // A provider failure always carries its error; this keeps the core
            // panic-free even if one does not (invariant 5h).
            (InterruptionReason::ProviderFailed, None) => TurnEnd::ProviderFailed {
                error: ProviderError::new(
                    ProviderErrorKind::Transport,
                    "stream ended without a terminal event",
                ),
            },
        };
        let body = RecordBody::AssistantInterrupted {
            reason,
            partial_text,
            error,
        };
        if let Err(error) = self.commit(body).await {
            return TurnEnd::CommitFailed { message: error.0 };
        }
        // ADR-0121: the request's timing follows its `AssistantInterrupted`.
        if let Some(mut timing) = timing {
            timing.finish(self.clock.as_ref());
            if let Err(error) = self.commit_request_timing(timing.into_body()).await {
                return TurnEnd::CommitFailed { message: error.0 };
            }
        }
        end
    }

    /// Commit a `RequestTiming` record, but only to a sink whose format carries it
    /// (ADR-0121). A version-1 or version-2 journal answers `false`, so the core
    /// commits none and `seq` stays dense; the caller sees `Ok`.
    async fn commit_request_timing(&mut self, body: RecordBody) -> Result<(), CommitError> {
        if !self.parts.journal.accepts_request_timing() {
            return Ok(());
        }
        self.commit(body).await
    }

    /// Commit one record. Invariant 5b: callers only push history, execute a tool
    /// or send a request after this returns `Ok`.
    async fn commit(&mut self, body: RecordBody) -> Result<(), CommitError> {
        let record = JournalRecord {
            seq: self.next_seq,
            body,
        };
        let journal = self.parts.journal.clone();
        journal.commit(&record).await?;
        // Invariant 5c: `seq` advances only on success, so it stays dense and a
        // failed commit does not burn a number (R2).
        self.next_seq = self.next_seq.saturating_add(1);
        Ok(())
    }

    /// §3c: the request sent to the provider.
    fn build_request(&self) -> ProviderRequest {
        ProviderRequest {
            system_prompt: self.parts.system_prompt.clone(),
            history: self.history.clone(),
            tools: self
                .parts
                .tools
                .iter()
                .map(|tool| tool.declaration().clone())
                .collect(),
            options: self.parts.options.clone(),
        }
    }
}

/// ADR-0118 Decision 4: cut the calls of one response, in block order, into groups. A
/// maximal run of consecutive Shared calls is one group; every other call is a group of its
/// own. A call is Shared only when its tool exists, answers Shared and its effect neither
/// writes files nor delegates. With a bound of 1 every call is its own group, so records and
/// authorization keep the order of sequential execution.
fn groups(
    calls: &[ToolCall],
    tools: &[Option<Arc<dyn Tool>>],
    max_parallel: NonZeroUsize,
) -> Vec<Range<usize>> {
    let shared = |index: usize| {
        max_parallel.get() > 1
            && tools[index].as_ref().is_some_and(|tool| {
                tool.concurrency(&calls[index]) == Concurrency::Shared
                    && !matches!(
                        tool.effect(&calls[index]),
                        Effect::WritesFiles | Effect::Delegates
                    )
            })
    };
    let mut groups = Vec::new();
    let mut start = 0;
    while start < calls.len() {
        let mut end = start + 1;
        if shared(start) {
            while end < calls.len() && shared(end) {
                end += 1;
            }
        }
        groups.push(start..end);
        start = end;
    }
    groups
}

/// Step (b) of §4 for one admitted call: execute it with a child of the turn's token and
/// read, right after its own `execute` returns, whether its outcome ends the turn and the
/// exit the host observed. A `queued` call (one behind the bound) whose turn was cancelled
/// while it waited for a slot does not start; a call that starts on the first poll always
/// executes, with an already cancelled token if cancel fired, exactly as a sequential call.
async fn execute(
    index: usize,
    call: &ToolCall,
    tool: Arc<dyn Tool>,
    cancel: &CancellationToken,
    queued: bool,
) -> (usize, Executed) {
    if queued && cancel.is_cancelled() {
        let outcome = ToolOutcome {
            status: ToolStatus::Cancelled,
            content: CANCELLED_BEFORE_EXECUTION.into(),
        };
        return (
            index,
            Executed {
                outcome,
                ends_turn: false,
                exit_code: None,
            },
        );
    }
    // Invariant 5f: the tool is awaited, never dropped, and its token is a child of the
    // turn's `cancel`.
    let outcome = tool
        .execute(
            call,
            ToolContext {
                cancel: cancel.child_token(),
            },
        )
        .await;
    // ADR-0120: ask the tool (by the outcome, never its name) whether this call ends the
    // turn, in the same poll that saw its `execute` return (#592).
    let ends_turn = tool.ends_turn(&outcome);
    // The exit the host observed for this call, read without consuming it so the session
    // log can read the same record when the event is emitted.
    let exit_code = tool.command_exit_code(&call.call_id);
    (
        index,
        Executed {
            outcome,
            ends_turn,
            exit_code,
        },
    )
}

/// The ONE `Environment` record, shared by the lazy first commit and
/// [`Agent::reconfigure`], so a switched and a constructed assembly journal alike.
fn environment_record(
    provider: &Arc<dyn Provider>,
    tools: &[Arc<dyn Tool>],
    system_prompt: &str,
    options: &ModelOptions,
) -> RecordBody {
    RecordBody::Environment {
        route: provider.describe(),
        system_prompt: system_prompt.to_string(),
        tools: tools
            .iter()
            .map(|tool| (tool.declaration().clone(), tool.identity().clone()))
            .collect(),
        options: options.clone(),
    }
}

/// The ONE environment check, shared by every path that installs parts: §1 rejects
/// duplicate assembled call names before anything else runs; R3 then asks the
/// provider to validate a request as it would be sent. `history` is the transcript
/// the agent will send: empty for [`Agent::new`]'s first request, the history the
/// agent already holds for [`Agent::reconfigure`], and the projection for
/// [`Agent::resume`] — so a switch, a resume and a construction cannot disagree
/// about what the provider accepts (ADR-0049).
fn check_environment(
    provider: &Arc<dyn Provider>,
    tools: &[Arc<dyn Tool>],
    system_prompt: &str,
    options: &ModelOptions,
    history: &[Item],
) -> Result<(), BuildError> {
    let mut names = HashSet::new();
    for tool in tools {
        let name = tool.declaration().name.clone();
        if !names.insert(name.clone()) {
            return Err(BuildError::DuplicateToolName(name));
        }
    }
    let request = ProviderRequest {
        system_prompt: system_prompt.to_string(),
        history: history.to_vec(),
        tools: tools
            .iter()
            .map(|tool| tool.declaration().clone())
            .collect(),
        options: options.clone(),
    };
    provider
        .validate(&request)
        .map_err(BuildError::ProviderRejected)
}

/// §3b: a replacement must be a well-formed transcript BEFORE it becomes history.
/// Every `ToolResult` belongs to a `ToolCall` of an EARLIER `Assistant` item, and
/// every call of a non-final `Assistant` item has its result. A final `Assistant`
/// item may keep unresolved calls (the request loop answers them next). Returns the
/// exact `ContextFailed` message naming the offending call id.
fn validate_replacement(items: &[Item]) -> Result<(), String> {
    let last = items.len().checked_sub(1);
    let mut calls_seen: HashSet<&str> = HashSet::new();
    for (index, item) in items.iter().enumerate() {
        match item {
            Item::Assistant(assistant) => {
                let calls: Vec<&ToolCall> = assistant.tool_calls().collect();
                for call in &calls {
                    calls_seen.insert(call.call_id.as_str());
                }
                if Some(index) != last {
                    for call in calls {
                        let resolved = items[index + 1..].iter().any(|later| match later {
                            Item::ToolResult(result) => result.call_id == call.call_id,
                            _ => false,
                        });
                        if !resolved {
                            return Err(unpaired_message(&call.call_id));
                        }
                    }
                }
            }
            Item::ToolResult(result) if !calls_seen.contains(result.call_id.as_str()) => {
                return Err(unpaired_message(&result.call_id));
            }
            _ => {}
        }
    }
    Ok(())
}

fn unpaired_message(call_id: &str) -> String {
    format!("context policy returned an unpaired tool call or result: {call_id}")
}
