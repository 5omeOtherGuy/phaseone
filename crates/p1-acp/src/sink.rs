//! Nonblocking observation. Sequence numbers order emitted items, not elapsed time.

use agent_client_protocol_schema::v1::{
    self as acp, ContentChunk, SessionUpdate, ToolCallUpdateFields, ToolKind,
};
use p1_contracts::{AgentEvent, EventSink, Tool, ToolCall, ToolInput, ToolStatus};
use std::sync::{Arc, Mutex};
use tokio::sync::mpsc;

#[derive(Debug, Clone, PartialEq)]
pub enum Outbound {
    Update(SessionUpdate),
    Turn(Result<acp::PromptResponse, acp::Error>),
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

    pub fn tool_call(&self, call: &ToolCall) -> acp::ToolCall {
        let tool = self
            .tools
            .lock()
            .unwrap()
            .iter()
            .find(|tool| tool.declaration().name == call.name)
            .cloned();
        let (title, kind) = if let Some(tool) = tool {
            let description = tool.describe(call);
            let title = match description.target {
                Some(target) => format!("{} {target}", description.verb),
                None => description.verb.to_string(),
            };
            (title, tool_kind(description.verb))
        } else {
            (call.name.clone(), tool_kind(&call.name))
        };
        acp::ToolCall::new(call.call_id.clone(), title)
            .name(call.name.clone())
            .kind(kind)
            .status(acp::ToolCallStatus::InProgress)
            .raw_input(raw_input(call))
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
fn tool_kind(verb: &str) -> ToolKind {
    match verb {
        "read" | "read_output" => ToolKind::Read,
        "edit" | "write" | "apply_patch" => ToolKind::Edit,
        "delete" => ToolKind::Delete,
        "move" | "rename" => ToolKind::Move,
        "search" | "grep" => ToolKind::Search,
        "execute" | "run" | "shell" => ToolKind::Execute,
        "think" => ToolKind::Think,
        "fetch" => ToolKind::Fetch,
        _ => ToolKind::Other,
    }
}

impl EventSink for AcpSink {
    fn emit(&self, event: AgentEvent) {
        let item = match event {
            AgentEvent::TextDelta { text } => Outbound::Update(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(text.into()),
            )),
            AgentEvent::ReasoningDelta { text } => Outbound::Update(
                SessionUpdate::AgentThoughtChunk(ContentChunk::new(text.into())),
            ),
            AgentEvent::ToolStarted { call } => {
                Outbound::Update(SessionUpdate::ToolCall(self.tool_call(&call)))
            }
            AgentEvent::ToolFinished { result } => {
                let status = if result.status == ToolStatus::Ok {
                    acp::ToolCallStatus::Completed
                } else {
                    acp::ToolCallStatus::Failed
                };
                Outbound::Update(SessionUpdate::ToolCallUpdate(acp::ToolCallUpdate::new(
                    result.call_id,
                    ToolCallUpdateFields::new()
                        .status(status)
                        .content(vec![result.content.into()]),
                )))
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
