//! Display the durable transcript, not the compacted model projection. Never
//! dispatch a recorded tool, or expose environment prompts/provider replay data.

use crate::sink::{ToolCategory, ToolDisplay, Update};
use p1_contracts::frontend::SessionHandle;
use p1_contracts::{AssistantBlock, ImageData, JournalRecord, RecordBody, ToolStatus, UserBlock};

/// A recorded image goes back as the image the client sent, read from the session's
/// store; one whose bytes cannot be read is its placeholder text, naming why (#694).
pub(crate) async fn updates(records: &[JournalRecord], session: &dyn SessionHandle) -> Vec<Update> {
    let mut updates = Vec::new();
    for record in records {
        match &record.body {
            RecordBody::UserInput { text } => updates.push(Update::UserMessage(text.clone())),
            RecordBody::UserContent { blocks } => {
                for block in blocks {
                    updates.push(match block {
                        UserBlock::Image { image, .. } => match session.read_image(image).await {
                            Ok(bytes) => Update::UserImage {
                                media_type: image.media_type,
                                data: ImageData::encode(&bytes),
                            },
                            Err(reason) => Update::UserMessage(image.unavailable(&reason)),
                        },
                        other => Update::UserMessage(other.as_text().into_owned()),
                    });
                }
            }
            RecordBody::AssistantCompleted { item, .. } => {
                for block in &item.blocks {
                    match block {
                        AssistantBlock::Text { text } => {
                            updates.push(Update::Message(text.clone()))
                        }
                        AssistantBlock::Reasoning { text, .. } if !text.is_empty() => {
                            updates.push(Update::Thought(text.clone()));
                        }
                        AssistantBlock::ToolCall(call) => {
                            updates.push(Update::ToolStarted(ToolDisplay {
                                id: call.call_id.clone(),
                                title: call.name.clone(),
                                name: call.name.clone(),
                                category: ToolCategory::Other,
                                input: serde_json::from_str(call.input.raw()).unwrap_or_else(
                                    |_| serde_json::json!({"raw":call.input.raw()}),
                                ),
                            }))
                        }
                        _ => {}
                    }
                }
            }
            RecordBody::ToolFinished { result, .. } => updates.push(Update::ToolFinished {
                id: result.call_id.clone(),
                succeeded: result.status == ToolStatus::Ok,
                text: result.content.clone(),
            }),
            RecordBody::AssistantInterrupted { partial_text, .. } if !partial_text.is_empty() => {
                updates.push(Update::Message(partial_text.clone()));
            }
            _ => {}
        }
    }
    updates
}
