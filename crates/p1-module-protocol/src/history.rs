//! Wire form of the model-visible history (`schema/history-item.json`, `tool-call.json`,
//! `replay-data.json`).

use p1_contracts::{
    AssistantBlock, AssistantItem, AttachmentId, FileRef, ImageData, ImageRef, ImageType,
    InboxKind, Item, Origin, ReplayData, ToolCall, ToolInput, ToolResultItem, ToolStatus,
    UserBlock,
};
use serde::{Deserialize, Serialize};

/// The model and route that produced a value; replay data is only valid for its origin.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireOrigin {
    /// Route identifier.
    pub route: String,
    /// Model identifier as the provider reported it.
    pub model: String,
}

impl From<Origin> for WireOrigin {
    fn from(origin: Origin) -> Self {
        Self {
            route: origin.route,
            model: origin.model,
        }
    }
}

impl From<WireOrigin> for Origin {
    fn from(origin: WireOrigin) -> Self {
        Self {
            route: origin.route,
            model: origin.model,
        }
    }
}

/// Provider-native continuation data. `payload` is carried verbatim and is deliberately
/// schema-free: only the adapter that wrote it may interpret it, and `version` lets that
/// adapter refuse a layout it no longer reads.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireReplayData {
    /// Where the payload came from.
    pub origin: WireOrigin,
    /// The adapter's payload layout version.
    pub version: u32,
    /// Opaque to everyone but the originating adapter.
    pub payload: serde_json::Value,
}

impl From<ReplayData> for WireReplayData {
    fn from(replay: ReplayData) -> Self {
        Self {
            origin: replay.origin.into(),
            version: replay.version,
            payload: replay.payload,
        }
    }
}

impl From<WireReplayData> for ReplayData {
    fn from(replay: WireReplayData) -> Self {
        Self {
            origin: replay.origin.into(),
            version: replay.version,
            payload: replay.payload,
        }
    }
}

/// Raw tool input as the model produced it; kept raw so the tool, not the boundary, is
/// the one that validates it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    tag = "kind",
    content = "raw",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WireToolInput {
    /// Arguments of a function tool as raw JSON text, possibly invalid.
    Json(String),
    /// Input of a freeform tool.
    Text(String),
}

impl From<ToolInput> for WireToolInput {
    fn from(input: ToolInput) -> Self {
        match input {
            ToolInput::Json(raw) => Self::Json(raw),
            ToolInput::Text(raw) => Self::Text(raw),
        }
    }
}

impl From<WireToolInput> for ToolInput {
    fn from(input: WireToolInput) -> Self {
        match input {
            WireToolInput::Json(raw) => Self::Json(raw),
            WireToolInput::Text(raw) => Self::Text(raw),
        }
    }
}

/// A complete tool call.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolCall {
    /// Pairs the call with its result.
    pub call_id: String,
    /// Model-facing tool name.
    pub name: String,
    /// The raw input.
    pub input: WireToolInput,
}

impl From<ToolCall> for WireToolCall {
    fn from(call: ToolCall) -> Self {
        Self {
            call_id: call.call_id,
            name: call.name,
            input: call.input.into(),
        }
    }
}

impl From<WireToolCall> for ToolCall {
    fn from(call: WireToolCall) -> Self {
        Self {
            call_id: call.call_id,
            name: call.name,
            input: call.input.into(),
        }
    }
}

/// One block of an assistant response, in the order the model produced it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireAssistantBlock {
    /// Visible text.
    Text {
        /// The text.
        text: String,
    },
    /// Reasoning as shown plus what the origin route needs to continue from it.
    Reasoning {
        /// Shown reasoning; may be empty.
        text: String,
        /// Absent when the route returned nothing to replay.
        #[serde(
            default,
            deserialize_with = "crate::refuse_null",
            skip_serializing_if = "Option::is_none"
        )]
        replay: Option<WireReplayData>,
    },
    /// A complete tool call.
    ToolCall {
        /// Pairs the call with its result.
        call_id: String,
        /// Model-facing tool name.
        name: String,
        /// The raw input.
        input: WireToolInput,
    },
}

impl From<AssistantBlock> for WireAssistantBlock {
    fn from(block: AssistantBlock) -> Self {
        match block {
            AssistantBlock::Text { text } => Self::Text { text },
            AssistantBlock::Reasoning { text, replay } => Self::Reasoning {
                text,
                replay: replay.map(Into::into),
            },
            AssistantBlock::ToolCall(call) => Self::ToolCall {
                call_id: call.call_id,
                name: call.name,
                input: call.input.into(),
            },
        }
    }
}

