//! `workflow_start` as a guest component (`p1/workflow-start`): starts a workflow script in
//! the background — the native `WorkflowStartTool` of `crates/p1-tool-workflow` over the
//! `tool` world.
//!
//! Its capabilities are `control` and `workflows`. `workflows` is one interface (decision
//! S0-R1.3), so this component could link `status`, `wait` and `cancel` too; it calls only
//! `start`, and refusing the operations a member does not own is the host link's part.
#![forbid(unsafe_code)]

mod workflow;

use p1_bindings_tool::generated::p1::module::workflows::StartRequest;
use p1_bindings_tool::generated::p1::module::{control, workflows};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use serde_json::Value;
use workflow::{START_DESCRIPTION, START_NAME, StartInput};

struct WorkflowStart;

impl Guest for WorkflowStart {
    fn declaration() -> ToolDeclaration {
        workflow::declaration(START_NAME, START_DESCRIPTION, workflow::start_schema())
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the resumed run's id, or the script's first line as its name.
    fn describe(call: ToolCall) -> CallDescription {
        workflow::call_description(
            workflow::parse_input::<StartInput>(START_NAME, &call)
                .ok()
                .and_then(|input| {
                    input.resume_from.or_else(|| {
                        input
                            .script
                            .lines()
                            .find(|line| !line.trim().is_empty())
                            .map(|line| line.trim().chars().take(80).collect())
                    })
                }),
        )
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        workflow::text_result(&tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: StartInput = match workflow::parse_input(START_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // A call cancelled before it reaches the host starts nothing (the world's rule: return
        // promptly with the `cancelled` status).
        if control::cancelled() {
            return workflow::cancelled_outcome();
        }
        // The role models, the workspace and the base the native request also carries are
        // the host's to fill in; the frozen `start-request` has no field for them.
        let request = StartRequest {
            script: input.script,
            args: Value::Object(input.args).to_string(),
            resume_from: input.resume_from.clone(),
        };
        match workflows::start(&request) {
            Ok(id) => {
                let resumed = input
                    .resume_from
                    .map(|old| format!(", resuming {old}"))
                    .unwrap_or_default();
                workflow::ok_outcome(&format!(
                    "Started workflow {id}{resumed}. You will be notified when it ends; do not poll."
                ))
            }
            Err(error) => workflow::start_error(error),
        }
    }
}

p1_bindings_tool::generated::export!(WorkflowStart);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(raw: &str) -> String {
        json!({"call_id": "c1", "name": "workflow_start", "input": {"kind": "json", "raw": raw}})
            .to_string()
    }

    #[test]
    fn describes_the_script_by_its_first_line_or_the_resumed_run() {
        assert_eq!(
            WorkflowStart::describe(call(r#"{"script":"\n  let x = 1;  \nmore"}"#)),
            r#"{"destructive":false,"target":"let x = 1;","verb":"workflow"}"#
        );
        assert_eq!(
            WorkflowStart::describe(call(r#"{"script":"x","resume_from":"wf3"}"#)),
            r#"{"destructive":false,"target":"wf3","verb":"workflow"}"#
        );
    }

    #[test]
    fn unknown_input_fields_are_the_native_serde_error() {
        let outcome = WorkflowStart::execute(call(r#"{"script":"x","model":"m"}"#));
        assert!(outcome.contains(
            "Invalid input for workflow_start: unknown field `model`, expected one of `script`, `args`, `resume_from`"
        ));
    }
}
