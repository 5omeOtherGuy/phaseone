//! `worker_continue` as a guest component (`p1/worker-continue`): another turn in the same
//! worker session, optionally with a larger tool grant (ADR-0050 item 6) — the native
//! `WorkerContinueTool` of `crates/p1-tool-delegate` over the `tool` world.
//!
//! Its only capabilities are `control` and `workers-control`. The `add_tools` enum of the
//! schema is the host's grantable list: the component declares it empty (see
//! [`delegation::GRANTABLE`]) and leaves membership of an added module to the host's
//! `continue-child`, which refuses it as a `regrant` error.
#![forbid(unsafe_code)]

#[path = "../../p1-module-worker-start/src/delegation.rs"]
mod delegation;

use delegation::{CONTINUE_DESCRIPTION, CONTINUE_NAME, ContinueInput, GRANTABLE, ResultItem};
use p1_bindings_tool::generated::p1::module::{control, workers_control};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};

struct WorkerContinue;

impl Guest for WorkerContinue {
    fn declaration() -> ToolDeclaration {
        delegation::declaration(
            CONTINUE_NAME,
            CONTINUE_DESCRIPTION,
            delegation::continue_schema(GRANTABLE),
        )
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the worker this call continues.
    fn describe(call: ToolCall) -> CallDescription {
        delegation::call_description(
            delegation::parse_input::<ContinueInput>(CONTINUE_NAME, &call)
                .ok()
                .map(|input| {
                    if input.add_tools.is_empty() {
                        input.id
                    } else {
                        format!("{} +{}", input.id, input.add_tools.join(" +"))
                    }
                }),
        )
    }

    fn describe_result(call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let result = ResultItem::parse(&tool_result);
        let mut summary = delegation::plain_summary(&result);
        if result.is_ok() {
            let grants = delegation::parse_input::<ContinueInput>(CONTINUE_NAME, &call)
                .map(|input| input.add_tools)
                .unwrap_or_default();
            summary = if grants.is_empty() {
                "resumed".into()
            } else {
                format!("resumed · +{}", grants.join(" +"))
            };
        }
        delegation::result_description(&summary, None)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: ContinueInput = match delegation::parse_input(CONTINUE_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // Which modules may be added is the host's check at `continue-child`, where the
        // grantable list lives; duplicates are removed here as the native tool removes them.
        let add_tools = delegation::dedup(input.add_tools);
        // A call cancelled before it reaches the host sends nothing (the world's rule: return
        // promptly with the `cancelled` status).
        if control::cancelled() {
            return delegation::cancelled_outcome();
        }
        match workers_control::continue_child(&input.id, &input.message, &add_tools) {
            Ok(()) if add_tools.is_empty() => {
                delegation::ok_outcome(&format!("Worker {} continues.", input.id))
            }
            Ok(()) => delegation::ok_outcome(&format!(
                "Added tools: {}. Message sent to worker {}.",
                add_tools.join(", "),
                input.id
            )),
            Err(error) => delegation::id_error(&input.id, error),
        }
    }
}

p1_bindings_tool::generated::export!(WorkerContinue);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(raw: &str) -> String {
        json!({"call_id": "c1", "name": "worker_continue", "input": {"kind": "json", "raw": raw}})
            .to_string()
    }

    #[test]
    fn describes_the_worker_and_the_added_tools() {
        assert_eq!(
            WorkerContinue::describe(call(r#"{"id":"w1","message":"m","add_tools":["a","b"]}"#)),
            r#"{"destructive":false,"target":"w1 +a +b","verb":"worker"}"#
        );
        let item = json!({"item": "tool_result", "call_id": "c1", "name": "worker_continue",
            "status": "ok", "content": "Worker w1 continues."})
        .to_string();
        assert_eq!(
            WorkerContinue::describe_result(call(r#"{"id":"w1","message":"m"}"#), item),
            r#"{"summary":"resumed"}"#
        );
    }
}
