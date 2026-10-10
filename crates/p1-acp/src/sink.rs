//! Nonblocking observation. Sequence numbers order emitted items, not elapsed time.

use crate::plan::{PlanEntry, PlanState};
use crate::turn::{TurnError, TurnStop};
use crate::usage::{SessionUsage, UsageState};
use crate::workflow_card::{CardStatus, StartKind, WorkflowCards};
use p1_contracts::frontend::{WorkflowProgress, WorkflowStep};
use p1_contracts::{AgentEvent, EventSink, Tool, ToolCall, ToolInput};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolCategory {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    Other,
}

/// Display data owned by p1, not a version's tool-call wire representation.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolDisplay {
    pub id: String,
    pub title: String,
    pub name: String,
    pub category: ToolCategory,
    pub input: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq)]
pub enum Update {
    Message(String),
    Thought(String),
    Usage(SessionUsage),
    Plan(Vec<PlanEntry>),
    ToolStarted(ToolDisplay),
    /// A call awaiting the client's permission: the core authorizes before it starts
    /// a call, so the driver announces the call before its permission request. The sink
    /// also sends it for a call that finishes without starting (refused unasked).
    ToolPending(ToolDisplay),
    /// An announced call was permitted and runs now.
    ToolRunning {
        id: String,
    },
    ToolFinished {
        id: String,
        succeeded: bool,
        text: String,
    },
    /// Replace the card's cumulative content; a worker note leaves status alone.
    ToolProgress {
        id: String,
        text: String,
        status: Option<CardStatus>,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    Update(Box<Update>),
    Turn(Result<TurnStop, TurnError>),
    /// Driver logs these to stderr, never as assistant content on the wire.
    Operator(AgentEvent),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stamped {
    pub sequence: u64,
    pub item: Outbound,
}

pub struct AcpSink {
    // Sequence claim and send share one lock so concurrent emitters cannot
    // enqueue sequence 1 before sequence 0.
    output: Mutex<Output>,
    tools: Mutex<Vec<Arc<dyn Tool>>>,
}

struct Output {
    sequence: u64,
    sender: mpsc::UnboundedSender<Stamped>,
    usage: UsageState,
    plan: PlanState,
    cards: WorkflowCards,
}

impl Output {
    fn send(&mut self, item: Outbound) {
        let sequence = self.sequence;
        self.sequence += 1;
        let _ = self.sender.send(Stamped { sequence, item });
    }

    fn updates(&mut self, updates: Vec<Update>) {
        for update in updates {
            self.send(Outbound::Update(Box::new(update)));
        }
    }
}

impl AcpSink {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Stamped>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                output: Mutex::new(Output {
                    sequence: 0,
                    sender: tx,
                    usage: UsageState::default(),
                    plan: PlanState::default(),
                    cards: WorkflowCards::default(),
                }),
                tools: Mutex::new(Vec::new()),
            },
            rx,
        )
    }

    /// The effective parent window, never a guessed provider capacity.
    pub fn context_configured(&self, window_tokens: Option<u64>) {
        self.output.lock().unwrap().usage.window_tokens = window_tokens;
    }

    /// Record run order before parallel runs can emit their first steps.
    pub fn workflow_started(&self, run: &str) {
        self.output.lock().unwrap().plan.begin(run);
    }

    /// Every step event emits the full session snapshot, including repeated starts
    /// for a fallback. Identity is run + ordinal, never call id or worker id.
    pub fn workflow_step(&self, step: &WorkflowStep) {
        let mut output = self.output.lock().unwrap();
        let entries = output.plan.observe(step);
        output.send(Outbound::Update(Box::new(Update::Plan(entries))));
    }

    pub fn workflow_progress(&self, progress: &WorkflowProgress) {
        let mut output = self.output.lock().unwrap();
        let updates = output.cards.progress(progress);
        output.updates(updates);
    }

    pub fn worker_ended(&self, worker: &str, note: &str) {
        let mut output = self.output.lock().unwrap();
        let updates = output.cards.worker_ended(worker, note);
        output.updates(updates);
    }

    /// The driver's FrontEnd::parent_tools hook supplies the assembled tools.
    /// Replace the snapshot at assembly, never infer targets from argument keys.
    pub fn parent_tools(&self, tools: &[Arc<dyn Tool>]) {
        *self.tools.lock().unwrap() = tools.to_vec();
    }

    pub fn describe_call(&self, call: &ToolCall) -> ToolDisplay {
        let tool = self
            .tools
            .lock()
            .unwrap()
            .iter()
            .find(|tool| tool.declaration().name == call.name)
            .cloned();
        let (title, category) = if let Some(tool) = tool {
            let description = tool.describe(call);
            let title = match description.target {
                Some(target) => format!("{} {target}", description.verb),
                None => description.verb.to_string(),
            };
            (title, tool_category(description.verb))
        } else {
            (call.name.clone(), tool_category(&call.name))
        };
        ToolDisplay {
            id: call.call_id.clone(),
            title,
            name: call.name.clone(),
            category,
            input: raw_input(call),
        }
    }
}

