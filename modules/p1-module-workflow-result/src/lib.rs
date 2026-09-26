//! `workflow_result` as a guest component (`p1/workflow-result`): a run's retained report, or
//! a wait for it that the call's cancellation ends — the native `WorkflowResultTool` of
//! `crates/p1-tool-workflow` over the `tool` world.
//!
//! Its capabilities are `control` and `workflows`. `workflows` is one interface (decision
//! S0-R1.3), so this component could link `start` and `cancel` too; it calls only `status` and
//! `wait`, and refusing the operations a member does not own is the host link's part.
#![forbid(unsafe_code)]

#[path = "../../p1-module-workflow-start/src/workflow.rs"]
mod workflow;

use p1_bindings_tool::generated::p1::module::{control, workflows};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use workflow::{RESULT_DESCRIPTION, RESULT_NAME, ResultInput, RunStatus};

struct WorkflowResult;

impl Guest for WorkflowResult {
    fn declaration() -> ToolDeclaration {
        workflow::declaration(RESULT_NAME, RESULT_DESCRIPTION, workflow::result_schema())
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the run this call reads the result of.
    fn describe(call: ToolCall) -> CallDescription {
        workflow::call_description(
            workflow::parse_input::<ResultInput>(RESULT_NAME, &call)
                .ok()
                .map(|input| input.id),
        )
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        workflow::text_result(&tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: ResultInput = match workflow::parse_input(RESULT_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // A call cancelled before it reaches the host reads nothing (the world's rule: return
        // promptly with the `cancelled` status).
        if control::cancelled() {
            return workflow::cancelled_outcome();
        }
        // The call's own cancellation is the wait's cancel: the host returns the running
        // status when it fires first.
        let text = if input.wait {
            workflows::wait(&input.id)
        } else {
            workflows::status(&input.id)
        };
        let text = match text {
            Ok(text) => text,
            Err(error) => return workflow::id_error(&input.id, error),
        };
        match workflow::parse_status(&text) {
            Ok(RunStatus::Running(_)) if input.wait => workflow::cancelled_outcome(),
            Ok(RunStatus::Running(progress)) => {
                workflow::ok_outcome(&workflow::render_progress(&input.id, &progress))
            }
            Ok(RunStatus::Ended(report)) => workflow::ok_outcome(&workflow::render_report(&report)),
            Err(outcome) => outcome,
        }
    }
}

p1_bindings_tool::generated::export!(WorkflowResult);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn describes_the_run_it_reads() {
        let call = json!({"call_id": "c1", "name": "workflow_result",
            "input": {"kind": "json", "raw": r#"{"id":"wf4","wait":true}"#}})
        .to_string();
        assert_eq!(
            WorkflowResult::describe(call),
            r#"{"destructive":false,"target":"wf4","verb":"workflow"}"#
        );
    }
}
