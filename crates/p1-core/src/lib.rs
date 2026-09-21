//! The p1 agent core: one agent's request → stream → tool calls → repeat loop.
//!
//! The core knows only `p1-contracts`. It never names a provider, a tool, a file
//! format, a prompt template or a UI. Behaviour is specified in
//! `docs/design/core.md`; that note is authoritative.

use std::collections::{HashSet, VecDeque};
use std::sync::{Arc, Mutex, Weak};

use futures_util::StreamExt;
use p1_contracts::{
    AgentEvent, AuthorizationPolicy, AuthorizationRequest, CancellationToken, CommitError,
    CommitSink, CompletedResponse, ContextError, ContextInput, ContextPolicy, Decision, EventSink,
    InboxKind, InterruptionReason, Item, JournalRecord, ModelOptions, Outcome, Prepared, Provider,
    ProviderError, ProviderErrorKind, ProviderRequest, ProviderStream, RecordBody, StopReason,
    StreamEvent, Tool, ToolCall, ToolContext, ToolResultItem, ToolStatus, TurnEnd, Usage,
};
use tokio::sync::Notify;

mod resume;
pub use resume::{Projection, ResumeError, ResumeReport, UnresolvedCall, project};

/// R5: exact model-visible content for a call whose `ToolStarted` was committed but
/// whose outcome was never recorded.
const UNKNOWN_OUTCOME: &str = "Interrupted: this call was started before the session stopped and its outcome is unknown. Check the current state before retrying.";

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

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BuildError {
    #[error("two assembled tools share the call name `{0}`")]
    DuplicateToolName(String),
    #[error("the provider rejected this environment: {0}")]
    ProviderRejected(ProviderError),
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
    inbox: Arc<InboxShared>,
}

/// What one request-loop iteration wants next.
enum Flow {
    Continue,
    End(TurnEnd),
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

impl Agent {
    /// Fails before anything runs if the environment is incoherent.
    pub fn new(parts: AgentParts) -> Result<Self, BuildError> {
        Self::assemble(parts, Vec::new(), 0, false, HashSet::new(), None)
    }

    /// Validate the parts (exactly as [`Agent::new`] does) and install `history`,
    /// `next_seq`, `environment_committed`, `started_calls` and `last_usage`.
    /// Shared with `resume`, so construction and its `BuildError`s exist once.
    pub(crate) fn assemble(
        parts: AgentParts,
        history: Vec<Item>,
        next_seq: u64,
        environment_committed: bool,
        started_calls: HashSet<String>,
        last_usage: Option<Usage>,
    ) -> Result<Self, BuildError> {
        // §1: reject duplicate assembled call names before anything else runs.
        let mut names = HashSet::new();
        for tool in &parts.tools {
            let name = tool.declaration().name.clone();
            if !names.insert(name.clone()) {
                return Err(BuildError::DuplicateToolName(name));
            }
        }
        // R3: validate exactly once, against the empty-history first request.
        let request = ProviderRequest {
            system_prompt: parts.system_prompt.clone(),
            history: Vec::new(),
            tools: parts
                .tools
                .iter()
                .map(|tool| tool.declaration().clone())
                .collect(),
            options: parts.options.clone(),
        };
        parts
            .provider
            .validate(&request)
            .map_err(BuildError::ProviderRejected)?;
        Ok(Self {
            parts,
            history,
            next_seq,
            environment_committed,
            started_calls,
            last_usage,
            inbox: Arc::new(InboxShared {
                queue: Mutex::new(VecDeque::new()),
                notify: Notify::new(),
            }),
        })
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
        let body = RecordBody::Environment {
            route: self.parts.provider.describe(),
            system_prompt: self.parts.system_prompt.clone(),
            tools: self
                .parts
                .tools
                .iter()
                .map(|tool| (tool.declaration().clone(), tool.identity().clone()))
                .collect(),
            options: self.parts.options.clone(),
        };
        self.commit(body).await.map_err(|error| error.0)?;
        self.environment_committed = true;
        Ok(())
    }

