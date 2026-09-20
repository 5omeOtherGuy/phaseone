//! Scripted fakes for testing against the p1 contracts.
//!
//! Test dependency only. Everything here is deterministic: no clocks, no sleeps, no
//! network, no filesystem. Synchronisation points are explicit (`Notify`-based).

use std::collections::VecDeque;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};

use p1_contracts::{
    AgentEvent, AssistantBlock, AssistantItem, AuthorizationPolicy, AuthorizationRequest,
    BoxFuture, CancellationToken, CommitError, CommitSink, CompletedResponse, ContextError,
    ContextInput, ContextPolicy, Decision, DeclarationKind, Effect, EventSink, Item, JournalRecord,
    Origin, Outcome, Prepared, Provider, ProviderError, ProviderRequest, ProviderStream,
    RouteDescription, StopReason, StreamEvent, Tool, ToolCall, ToolContext, ToolDeclaration,
    ToolIdentity, ToolInput, ToolOutcome, Usage,
};
use tokio::sync::Notify;

// ---------------------------------------------------------------- builders

pub fn origin() -> Origin {
    Origin {
        route: "fake-route".into(),
        model: "fake-model".into(),
    }
}

pub fn json_call(call_id: &str, name: &str, raw_json: &str) -> ToolCall {
    ToolCall {
        call_id: call_id.into(),
        name: name.into(),
        input: ToolInput::Json(raw_json.into()),
    }
}

/// A completed response made of the given blocks.
pub fn completed(blocks: Vec<AssistantBlock>, stop: StopReason, usage: Option<Usage>) -> Outcome {
    Outcome::Completed(CompletedResponse {
        item: AssistantItem {
            origin: origin(),
            blocks,
        },
        stop,
        usage,
    })
}

pub fn text_block(text: &str) -> AssistantBlock {
    AssistantBlock::Text { text: text.into() }
}

/// Stream script for a plain text answer: one delta, then `Finished(Completed)`.
pub fn text_response(text: &str) -> Step {
    Step::Events(vec![
        StreamEvent::TextDelta {
            block: 0,
            text: text.into(),
        },
        StreamEvent::Finished(completed(vec![text_block(text)], StopReason::EndTurn, None)),
    ])
}

/// Stream script for a response that requests the given tool calls.
pub fn tool_call_response(calls: Vec<ToolCall>) -> Step {
    let blocks = calls.into_iter().map(AssistantBlock::ToolCall).collect();
    Step::Events(vec![StreamEvent::Finished(completed(
        blocks,
        StopReason::ToolUse,
        None,
    ))])
}

// ---------------------------------------------------------------- provider

/// What the scripted provider does for ONE `stream` call.
#[derive(Debug, Clone)]
pub enum Step {
    /// `stream` returns `Err` (setup failure, no stream exists).
    SetupError(ProviderError),
    /// Yields these events, then the stream ENDS (`None`).
    Events(Vec<StreamEvent>),
    /// Yields these events, then stays pending forever and IGNORES cancellation —
    /// the consumer must stop waiting on its own.
    EventsThenHang(Vec<StreamEvent>),
    /// Yields these events, then waits for cancellation and yields
    /// `Finished(Outcome::Cancelled)`.
    EventsThenAwaitCancel(Vec<StreamEvent>),
}

#[derive(Default)]
struct ProviderState {
    script: VecDeque<Step>,
    requests: Vec<ProviderRequest>,
    validated: Vec<ProviderRequest>,
}

/// A provider that replays a script and records every request it was given.
/// Panics if `stream` is called more often than the script has steps.
#[derive(Clone)]
pub struct ScriptedProvider {
    state: Arc<Mutex<ProviderState>>,
    /// Notified each time a stream has yielded all its scripted events.
    pub drained: Arc<Notify>,
    reject_validation: Option<ProviderError>,
}

impl ScriptedProvider {
    pub fn new(script: Vec<Step>) -> Self {
        Self {
            state: Arc::new(Mutex::new(ProviderState {
                script: script.into(),
                requests: Vec::new(),
                validated: Vec::new(),
            })),
            drained: Arc::new(Notify::new()),
            reject_validation: None,
        }
    }

