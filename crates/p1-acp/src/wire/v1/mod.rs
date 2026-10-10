//! ACP v1 subset. Shapes adapted from agentclientprotocol/agent-client-protocol
//! schema/v1/schema.json (Apache-2.0). Only this module knows wire field names.

use crate::{
    capabilities::Capabilities,
    plan::PlanStatus,
    policy::{PermissionPrompt, PermissionReply},
    sink::{ToolCategory, ToolDisplay, Update},
    turn::{TurnError, TurnStop},
    workflow_card::CardStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

pub(crate) mod config;

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum Content<'a> {
    Text { text: &'a str },
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ToolCall<'a> {
    tool_call_id: &'a str,
    title: &'a str,
    name: &'a str,
    kind: &'static str,
    status: &'static str,
    raw_input: &'a Value,
}

impl<'a> ToolCall<'a> {
    fn new(tool: &'a ToolDisplay, status: &'static str) -> Self {
        let kind = match tool.category {
            ToolCategory::Read => "read",
            ToolCategory::Edit => "edit",
            ToolCategory::Delete => "delete",
            ToolCategory::Move => "move",
            ToolCategory::Search => "search",
            ToolCategory::Execute => "execute",
            ToolCategory::Think => "think",
            ToolCategory::Fetch => "fetch",
            ToolCategory::Other => "other",
        };
        Self {
            tool_call_id: &tool.id,
            title: &tool.title,
            name: &tool.name,
            kind,
            status,
            raw_input: &tool.input,
        }
    }
}

#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ToolContent<'a> {
    Content { content: Content<'a> },
}

#[derive(Serialize)]
struct Cost {
    amount: f64,
    currency: &'static str,
}

#[derive(Serialize)]
struct PlanEntry<'a> {
    content: &'a str,
    priority: &'static str,
    status: &'static str,
}

#[derive(Serialize)]
#[serde(tag = "sessionUpdate", rename_all = "snake_case")]
enum SessionUpdate<'a> {
    AgentMessageChunk {
        content: Content<'a>,
    },
    AgentThoughtChunk {
        content: Content<'a>,
    },
    UsageUpdate {
        used: u64,
        size: u64,
        #[serde(skip_serializing_if = "Option::is_none")]
        cost: Option<Cost>,
    },
    Plan {
        entries: Vec<PlanEntry<'a>>,
    },
    ToolCall {
        #[serde(flatten)]
        call: ToolCall<'a>,
    },
    ToolCallUpdate {
        #[serde(rename = "toolCallId")]
        id: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        status: Option<&'static str>,
        #[serde(skip_serializing_if = "Option::is_none")]
        content: Option<[ToolContent<'a>; 1]>,
    },
}

pub(crate) fn update(update: &Update) -> Value {
    let wire = match update {
        Update::Message(text) => SessionUpdate::AgentMessageChunk {
            content: Content::Text { text },
        },
        Update::Thought(text) => SessionUpdate::AgentThoughtChunk {
            content: Content::Text { text },
        },
        Update::Usage(usage) => SessionUpdate::UsageUpdate {
            used: usage.used_tokens,
            size: usage.window_tokens,
            cost: usage.cost_micro_usd.map(|cost| Cost {
                amount: cost as f64 / 1_000_000.0,
                currency: "USD",
            }),
        },
        Update::Plan(entries) => SessionUpdate::Plan {
            entries: entries
                .iter()
                .map(|entry| PlanEntry {
                    content: &entry.content,
                    // Workflow steps have no relative priority: all are equal.
                    priority: "medium",
                    status: match entry.status {
                        PlanStatus::Pending => "pending",
                        PlanStatus::Active => "in_progress",
                        PlanStatus::Done => "completed",
                    },
                })
                .collect(),
        },
        Update::ToolStarted(tool) => SessionUpdate::ToolCall {
            call: ToolCall::new(tool, "in_progress"),
        },
        Update::ToolPending(tool) => SessionUpdate::ToolCall {
            call: ToolCall::new(tool, "pending"),
        },
        Update::ToolRunning { id } => SessionUpdate::ToolCallUpdate {
            id,
            status: Some("in_progress"),
            content: None,
        },
        Update::ToolFinished {
            id,
            succeeded,
            text,
        } => SessionUpdate::ToolCallUpdate {
            id,
            status: Some(if *succeeded { "completed" } else { "failed" }),
            content: Some([ToolContent::Content {
                content: Content::Text { text },
            }]),
        },
        Update::ToolProgress { id, text, status } => SessionUpdate::ToolCallUpdate {
            id,
            status: status.map(|status| match status {
                CardStatus::Running => "in_progress",
                CardStatus::Completed => "completed",
                CardStatus::Failed => "failed",
            }),
            content: Some([ToolContent::Content {
                content: Content::Text { text },
            }]),
        },
    };
    serde_json::to_value(wire).expect("wire serialization is infallible")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptResponse {
    stop_reason: &'static str,
}

pub(crate) fn stop(stop: TurnStop) -> Value {
    serde_json::to_value(PromptResponse {
        stop_reason: match stop {
            TurnStop::Finished => "end_turn",
            TurnStop::OutputLimit => "max_tokens",
            TurnStop::Refused => "refusal",
            TurnStop::Cancelled => "cancelled",
        },
    })
    .expect("wire serialization is infallible")
}

#[derive(Serialize)]
struct Error<'a> {
    code: i32,
    message: &'a str,
}

