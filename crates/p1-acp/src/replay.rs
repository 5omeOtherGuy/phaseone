//! Display the durable transcript, not the compacted model projection. Never
//! dispatch a recorded tool, or expose environment prompts/provider replay data.

use crate::sink::{ToolCategory, ToolDisplay, Update};
use p1_contracts::{AssistantBlock, JournalRecord, RecordBody, ToolStatus};

pub(crate) fn updates(records: &[JournalRecord]) -> Vec<Update> {
    let mut updates = Vec::new();
    for record in records {
        match &record.body {
            RecordBody::UserInput { text } => updates.push(Update::UserMessage(text.clone())),
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
