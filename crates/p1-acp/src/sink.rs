//! Nonblocking observation. Sequence numbers order emitted items, not elapsed time.

use crate::turn::{TurnError, TurnStop};
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
    ToolStarted(ToolDisplay),
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
    output: Mutex<(u64, mpsc::UnboundedSender<Stamped>)>,
    tools: Mutex<Vec<Arc<dyn Tool>>>,
}

impl AcpSink {
    pub fn new() -> (Self, mpsc::UnboundedReceiver<Stamped>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                output: Mutex::new((0, tx)),
                tools: Mutex::new(Vec::new()),
            },
            rx,
        )
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
            AgentEvent::TurnStarted
            | AgentEvent::RequestStarted { .. }
            | AgentEvent::ToolInputDelta { .. } => return,
            event @ (AgentEvent::ProviderNotice { .. }
            | AgentEvent::InboxDelivered { .. }
            | AgentEvent::ContextReplaced { .. }
            | AgentEvent::ResponseCompleted { .. }) => Outbound::Operator(event),
        };
        let mut output = self.output.lock().unwrap();
        let sequence = output.0;
        output.0 += 1;
        let _ = output.1.send(Stamped { sequence, item });
    }
}
