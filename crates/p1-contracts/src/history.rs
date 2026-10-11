//! Model-visible history: one flat, ordered list of items.
//!
//! Flat because the Responses route is a flat item list and the Messages route can
//! be derived from it by coalescing adjacent same-role items (the adapter's job).
//! Block order inside an assistant item is preserved; nothing is flattened to a string.

use std::borrow::Cow;

use serde::{Deserialize, Serialize};

use crate::attachment::UserBlock;

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
    /// User input that carries attachments (ADR-0180): references in order, never bytes.
    /// Text-only input stays `User`, so its journal record is unchanged.
    UserContent {
        blocks: Vec<UserBlock>,
    },
    /// Delivered at a safe boundary; model-visible as user-role input.
    Inbox {
        kind: InboxKind,
        text: String,
    },
    Assistant(AssistantItem),
    ToolResult(ToolResultItem),
}

impl Item {
    /// This item as user input: `User` and `UserContent`, `None` for every other item, the
    /// inbox included. Every check that asks "is this the user, and what did they say" goes
    /// through it (#801), so none can skip input with attachments.
    pub fn user_input(&self) -> Option<UserInput<'_>> {
        match self {
            Item::User { text } => Some(UserInput::Text(text)),
            Item::UserContent { blocks } => Some(UserInput::Content(blocks)),
            Item::Inbox { .. } | Item::Assistant(_) | Item::ToolResult(_) => None,
        }
    }
}

/// User input, from whichever variant carries it: [`Item::user_input`] and
/// [`crate::RecordBody::user_input`]. A check reads [`UserInput::text`] or
/// [`UserInput::blocks`]; a match on the two variants is exhaustive, so the compiler flags
/// one that forgets a kind of input.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum UserInput<'a> {
    /// Text-only input: `Item::User`, `RecordBody::UserInput`.
    Text(&'a str),
    /// Input with attachments (ADR-0180): `Item::UserContent`, `RecordBody::UserContent`.
    Content(&'a [UserBlock]),
}

impl<'a> UserInput<'a> {
    /// What the user said: the text, or the text blocks joined by a newline (an image or a
    /// file says nothing here).
    pub fn text(self) -> Cow<'a, str> {
        match self {
            UserInput::Text(text) => Cow::Borrowed(text),
            UserInput::Content(blocks) => {
                let texts: Vec<&str> = blocks
                    .iter()
                    .filter_map(|block| match block {
                        UserBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                Cow::Owned(texts.join("\n"))
            }
        }
    }

    /// The input in order, as blocks: text-only input is one text block.
    pub fn blocks(self) -> Cow<'a, [UserBlock]> {
        match self {
            UserInput::Text(text) => Cow::Owned(vec![UserBlock::text(text)]),
            UserInput::Content(blocks) => Cow::Borrowed(blocks),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::attachment::{AttachmentId, ImageRef, ImageType};

    #[test]
    fn user_input_is_the_text_of_user_and_user_content_only() {
        let image = UserBlock::image(ImageRef {
            id: AttachmentId::new("ab".repeat(32)).unwrap(),
            media_type: ImageType::Png,
            bytes: 3,
            width: 1,
            height: 1,
        });
        let blocks = vec![
            UserBlock::text("what is"),
            image.clone(),
            UserBlock::text("this?"),
        ];
        let content = Item::UserContent {
            blocks: blocks.clone(),
        };
        let input = content.user_input().unwrap();
        assert_eq!(input.text(), "what is\nthis?");
        assert_eq!(input.blocks().as_ref(), blocks.as_slice());
        let image_only = Item::UserContent {
            blocks: vec![image],
        };
        assert_eq!(image_only.user_input().unwrap().text(), "");
        let user = Item::User { text: "hi".into() };
        let input = user.user_input().unwrap();
        assert_eq!(input.text(), "hi");
        assert_eq!(input.blocks().as_ref(), [UserBlock::text("hi")].as_slice());
        let inbox = Item::Inbox {
            kind: InboxKind::Steering,
            text: "steer".into(),
        };
        assert_eq!(inbox.user_input(), None);
    }
}