    async fn request_loop(&mut self, cancel: &CancellationToken) -> TurnEnd {
        let mut request_index: u32 = 0;
        loop {
            match self.one_request(request_index, cancel).await {
                Flow::Continue => request_index = request_index.wrapping_add(1),
                Flow::End(end) => return end,
            }
        }
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
                    .end_interrupted(InterruptionReason::Cancelled, String::new(), None)
                    .await;
                return Flow::End(end);
            }
            Some(Ok(None)) => {}
            Some(Ok(Some(Prepared { items, usage }))) => {
                // A policy bug must not become a provider 400 three requests later.
                if let Err(message) = validate_replacement(&items) {
                    return Flow::End(TurnEnd::ContextFailed { message });
                }
                let items_after = items.len();
                let body = RecordBody::ContextReplaced {
                    items: items.clone(),
                    usage,
                };
                if let Err(error) = self.commit(body).await {
                    return Flow::End(TurnEnd::CommitFailed { message: error.0 });
                }
                self.history = items;
                // R6: the event announces the committed record, so it comes after.
                self.parts.events.emit(AgentEvent::ContextReplaced {
                    items_before,
                    items_after,
                    usage,
                });
            }
            // The policy itself gave up, with or without the turn's token firing.
            Some(Err(ContextError::Cancelled)) => {
                let end = self
                    .end_interrupted(InterruptionReason::Cancelled, String::new(), None)
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
                .end_interrupted(InterruptionReason::Cancelled, String::new(), None)
                .await;
            return Flow::End(end);
        }
        let provider = self.parts.provider.clone();
        let request = self.build_request();
        let cancel_child = cancel.child_token();
        // Invariant 5a: the `stream()` setup future itself races `cancel` (R1).
        let stream_result = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            result = provider.stream(request, cancel_child) => Some(result),
        };
        let stream = match stream_result {
            None => {
                let end = self
                    .end_interrupted(InterruptionReason::Cancelled, String::new(), None)
                    .await;
                return Flow::End(end);
            }
            Some(Err(error)) => {
                let end = self
                    .end_interrupted(
                        InterruptionReason::ProviderFailed,
                        String::new(),
                        Some(error),
                    )
                    .await;
                return Flow::End(end);
            }
            Some(Ok(stream)) => stream,
        };
        // 3d: consume the stream to its terminal event.
        let outcome = self.consume_stream(stream, cancel).await;
        let response = match outcome {
            StreamOutcome::Completed(response) => response,
            StreamOutcome::Cancelled(partial) => {
                let end = self
                    .end_interrupted(InterruptionReason::Cancelled, partial, None)
                    .await;
                return Flow::End(end);
            }
            StreamOutcome::Failed(partial, error) => {
                let end = self
                    .end_interrupted(InterruptionReason::ProviderFailed, partial, Some(error))
                    .await;
                return Flow::End(end);
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
        // 3g: tool calls, strictly sequentially, in block order.
        for call in &calls {
            // Invariant 5d: each call gets its result here, before the next request.
            // If a commit inside fails, R5 reconciles the leftovers next turn.
            if let Err(end) = self.run_tool_call(call, cancel).await {
                return Flow::End(end);
            }
        }
        if cancel.is_cancelled() {
            return Flow::End(TurnEnd::Cancelled);
        }
        Flow::Continue
    }

    /// §3d: consume one provider stream, racing every wait on `cancel`.
    async fn consume_stream(
        &self,
        mut stream: ProviderStream,
        cancel: &CancellationToken,
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
                StreamStep::Event(event) => match event {
                    StreamEvent::TextDelta { text, .. } => {
                        partial.push_str(&text);
                        self.parts.events.emit(AgentEvent::TextDelta { text });
                    }
                    StreamEvent::ReasoningDelta { text, .. } => {
                        self.parts.events.emit(AgentEvent::ReasoningDelta { text });
                    }
                    StreamEvent::ToolInputDelta { call_id, text } => {
                        self.parts
                            .events
                            .emit(AgentEvent::ToolInputDelta { call_id, text });
                    }
                    // ADR-0048: a display-only notice is forwarded in order with
                    // the other events and touches nothing else — no history, no
                    // journal, no partial text.
                    StreamEvent::Notice { text } => {
                        self.parts.events.emit(AgentEvent::ProviderNotice { text });
                    }
                    StreamEvent::Activity => {}
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
                },
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

    /// §4: one tool call, in block order. `Err` ends the turn (a failed commit).
    async fn run_tool_call(
        &mut self,
        call: &ToolCall,
        cancel: &CancellationToken,
    ) -> Result<(), TurnEnd> {
        // §4 row 1 / R1: cancellation is checked before lookup or authorization.
        if cancel.is_cancelled() {
            return self
                .finish_tool(
                    call,
                    ToolStatus::Cancelled,
                    "Cancelled before execution.".into(),
                )
                .await;
        }
        // Invariant 5e: dispatch is by exact assembled declaration name only.
        let tool = self
            .parts
            .tools
            .iter()
            .find(|tool| tool.declaration().name == call.name)
            .cloned();
        let Some(tool) = tool else {
            let name = call.name.as_str();
            return self
                .finish_tool(
                    call,
                    ToolStatus::Unavailable,
                    format!("Tool `{name}` is not available."),
                )
                .await;
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
            authorization.authorize(request).await
        };
        match decision {
            Decision::Deny { reason } => self.finish_tool(call, ToolStatus::Denied, reason).await,
            Decision::Permit => {
                // §4: `ToolStarted` is committed BEFORE `execute`; if that commit
                // fails the tool is not executed (invariant 5b).
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
                // Invariant 5f: the tool is awaited, never dropped, and its token
                // is a child of the turn's `cancel`.
                let child = cancel.child_token();
                let outcome = tool.execute(call, ToolContext { cancel: child }).await;
                let result = ToolResultItem {
                    call_id: call.call_id.clone(),
                    name: call.name.clone(),
                    status: outcome.status,
                    content: outcome.content,
                };
                // Invariant 5b: `ToolFinished` is committed before the next request.
                if let Err(error) = self
                    .commit(RecordBody::ToolFinished {
                        result: result.clone(),
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
        }
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
                    "Cancelled before execution.".to_string(),
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
        let resolved: HashSet<&str> = self
            .history
            .iter()
            .filter_map(|item| match item {
                Item::ToolResult(result) => Some(result.call_id.as_str()),
                _ => None,
            })
            .collect();
        let Some(item) = self.history.iter().rev().find_map(|item| match item {
            Item::Assistant(item) => Some(item),
            _ => None,
        }) else {
            return Vec::new();
        };
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
        end
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