pub(crate) fn raw_input(call: &ToolCall) -> serde_json::Value {
    match &call.input {
        ToolInput::Json(raw) => {
            serde_json::from_str(raw).unwrap_or_else(|_| serde_json::Value::String(raw.clone()))
        }
        ToolInput::Text(raw) => serde_json::Value::String(raw.clone()),
    }
}

/// Small display-only table; unknown or renamed tools stay `other`.
pub(crate) fn tool_category(verb: &str) -> ToolCategory {
    match verb {
        "read" | "read_output" => ToolCategory::Read,
        "edit" | "write" | "apply_patch" => ToolCategory::Edit,
        "delete" => ToolCategory::Delete,
        "move" | "rename" => ToolCategory::Move,
        "search" | "grep" => ToolCategory::Search,
        "execute" | "run" | "shell" => ToolCategory::Execute,
        "think" => ToolCategory::Think,
        "fetch" => ToolCategory::Fetch,
        _ => ToolCategory::Other,
    }
}

impl EventSink for AcpSink {
    fn emit(&self, event: AgentEvent) {
        let mut output = self.output.lock().unwrap();
        let item = match event {
            AgentEvent::TextDelta { text } => Outbound::Update(Box::new(Update::Message(text))),
            AgentEvent::ReasoningDelta { text } => {
                Outbound::Update(Box::new(Update::Thought(text)))
            }
            AgentEvent::ToolStarted { call } => {
                let kind = {
                    let tools = self.tools.lock().unwrap();
                    let tool = tools
                        .iter()
                        .find(|tool| tool.declaration().name == call.name);
                    StartKind::for_tool(&call.name, tool.map(|tool| tool.identity()))
                };
                output.cards.started(&call.call_id, kind);
                Outbound::Update(Box::new(Update::ToolStarted(self.describe_call(&call))))
            }
            AgentEvent::ToolFinished { result } => {
                // A refused call never started: announce it so its result has a card.
                if !output.cards.knows(&result.call_id) {
                    let display = ToolDisplay {
                        id: result.call_id.clone(),
                        title: result.name.clone(),
                        name: result.name.clone(),
                        category: tool_category(&result.name),
                        input: serde_json::Value::Null,
                    };
                    output.updates(vec![Update::ToolPending(display)]);
                }
                let updates = output.cards.finished(result);
                output.updates(updates);
                return;
            }
            AgentEvent::TurnFinished { end } => {
                let notes = output.cards.turn_ended();
                output.updates(notes);
                Outbound::Turn(crate::turn::prompt_outcome(end))
            }
            AgentEvent::ResponseCompleted { usage, .. } => {
                let Some(usage) = output.usage.completed(usage) else {
                    return;
                };
                Outbound::Update(Box::new(Update::Usage(usage)))
            }
            AgentEvent::TurnStarted
            | AgentEvent::RequestStarted { .. }
            | AgentEvent::ToolInputDelta { .. } => return,
            event @ (AgentEvent::ProviderNotice { .. }
            | AgentEvent::InboxDelivered { .. }
            | AgentEvent::ContextReplaced { .. }) => Outbound::Operator(event),
        };
        output.send(item);
    }
}
