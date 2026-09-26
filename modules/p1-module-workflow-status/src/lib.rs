//! `workflow_status` as a guest component (`p1/workflow-status`): a run's compact progress, or
//! the first line of its ended report, without waiting — the native `WorkflowStatusTool` of
//! `crates/p1-tool-workflow` over the `tool` world.
//!
//! Its capabilities are `control` and `workflows`. `workflows` is one interface (decision
//! S0-R1.3), so this component could link `start`, `wait` and `cancel` too; it calls only
//! `status`, and refusing the operations a member does not own is the host link's part.
#![forbid(unsafe_code)]

#[path = "../../p1-module-workflow-start/src/workflow.rs"]
mod workflow;

use p1_bindings_tool::generated::p1::module::{control, workflows};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use workflow::{IdInput, RunStatus, STATUS_DESCRIPTION, STATUS_NAME};

struct WorkflowStatus;

impl Guest for WorkflowStatus {
    fn declaration() -> ToolDeclaration {
        workflow::declaration(STATUS_NAME, STATUS_DESCRIPTION, workflow::id_schema())
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the run this call reads the status of.
    fn describe(call: ToolCall) -> CallDescription {
        workflow::call_description(
            workflow::parse_input::<IdInput>(STATUS_NAME, &call)
                .ok()
                .map(|input| input.id),
        )
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        workflow::text_result(&tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: IdInput = match workflow::parse_input(STATUS_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // A call cancelled before it reaches the host reads nothing (the world's rule: return
        // promptly with the `cancelled` status).
        if control::cancelled() {
            return workflow::cancelled_outcome();
        }
        let text = match workflows::status(&input.id) {
            Ok(text) => text,
            Err(error) => return workflow::id_error(&input.id, error),
        };
        match workflow::parse_status(&text) {
            Ok(RunStatus::Running(progress)) => {
                workflow::ok_outcome(&workflow::render_progress(&input.id, &progress))
            }
            Ok(RunStatus::Ended(report)) => workflow::ok_outcome(&workflow::report_line(&report)),
            Err(outcome) => outcome,
        }
    }
}

p1_bindings_tool::generated::export!(WorkflowStatus);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn declares_the_native_name_and_description() {
        let declaration = WorkflowStatus::declaration();
        assert_eq!(declaration.name, "workflow_status");
        assert_eq!(declaration.description, STATUS_DESCRIPTION);
    }
}