    /// Make `validate` fail with this error.
    pub fn rejecting_validation(mut self, error: ProviderError) -> Self {
        self.reject_validation = Some(error);
        self
    }

    pub fn requests(&self) -> Vec<ProviderRequest> {
        self.state.lock().unwrap().requests.clone()
    }

    /// Every request `validate` was asked about, in order.
    pub fn validated(&self) -> Vec<ProviderRequest> {
        self.state.lock().unwrap().validated.clone()
    }

    pub fn remaining_steps(&self) -> usize {
        self.state.lock().unwrap().script.len()
    }
}

impl Provider for ScriptedProvider {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: origin(),
            supports_freeform_tools: true,
            mandatory_prompt_prefix: None,
            reports_cost: false,
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.state.lock().unwrap().validated.push(request.clone());
        match &self.reject_validation {
            Some(error) => Err(error.clone()),
            None => Ok(()),
        }
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            let step = {
                let mut state = self.state.lock().unwrap();
                state.requests.push(request);
                state
                    .script
                    .pop_front()
                    .expect("ScriptedProvider: stream called more often than scripted")
            };
            let (events, tail) = match step {
                Step::SetupError(error) => return Err(error),
                Step::Events(events) => (events, Tail::End),
                Step::EventsThenHang(events) => (events, Tail::Hang),
                Step::EventsThenAwaitCancel(events) => (events, Tail::AwaitCancel),
            };
            let stream: ProviderStream = Box::pin(ScriptedStream {
                events: events.into(),
                tail,
                cancelled: Box::pin(async move { cancel.cancelled_owned().await }),
                drained: self.drained.clone(),
                announced: false,
            });
            Ok(stream)
        })
    }
}

enum Tail {
    End,
    Hang,
    AwaitCancel,
    Done,
}

struct ScriptedStream {
    events: VecDeque<StreamEvent>,
    tail: Tail,
    cancelled: Pin<Box<dyn Future<Output = ()> + Send>>,
    drained: Arc<Notify>,
    announced: bool,
}

impl futures_core::Stream for ScriptedStream {
    type Item = StreamEvent;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<StreamEvent>> {
        if let Some(event) = self.events.pop_front() {
            return Poll::Ready(Some(event));
        }
        if !self.announced {
            self.announced = true;
            self.drained.notify_one();
        }
        match self.tail {
            Tail::End | Tail::Done => Poll::Ready(None),
            Tail::Hang => Poll::Pending,
            Tail::AwaitCancel => match self.cancelled.as_mut().poll(cx) {
                Poll::Ready(()) => {
                    self.tail = Tail::Done;
                    Poll::Ready(Some(StreamEvent::Finished(Outcome::Cancelled)))
                }
                Poll::Pending => Poll::Pending,
            },
        }
    }
}

// ---------------------------------------------------------------- tools

#[derive(Debug, Clone)]
enum Behaviour {
    Return(ToolOutcome),
    /// Signals `started`, then waits for cancellation and returns `Cancelled`.
    RunUntilCancelled,
}

/// A tool that records its calls and returns a fixed outcome.
#[derive(Clone)]
pub struct FakeTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    effect: Effect,
    behaviour: Behaviour,
    calls: Arc<Mutex<Vec<ToolCall>>>,
    /// Notified when an execution has begun.
    pub started: Arc<Notify>,
}

impl FakeTool {
    /// A function tool named `name` returning `Ok` with content `"<name> ok"`.
    pub fn new(name: &str) -> Self {
        Self {
            declaration: ToolDeclaration {
                name: name.into(),
                description: format!("fake tool {name}"),
                kind: DeclarationKind::Function {
                    input_schema: serde_json_object(),
                },
            },
            identity: ToolIdentity {
                implementation: format!("fake-{name}"),
                variant: "test".into(),
            },
            effect: Effect::ReadOnly,
            behaviour: Behaviour::Return(ToolOutcome::ok(format!("{name} ok"))),
            calls: Arc::new(Mutex::new(Vec::new())),
            started: Arc::new(Notify::new()),
        }
    }