pub(crate) fn error(error: &TurnError) -> Value {
    serde_json::to_value(Error {
        code: -32603,
        message: &error.message,
    })
    .expect("wire serialization is infallible")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct InitializeResponse {
    protocol_version: u16,
    agent_capabilities: AgentCapabilities,
    auth_methods: [(); 0],
    agent_info: AgentInfo,
}

#[derive(Serialize)]
struct AgentInfo {
    name: &'static str,
    version: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentCapabilities {
    load_session: bool,
    prompt_capabilities: PromptCapabilities,
    #[serde(skip_serializing_if = "Option::is_none")]
    session_capabilities: Option<Value>,
    #[serde(rename = "_meta", skip_serializing_if = "Option::is_none")]
    meta: Option<Value>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PromptCapabilities {
    image: bool,
    audio: bool,
    embedded_context: bool,
}

pub(crate) fn capabilities(capabilities: &Capabilities) -> Value {
    serde_json::to_value(InitializeResponse {
        protocol_version: 1,
        auth_methods: [],
        agent_info: AgentInfo {
            name: "p1",
            version: env!("CARGO_PKG_VERSION"),
        },
        agent_capabilities: AgentCapabilities {
            load_session: false,
            prompt_capabilities: PromptCapabilities {
                image: false,
                audio: false,
                embedded_context: false,
            },
            session_capabilities: capabilities.close_sessions.then(|| json!({"close": {}})),
            meta: capabilities
                .p1_extensions
                .then(|| json!({"p1.dev":{"version":1,"extensions":[]}})),
        },
    })
    .expect("wire serialization is infallible")
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct PermissionOption {
    option_id: &'static str,
    name: &'static str,
    kind: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RequestPermission<'a> {
    session_id: &'a str,
    tool_call: ToolCall<'a>,
    options: [PermissionOption; 3],
}

pub(crate) fn permission<'a>(session: &'a str, prompt: &'a PermissionPrompt) -> Value {
    serde_json::to_value(RequestPermission {
        session_id: session,
        tool_call: ToolCall::new(&prompt.tool, "pending"),
        options: [
            PermissionOption {
                option_id: "allow_once",
                name: "Allow once",
                kind: "allow_once",
            },
            PermissionOption {
                option_id: "allow_always",
                name: "Always allow",
                kind: "allow_always",
            },
            PermissionOption {
                option_id: "reject_once",
                name: "Reject",
                kind: "reject_once",
            },
        ],
    })
    .expect("wire serialization is infallible")
}

#[derive(Deserialize)]
struct PermissionResponse {
    outcome: PermissionOutcome,
}

#[derive(Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
enum PermissionOutcome {
    Cancelled,
    Selected {
        #[serde(rename = "optionId")]
        id: String,
    },
}

pub(crate) fn decode_permission(reply: Value) -> Result<PermissionReply, serde_json::Error> {
    Ok(
        match serde_json::from_value::<PermissionResponse>(reply)?.outcome {
            PermissionOutcome::Cancelled => PermissionReply::Cancelled,
            PermissionOutcome::Selected { id } => match id.as_str() {
                "allow_once" => PermissionReply::AllowOnce,
                "allow_always" => PermissionReply::AllowAlways,
                _ => PermissionReply::Reject,
            },
        },
    )
}