impl From<WireAssistantBlock> for AssistantBlock {
    fn from(block: WireAssistantBlock) -> Self {
        match block {
            WireAssistantBlock::Text { text } => Self::Text { text },
            WireAssistantBlock::Reasoning { text, replay } => Self::Reasoning {
                text,
                replay: replay.map(Into::into),
            },
            WireAssistantBlock::ToolCall {
                call_id,
                name,
                input,
            } => Self::ToolCall(ToolCall {
                call_id,
                name,
                input: input.into(),
            }),
        }
    }
}

/// One completed assistant response.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireAssistantItem {
    /// Who produced it.
    pub origin: WireOrigin,
    /// Its blocks in production order.
    pub blocks: Vec<WireAssistantBlock>,
}

impl From<AssistantItem> for WireAssistantItem {
    fn from(item: AssistantItem) -> Self {
        Self {
            origin: item.origin.into(),
            blocks: item.blocks.into_iter().map(Into::into).collect(),
        }
    }
}

impl From<WireAssistantItem> for AssistantItem {
    fn from(item: WireAssistantItem) -> Self {
        Self {
            origin: item.origin.into(),
            blocks: item.blocks.into_iter().map(Into::into).collect(),
        }
    }
}

/// The closed set of tool result states; a module cannot invent one the core has no
/// handling for.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireToolStatus {
    /// The tool succeeded.
    Ok,
    /// The tool failed or its input was invalid.
    Error,
    /// No such tool is assembled.
    Unavailable,
    /// Authorization refused the call.
    Denied,
    /// Cancelled while running; effects may be partial.
    Cancelled,
    /// Started before a crash with no recorded result.
    Unknown,
}

impl From<ToolStatus> for WireToolStatus {
    fn from(status: ToolStatus) -> Self {
        match status {
            ToolStatus::Ok => Self::Ok,
            ToolStatus::Error => Self::Error,
            ToolStatus::Unavailable => Self::Unavailable,
            ToolStatus::Denied => Self::Denied,
            ToolStatus::Cancelled => Self::Cancelled,
            ToolStatus::Unknown => Self::Unknown,
        }
    }
}

impl From<WireToolStatus> for ToolStatus {
    fn from(status: WireToolStatus) -> Self {
        match status {
            WireToolStatus::Ok => Self::Ok,
            WireToolStatus::Error => Self::Error,
            WireToolStatus::Unavailable => Self::Unavailable,
            WireToolStatus::Denied => Self::Denied,
            WireToolStatus::Cancelled => Self::Cancelled,
            WireToolStatus::Unknown => Self::Unknown,
        }
    }
}

/// Why a non-user message entered through the inbox.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireInboxKind {
    /// The user steering a running turn.
    Steering,
    /// A module reporting something the agent waits for.
    Notification,
}

impl From<InboxKind> for WireInboxKind {
    fn from(kind: InboxKind) -> Self {
        match kind {
            InboxKind::Steering => Self::Steering,
            InboxKind::Notification => Self::Notification,
        }
    }
}

impl From<WireInboxKind> for InboxKind {
    fn from(kind: WireInboxKind) -> Self {
        match kind {
            WireInboxKind::Steering => Self::Steering,
            WireInboxKind::Notification => Self::Notification,
        }
    }
}

/// One history item; context policies read and write history in this form.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "item", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireItem {
    /// User input.
    User {
        /// The text.
        text: String,
    },
    /// User input that carries attachments (ADR-0180).
    UserContent {
        /// The blocks in order.
        blocks: Vec<WireUserBlock>,
    },
    /// Input delivered through the inbox at a safe boundary.
    Inbox {
        /// Why it arrived.
        kind: WireInboxKind,
        /// The text.
        text: String,
    },
    /// A completed assistant response.
    Assistant {
        /// Who produced it.
        origin: WireOrigin,
        /// Its blocks in production order.
        blocks: Vec<WireAssistantBlock>,
    },
    /// A tool result exactly as the model is shown it.
    ToolResult {
        /// The call it answers.
        call_id: String,
        /// Model-facing tool name.
        name: String,
        /// How the call ended.
        status: WireToolStatus,
        /// What the model sees.
        content: String,
    },
}

impl From<Item> for WireItem {
    fn from(item: Item) -> Self {
        match item {
            Item::User { text } => Self::User { text },
            Item::UserContent { blocks } => Self::UserContent {
                blocks: blocks.into_iter().map(WireUserBlock::from).collect(),
            },
            Item::Inbox { kind, text } => Self::Inbox {
                kind: kind.into(),
                text,
            },
            Item::Assistant(assistant) => {
                let WireAssistantItem { origin, blocks } = assistant.into();
                Self::Assistant { origin, blocks }
            }
            Item::ToolResult(result) => Self::ToolResult {
                call_id: result.call_id,
                name: result.name,
                status: result.status.into(),
                content: result.content,
            },
        }
    }
}

