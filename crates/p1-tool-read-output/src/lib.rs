//! The `read_output` tool: pages an output the host stored (ADR-0109) by a zero-based UTF-8
//! byte cursor.
//!
//! The tool is split along the WebAssembly boundary (ADR-0071). What it decides — input,
//! declaration, descriptions, the page footer and the text of every store error — is
//! `p1-read-output-guest`, which the `p1/read-output` component also runs. The store is the
//! host's (`p1_module_runtime::outputs`): [`ReadOutputTool`] is the native adapter that reads it
//! through the same [`ToolOutputsService`] the component's `tool-outputs` import is linked to,
//! so the tests of this crate prove what the component ships.
//!
//! Reading an output runs no command: the tool is read-only and records no command evidence,
//! so a page never counts as a verification run for `finish` (ADR-0109 item 8).

use std::sync::Arc;

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_module_runtime::ToolOutputsService;
use p1_module_runtime::outputs::{Capture, OutputError, OutputInfo, OutputPage};
use p1_read_output_guest::{self as guest, NAME, Outputs, RawInput, Status};

pub use p1_read_output_guest::page_text;

/// The `read_output` tool over one store view.
pub struct ReadOutputTool {
    outputs: Arc<dyn ToolOutputsService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl ReadOutputTool {
    /// The tool over `outputs`, the host's view of the run's store.
    pub fn new(outputs: Arc<dyn ToolOutputsService>) -> Self {
        Self {
            outputs,
            declaration: ToolDeclaration {
                name: NAME.to_owned(),
                description: guest::DESCRIPTION.to_owned(),
                kind: DeclarationKind::Function {
                    input_schema: guest::input_schema(),
                },
            },
            identity: ToolIdentity {
                implementation: env!("CARGO_PKG_NAME").to_owned(),
                variant: "claude".to_owned(),
            },
        }
    }
}

fn raw_input(call: &ToolCall) -> RawInput<'_> {
    match &call.input {
        ToolInput::Json(raw) => RawInput::Json(raw),
        ToolInput::Text(raw) => RawInput::Text(raw),
    }
}

impl Tool for ReadOutputTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: guest::VERB,
            target: guest::describe_target(&self.declaration.name, raw_input(call)),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(&self, _call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        ResultDescription {
            summary: guest::describe_result(&result.content, result.status == ToolStatus::Ok),
            detail: None,
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let outcome = guest::execute(
                &self.declaration.name,
                raw_input(call),
                &Service(self.outputs.as_ref()),
            );
            ToolOutcome {
                status: match outcome.status {
                    Status::Ok => ToolStatus::Ok,
                    Status::Error => ToolStatus::Error,
                },
                content: outcome.content,
            }
        })
    }
}

/// The host's service as the guest reads it.
struct Service<'a>(&'a dyn ToolOutputsService);

impl Outputs for Service<'_> {
    fn describe(&self, handle: &str) -> Result<guest::OutputInfo, guest::OutputError> {
        self.0.describe(handle).map(info).map_err(error)
    }

    fn page(
        &self,
        handle: &str,
        offset: u64,
        limit: u32,
    ) -> Result<guest::OutputPage, guest::OutputError> {
        self.0.page(handle, offset, limit).map(page).map_err(error)
    }
}

fn info(info: OutputInfo) -> guest::OutputInfo {
    guest::OutputInfo {
        handle: info.handle,
        stored_bytes: info.stored_bytes,
        capture: match info.capture {
            Capture::Complete => guest::Capture::Complete,
            Capture::StoredCapReached => guest::Capture::StoredCapReached,
            Capture::StorageIncomplete => guest::Capture::StorageIncomplete,
            Capture::StorageFailed => guest::Capture::StorageFailed,
        },
    }
}

fn page(page: OutputPage) -> guest::OutputPage {
    guest::OutputPage {
        text: page.text,
        next_offset: page.next_offset,
        at_end: page.at_end,
    }
}

fn error(error: OutputError) -> guest::OutputError {
    match error {
        OutputError::UnknownOutput => guest::OutputError::UnknownOutput,
        OutputError::LimitTooSmall => guest::OutputError::LimitTooSmall,
        OutputError::OffsetPastEnd(stored) => guest::OutputError::OffsetPastEnd(stored),
        OutputError::OffsetInsideCharacter => guest::OutputError::OffsetInsideCharacter,
        OutputError::ReadFailed(reason) => guest::OutputError::ReadFailed(reason),
    }
}
