//! Model-visible history: one flat, ordered list of items.
//!
//! Flat because the Responses route is a flat item list and the Messages route can
//! be derived from it by coalescing adjacent same-role items (the adapter's job).
//! Block order inside an assistant item is preserved; nothing is flattened to a string.

use serde::{Deserialize, Serialize};

/// The concrete model + route that produced something. Replay data is only valid
/// for the origin that produced it.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Origin {
    /// Route identifier, e.g. `anthropic-messages/claude-subscription`.
    pub route: String,
    /// Model identifier as reported by the provider for this response.
    pub model: String,
}

/// Opaque provider-native continuation data (thinking signature, encrypted
/// reasoning, …). The core and stores carry it verbatim and never interpret it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ReplayData {
    pub origin: Origin,
    /// Version of the adapter's payload layout, so an adapter can refuse old data.
    pub version: u32,
    pub payload: serde_json::Value,
}

/// Raw tool input exactly as the model produced it. It is validated at the tool
/// boundary, never parsed, repaired or defaulted on the way there.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "raw", rename_all = "snake_case")]
pub enum ToolInput {
    /// Raw JSON text of a function tool's arguments (may be invalid JSON).
    Json(String),
    /// Raw text of a freeform tool's input (e.g. a patch).
    Text(String),
}

impl ToolInput {
    pub fn raw(&self) -> &str {
        match self {
            Self::Json(raw) | Self::Text(raw) => raw,
        }
    }
}

/// A COMPLETE tool call. Providers never surface partial calls.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Provider-assigned call id; pairs the call with its result.
    pub call_id: String,
    /// Model-facing tool name.
    pub name: String,
    pub input: ToolInput,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case")]
pub enum AssistantBlock {
    Text {
        text: String,
    },
    /// Reasoning as shown (summary or visible thinking; may be empty) plus the opaque
    /// data the origin route needs to continue from it.
    Reasoning {
        text: String,
        replay: Option<ReplayData>,
    },
    ToolCall(ToolCall),
}

/// One completed assistant response, blocks in the order the model produced them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AssistantItem {
    pub origin: Origin,
    pub blocks: Vec<AssistantBlock>,
}

impl AssistantItem {
    pub fn tool_calls(&self) -> impl Iterator<Item = &ToolCall> {
        self.blocks.iter().filter_map(|block| match block {
            AssistantBlock::ToolCall(call) => Some(call),
            _ => None,
        })
    }

    /// All text blocks joined with a newline.
    pub fn text(&self) -> String {
        let parts: Vec<&str> = self
            .blocks
            .iter()
            .filter_map(|block| match block {
                AssistantBlock::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        parts.join("\n")
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    /// The tool ran and reported an error, or its input was invalid.
    Error,
    /// No tool with that name is assembled for this agent.
    Unavailable,
    /// The authorization policy refused the call; nothing was executed.
    Denied,
    /// Cancelled while running; side effects may be partial.
    Cancelled,
    /// Started before a crash and never recorded a result: the outcome is unknown
    /// and must be reconciled, never blindly re-run.
    Unknown,
}

/// A tool result. `content` is EXACTLY what the model is shown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultItem {
    pub call_id: String,
    pub name: String,
    pub status: ToolStatus,
    pub content: String,
}

/// Why a non-user message entered the conversation through the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InboxKind {
    /// The user steering a running turn.
    Steering,
    /// A module reporting something the agent waits for (e.g. a child finished).
    Notification,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case")]
pub enum Item {
    User {
        text: String,
    },
    /// Delivered at a safe boundary; model-visible as user-role input.
    Inbox {
        kind: InboxKind,
        text: String,
    },
    Assistant(AssistantItem),
    ToolResult(ToolResultItem),
}
