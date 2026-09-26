//! `workflow_cancel` as a guest component (`p1/workflow-cancel`): cancels a run and its
//! in-flight steps — the native `WorkflowCancelTool` of `crates/p1-tool-workflow` over the
//! `tool` world.
//!
//! Its capabilities are `control` and `workflows`. `workflows` is one interface (decision
//! S0-R1.3), so this component could link `start`, `status` and `wait` too; it calls only
//! `cancel`, and refusing the operations a member does not own is the host link's part.
#![forbid(unsafe_code)]

#[path = "../../p1-module-workflow-start/src/workflow.rs"]
mod workflow;

use p1_bindings_tool::generated::p1::module::{control, workflows};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use workflow::{CANCEL_DESCRIPTION, CANCEL_NAME, IdInput};

struct WorkflowCancel;

impl Guest for WorkflowCancel {
    fn declaration() -> ToolDeclaration {
        workflow::declaration(CANCEL_NAME, CANCEL_DESCRIPTION, workflow::id_schema())
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the run this call cancels.
    fn describe(call: ToolCall) -> CallDescription {
        workflow::call_description(
            workflow::parse_input::<IdInput>(CANCEL_NAME, &call)
                .ok()
                .map(|input| input.id),
        )
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        workflow::text_result(&tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: IdInput = match workflow::parse_input(CANCEL_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // A call cancelled before it reaches the host cancels nothing (the world's rule:
        // return promptly with the `cancelled` status).
        if control::cancelled() {
            return workflow::cancelled_outcome();
        }
        match workflows::cancel(&input.id) {
            Ok(()) => workflow::ok_outcome(&format!("Workflow {} cancelled.", input.id)),
            Err(error) => workflow::id_error(&input.id, error),
        }
    }
}

p1_bindings_tool::generated::export!(WorkflowCancel);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn freeform_input_is_the_native_error() {
        let call = json!({"call_id": "c1", "name": "workflow_cancel",
            "input": {"kind": "text", "raw": "wf1"}})
        .to_string();
        assert_eq!(
            WorkflowCancel::execute(call),
            workflow::error_outcome(
                "Invalid input for workflow_cancel: expected a JSON object input, got freeform text"
            )
        );
    }
}
