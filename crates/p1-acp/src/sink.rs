//! Nonblocking observation. Sequence numbers order emitted items, not elapsed time.

use crate::plan::{PlanEntry, PlanState};
use crate::turn::{TurnError, TurnStop};
use crate::usage::{SessionUsage, UsageState};
use p1_contracts::frontend::WorkflowStep;
use p1_contracts::{AgentEvent, EventSink, Tool, ToolCall, ToolInput, ToolStatus};
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
    /// a call, so the driver announces the call before its permission request.
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
}

impl Output {
    fn send(&mut self, item: Outbound) {
        let sequence = self.sequence;
        self.sequence += 1;
        let _ = self.sender.send(Stamped { sequence, item });
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
                Outbound::Update(Box::new(Update::ToolStarted(self.describe_call(&call))))
            }
            AgentEvent::ToolFinished { result } => {
                Outbound::Update(Box::new(Update::ToolFinished {
                    id: result.call_id,
                    succeeded: result.status == ToolStatus::Ok,
                    text: result.content,
                }))
            }
            AgentEvent::TurnFinished { end } => Outbound::Turn(crate::turn::prompt_outcome(end)),
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