    pub fn with_effect(mut self, effect: Effect) -> Self {
        self.effect = effect;
        self
    }

    pub fn returning(mut self, outcome: ToolOutcome) -> Self {
        self.behaviour = Behaviour::Return(outcome);
        self
    }

    pub fn running_until_cancelled(mut self) -> Self {
        self.behaviour = Behaviour::RunUntilCancelled;
        self
    }

    pub fn with_identity(mut self, implementation: &str, variant: &str) -> Self {
        self.identity = ToolIdentity {
            implementation: implementation.into(),
            variant: variant.into(),
        };
        self
    }

    /// Every call this tool was asked to execute, in order.
    pub fn calls(&self) -> Vec<ToolCall> {
        self.calls.lock().unwrap().clone()
    }
}

fn serde_json_object() -> p1_contracts::serde_json::Value {
    p1_contracts::serde_json::json!({"type": "object"})
}

impl Tool for FakeTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        self.effect
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(call.clone());
            self.started.notify_one();
            match &self.behaviour {
                Behaviour::Return(outcome) => outcome.clone(),
                Behaviour::RunUntilCancelled => {
                    context.cancel.cancelled().await;
                    ToolOutcome {
                        status: p1_contracts::ToolStatus::Cancelled,
                        content: "cancelled".into(),
                    }
                }
            }
        })
    }
}

// ---------------------------------------------------------------- journal

/// In-memory commit sink that records everything and can be told to fail.
type CommitHook = Arc<dyn Fn(&JournalRecord) + Send + Sync>;

#[derive(Clone, Default)]
pub struct RecordingJournal {
    records: Arc<Mutex<Vec<JournalRecord>>>,
    fail_at_seq: Arc<Mutex<Option<u64>>>,
    fail_once_at_seq: Arc<Mutex<Option<u64>>>,
    hook: Option<CommitHook>,
}

impl RecordingJournal {
    pub fn new() -> Self {
        Self::default()
    }

    /// The commit of the record with this `seq` (and every later commit) fails.
    pub fn failing_at(self, seq: u64) -> Self {
        *self.fail_at_seq.lock().unwrap() = Some(seq);
        self
    }

    /// Only the FIRST commit attempt of the record with this `seq` fails; later
    /// attempts (of any record) succeed.
    pub fn failing_once_at(self, seq: u64) -> Self {
        *self.fail_once_at_seq.lock().unwrap() = Some(seq);
        self
    }

    /// Run `hook` synchronously inside every SUCCESSFUL commit, after the record is
    /// stored and before `commit` returns. A deterministic way to act at an exact
    /// boundary: e.g. send an inbox message or cancel the turn right after
    /// `AssistantCompleted` is committed.
    pub fn with_commit_hook(
        mut self,
        hook: impl Fn(&JournalRecord) + Send + Sync + 'static,
    ) -> Self {
        self.hook = Some(Arc::new(hook));
        self
    }

    pub fn records(&self) -> Vec<JournalRecord> {
        self.records.lock().unwrap().clone()
    }
}

impl CommitSink for RecordingJournal {
    fn commit<'a>(&'a self, record: &'a JournalRecord) -> BoxFuture<'a, Result<(), CommitError>> {
        Box::pin(async move {
            if let Some(seq) = *self.fail_at_seq.lock().unwrap()
                && record.seq >= seq
            {
                return Err(CommitError(format!("scripted failure at seq {seq}")));
            }
            {
                let mut once = self.fail_once_at_seq.lock().unwrap();
                if *once == Some(record.seq) {
                    *once = None;
                    return Err(CommitError(format!(
                        "scripted one-time failure at seq {}",
                        record.seq
                    )));
                }
            }
            self.records.lock().unwrap().push(record.clone());
            if let Some(hook) = &self.hook {
                hook(record);
            }
            Ok(())
        })
    }
}

// ---------------------------------------------------------------- policies, events

/// Permits everything except calls to the named tools, which are denied with
/// reason `"denied by test policy"`. Records every request it saw as
/// `(call_id, effect)`.
#[derive(Clone, Default)]
pub struct ScriptedAuthorization {
    deny_names: Vec<String>,
    seen: Arc<Mutex<Vec<(String, Effect)>>>,
}

