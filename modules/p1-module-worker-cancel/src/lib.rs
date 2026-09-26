//! `worker_cancel` as a guest component (`p1/worker-cancel`): cancels a worker's current turn
//! and keeps its session — the native `WorkerCancelTool` of `crates/p1-tool-delegate` over
//! the `tool` world. Its only capabilities are `control` and `workers-control`.
#![forbid(unsafe_code)]

#[path = "../../p1-module-worker-start/src/delegation.rs"]
mod delegation;

use delegation::{CANCEL_DESCRIPTION, CANCEL_NAME, CancelInput, ResultItem};
use p1_bindings_tool::generated::p1::module::{control, workers_control};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};

struct WorkerCancel;

impl Guest for WorkerCancel {
    fn declaration() -> ToolDeclaration {
        delegation::declaration(CANCEL_NAME, CANCEL_DESCRIPTION, delegation::cancel_schema())
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the worker this call cancels.
    fn describe(call: ToolCall) -> CallDescription {
        delegation::call_description(
            delegation::parse_input::<CancelInput>(CANCEL_NAME, &call)
                .ok()
                .map(|input| input.id),
        )
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let result = ResultItem::parse(&tool_result);
        let summary = if result.is_ok() {
            "cancelled".to_owned()
        } else {
            delegation::plain_summary(&result)
        };
        delegation::result_description(&summary, None)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: CancelInput = match delegation::parse_input(CANCEL_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // A call cancelled before it reaches the host cancels nothing (the world's rule:
        // return promptly with the `cancelled` status).
        if control::cancelled() {
            return delegation::cancelled_outcome();
        }
        match workers_control::cancel(&input.id) {
            Ok(()) => delegation::ok_outcome(&format!("Worker {} cancelled.", input.id)),
            Err(error) => delegation::id_error(&input.id, error),
        }
    }
}

p1_bindings_tool::generated::export!(WorkerCancel);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn describes_the_worker_it_cancels() {
        let call = json!({"call_id": "c1", "name": "worker_cancel",
            "input": {"kind": "json", "raw": r#"{"id":"w3"}"#}})
        .to_string();
        assert_eq!(
            WorkerCancel::describe(call),
            r#"{"destructive":false,"target":"w3","verb":"worker"}"#
        );
        let error = json!({"item": "tool_result", "call_id": "c1", "name": "worker_cancel",
            "status": "error", "content": "No worker w3.\nmore"})
        .to_string();
        assert_eq!(
            WorkerCancel::describe_result(String::new(), error),
            r#"{"summary":"No worker w3."}"#
        );
    }
}
