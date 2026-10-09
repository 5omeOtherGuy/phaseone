//! The `read_output` tool as a component (`p1/read-output`, ADR-0109).
//!
//! Everything the tool decides is `p1-read-output-guest`, the same code the native adapter
//! (`ReadOutputTool` in `p1-tool-read-output`) runs; this crate binds it to the `tool` world and
//! reads the store through the host's `tool-outputs` capability. Which outputs exist, what a
//! handle names and where a page may start are the host's: this component only forwards the
//! handle, the offset and the limit, and shows the host's answer through the guest's texts.
#![forbid(unsafe_code)]

mod wire;

use p1_bindings_tool::generated::p1::module::tool_outputs;
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_read_output_guest::{self as guest, NAME};

struct ReadOutput;

impl Guest for ReadOutput {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: NAME.to_owned(),
            description: guest::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(guest::input_schema().to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }

    fn describe(call: ToolCall) -> CallDescription {
        // Restricted path: from the input alone. An unreadable call has no target.
        let target = serde_json::from_str::<wire::Call>(&call)
            .ok()
            .and_then(|call| guest::describe_target(NAME, call.input.raw()));
        wire::text(&wire::CallDescription {
            verb: guest::VERB,
            target,
            destructive: false,
            // ADR-0118 Decision 1: a read always overlaps other reads.
            shared: true,
        })
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let (content, ok) = match serde_json::from_str::<wire::ResultItem>(&tool_result) {
            Ok(item) => (item.content, item.status == "ok"),
            Err(_) => (String::new(), false),
        };
        wire::text(&wire::ResultDescription {
            summary: guest::describe_result(&content, ok),
        })
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let outcome = match serde_json::from_str::<wire::Call>(&call) {
            Ok(call) => guest::execute(NAME, call.input.raw(), &Store),
            Err(error) => guest::Outcome {
                status: guest::Status::Error,
                content: guest::invalid(NAME, &error.to_string()),
            },
        };
        wire::outcome(&outcome)
    }
}

p1_bindings_tool::generated::export!(ReadOutput);

/// The host's store through the `tool-outputs` import.
struct Store;

impl guest::Outputs for Store {
    fn describe(&self, handle: &str) -> Result<guest::OutputInfo, guest::OutputError> {
        tool_outputs::describe(handle).map(info).map_err(error)
    }

    fn page(
        &self,
        handle: &str,
        offset: u64,
        limit: u32,
    ) -> Result<guest::OutputPage, guest::OutputError> {
        tool_outputs::page(handle, offset, limit)
            .map(|page| guest::OutputPage {
                text: page.text,
                next_offset: page.next_offset,
                at_end: page.at_end,
            })
            .map_err(error)
    }
}

fn info(info: tool_outputs::OutputInfo) -> guest::OutputInfo {
    guest::OutputInfo {
        handle: info.handle,
        stored_bytes: info.stored_bytes,
        capture: match info.capture {
            tool_outputs::Capture::Complete => guest::Capture::Complete,
            tool_outputs::Capture::StoredCapReached => guest::Capture::StoredCapReached,
            tool_outputs::Capture::StorageIncomplete => guest::Capture::StorageIncomplete,
            tool_outputs::Capture::StorageFailed => guest::Capture::StorageFailed,
        },
    }
}

fn error(error: tool_outputs::OutputError) -> guest::OutputError {
    match error {
        tool_outputs::OutputError::UnknownOutput => guest::OutputError::UnknownOutput,
        tool_outputs::OutputError::LimitTooSmall => guest::OutputError::LimitTooSmall,
        tool_outputs::OutputError::OffsetPastEnd(stored) => {
            guest::OutputError::OffsetPastEnd(stored)
        }
        tool_outputs::OutputError::OffsetInsideCharacter => {
            guest::OutputError::OffsetInsideCharacter
        }
        tool_outputs::OutputError::ReadFailed(reason) => guest::OutputError::ReadFailed(reason),
    }
}