impl ScriptedAuthorization {
    pub fn permit_all() -> Self {
        Self::default()
    }

    pub fn denying(names: &[&str]) -> Self {
        Self {
            deny_names: names.iter().map(|name| name.to_string()).collect(),
            seen: Arc::default(),
        }
    }

    pub fn seen(&self) -> Vec<(String, Effect)> {
        self.seen.lock().unwrap().clone()
    }
}

impl AuthorizationPolicy for ScriptedAuthorization {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            self.seen
                .lock()
                .unwrap()
                .push((request.call.call_id.clone(), request.effect));
            if self.deny_names.contains(&request.call.name) {
                Decision::Deny {
                    reason: "denied by test policy".into(),
                }
            } else {
                Decision::Permit
            }
        })
    }
}

/// Sends the history unchanged.
#[derive(Clone, Copy, Default)]
pub struct PassthroughContext;

impl ContextPolicy for PassthroughContext {
    fn prepare<'a>(
        &'a self,
        _input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async { Ok(None) })
    }
}

/// Replaces the history with `replacement` WHENEVER it holds more than
/// `when_longer_than` items; otherwise passes it through. `failing()` always errors.
#[derive(Clone)]
pub struct ReplacingContext {
    pub when_longer_than: usize,
    pub replacement: Vec<Item>,
    /// What the replacement is reported to have cost; `None` = unknown.
    pub usage: Option<Usage>,
    fail: bool,
}

impl ReplacingContext {
    pub fn new(when_longer_than: usize, replacement: Vec<Item>) -> Self {
        Self {
            when_longer_than,
            replacement,
            usage: None,
            fail: false,
        }
    }

    pub fn failing() -> Self {
        Self {
            when_longer_than: 0,
            replacement: Vec::new(),
            usage: None,
            fail: true,
        }
    }

    /// Report a usage for the replacement (the cost of preparing it).
    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }
}

impl ContextPolicy for ReplacingContext {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            if self.fail {
                return Err(ContextError::Failed("scripted context failure".into()));
            }
            if input.history.len() > self.when_longer_than {
                Ok(Some(Prepared {
                    items: self.replacement.clone(),
                    usage: self.usage,
                }))
            } else {
                Ok(None)
            }
        })
    }
}

/// A context policy that signals `started` and then waits for `release`. If the
/// turn's `cancel` fires first it returns `Err(ContextError::Cancelled)`, exactly
/// as a real summarizer must when its request is abandoned.
#[derive(Clone)]
pub struct GatedContext {
    /// Notified when a preparation has begun.
    pub started: Arc<Notify>,
    /// Notify this to let a waiting preparation return its replacement.
    pub release: Arc<Notify>,
    replacement: Vec<Item>,
    usage: Option<Usage>,
}

impl GatedContext {
    pub fn new(replacement: Vec<Item>) -> Self {
        Self {
            started: Arc::new(Notify::new()),
            release: Arc::new(Notify::new()),
            replacement,
            usage: None,
        }
    }

    pub fn with_usage(mut self, usage: Usage) -> Self {
        self.usage = Some(usage);
        self
    }
}

impl ContextPolicy for GatedContext {
    fn prepare<'a>(
        &'a self,
        input: ContextInput<'a>,
    ) -> BoxFuture<'a, Result<Option<Prepared>, ContextError>> {
        Box::pin(async move {
            self.started.notify_one();
            tokio::select! {
                biased;
                _ = input.cancel.cancelled() => Err(ContextError::Cancelled),
                _ = self.release.notified() => Ok(Some(Prepared {
                    items: self.replacement.clone(),
                    usage: self.usage,
                })),
            }
        })
    }
}

#[derive(Clone, Default)]
pub struct RecordingEvents {
    events: Arc<Mutex<Vec<AgentEvent>>>,
}

impl RecordingEvents {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn events(&self) -> Vec<AgentEvent> {
        self.events.lock().unwrap().clone()
    }
}

impl EventSink for RecordingEvents {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}