impl From<WireItem> for Item {
    fn from(item: WireItem) -> Self {
        match item {
            WireItem::User { text } => Self::User { text },
            WireItem::UserContent { blocks } => Self::UserContent {
                blocks: blocks.into_iter().map(UserBlock::from).collect(),
            },
            WireItem::Inbox { kind, text } => Self::Inbox {
                kind: kind.into(),
                text,
            },
            WireItem::Assistant { origin, blocks } => {
                Self::Assistant(WireAssistantItem { origin, blocks }.into())
            }
            WireItem::ToolResult {
                call_id,
                name,
                status,
                content,
            } => Self::ToolResult(ToolResultItem {
                call_id,
                name,
                status: status.into(),
                content,
            }),
        }
    }
}

/// An image format, named by content.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireImageType {
    /// PNG.
    Png,
    /// JPEG.
    Jpeg,
    /// GIF.
    Gif,
    /// WebP.
    Webp,
}

impl From<ImageType> for WireImageType {
    fn from(kind: ImageType) -> Self {
        match kind {
            ImageType::Png => Self::Png,
            ImageType::Jpeg => Self::Jpeg,
            ImageType::Gif => Self::Gif,
            ImageType::Webp => Self::Webp,
        }
    }
}

impl From<WireImageType> for ImageType {
    fn from(kind: WireImageType) -> Self {
        match kind {
            WireImageType::Png => Self::Png,
            WireImageType::Jpeg => Self::Jpeg,
            WireImageType::Gif => Self::Gif,
            WireImageType::Webp => Self::Webp,
        }
    }
}

/// A stored image's reference. The id is checked on deserialization, so a module can never
/// hand the host a path-shaped id.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireImageRef {
    /// Lowercase hex SHA-256 of the stored bytes.
    pub id: AttachmentId,
    /// Its format.
    pub media_type: WireImageType,
    /// Stored size in bytes.
    pub bytes: u64,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

/// A stored file's reference.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireFileRef {
    /// Lowercase hex SHA-256 of the stored bytes.
    pub id: AttachmentId,
    /// The name the user gave it.
    pub name: String,
    /// Size in bytes.
    pub bytes: u64,
}

/// One block of attachment-carrying user input. `data` and `path` are what the host resolved
/// for one provider request; the contract type never serializes them, so they reach a
/// module but never a journal.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "block", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireUserBlock {
    /// Text.
    Text {
        /// The text.
        text: String,
    },
    /// An image.
    Image {
        /// Its reference.
        image: WireImageRef,
        /// Its bytes as standard padded base64, when the host resolved them.
        #[serde(
            default,
            deserialize_with = "crate::refuse_null",
            skip_serializing_if = "Option::is_none"
        )]
        data: Option<String>,
    },
    /// A file.
    File {
        /// Its reference.
        file: WireFileRef,
        /// The read-only path the host resolved for it.
        #[serde(
            default,
            deserialize_with = "crate::refuse_null",
            skip_serializing_if = "Option::is_none"
        )]
        path: Option<String>,
    },
}

impl From<UserBlock> for WireUserBlock {
    fn from(block: UserBlock) -> Self {
        match block {
            UserBlock::Text { text } => Self::Text { text },
            UserBlock::Image { image, data } => Self::Image {
                image: WireImageRef {
                    id: image.id,
                    media_type: image.media_type.into(),
                    bytes: image.bytes,
                    width: image.width,
                    height: image.height,
                },
                data: data.map(|data| data.as_base64().to_owned()),
            },
            UserBlock::File { file, path } => Self::File {
                file: WireFileRef {
                    id: file.id,
                    name: file.name,
                    bytes: file.bytes,
                },
                path,
            },
        }
    }
}

impl From<WireUserBlock> for UserBlock {
    fn from(block: WireUserBlock) -> Self {
        match block {
            WireUserBlock::Text { text } => Self::Text { text },
            WireUserBlock::Image { image, data } => Self::Image {
                image: ImageRef {
                    id: image.id,
                    media_type: image.media_type.into(),
                    bytes: image.bytes,
                    width: image.width,
                    height: image.height,
                },
                data: data.map(ImageData::from_base64),
            },
            WireUserBlock::File { file, path } => Self::File {
                file: FileRef {
                    id: file.id,
                    name: file.name,
                    bytes: file.bytes,
                },
                path,
            },
        }
    }
}
