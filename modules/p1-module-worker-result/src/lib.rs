//! `worker_result` as a guest component (`p1/worker-result`): a worker's retained status and
//! final text, optionally waiting — the native `WorkerResultTool` of `crates/p1-tool-delegate`
//! over the `tool` world.
//!
//! Its only capabilities are `control` and `workers-observe`, so the built component cannot
//! start, continue or cancel a worker. The native constructor also points the service's
//! completion notification at this tool's name; a component has no service to tell, so that
//! stays with the host that assembles the member.
#![forbid(unsafe_code)]

#[path = "../../p1-module-worker-start/src/delegation.rs"]
mod delegation;

use delegation::{RESULT_DESCRIPTION, RESULT_NAME, ResultInput, ResultItem};
use p1_bindings_tool::generated::p1::module::worker_types::ChildStatus;
use p1_bindings_tool::generated::p1::module::{control, workers_observe};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};

struct WorkerResult;

impl Guest for WorkerResult {
    fn declaration() -> ToolDeclaration {
        delegation::declaration(RESULT_NAME, RESULT_DESCRIPTION, delegation::result_schema())
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the worker this call reads.
    fn describe(call: ToolCall) -> CallDescription {
        delegation::call_description(
            delegation::parse_input::<ResultInput>(RESULT_NAME, &call)
                .ok()
                .map(|input| input.id),
        )
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let result = ResultItem::parse(&tool_result);
        let mut summary = delegation::plain_summary(&result);
        let mut detail = None;
        if result.is_ok() {
            let lines = result.content.lines().count();
            let status = if result.content.contains(": running") {
                Some("running")
            } else if result.content.contains(": cancelled") {
                Some("cancelled")
            } else if result.content.contains(": failed") {
                Some("failed")
            } else {
                result.content.lines().find_map(|line| {
                    line.strip_prefix("finish: ")
                        .map(|rest| rest.split([' ', '—']).next().unwrap_or(rest))
                })
            };
            summary = status.map_or_else(
                || format!("{lines} lines"),
                |word| format!("{word} · {lines} lines"),
            );
            detail = result.content.split_once("\n---\n").map(|(report, _)| {
                format!(
                    "lines\t{}",
                    report.lines().take(8).collect::<Vec<_>>().join("\n")
                )
            });
        }
        delegation::result_description(&summary, detail)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: ResultInput = match delegation::parse_input(RESULT_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // A call cancelled before it reaches the host reads nothing (the world's rule: return
        // promptly with the `cancelled` status).
        if control::cancelled() {
            return delegation::cancelled_outcome();
        }
        let status = if input.wait {
            // The call's own cancellation is the wait's cancel: the host returns `running`
            // when it fires first.
            match workers_observe::wait(&input.id) {
                Ok(ChildStatus::Running) => return delegation::cancelled_outcome(),
                Ok(status) => status,
                Err(error) => return delegation::id_error(&input.id, error),
            }
        } else {
            match workers_observe::status(&input.id) {
                Ok(status) => status,
                Err(error) => return delegation::id_error(&input.id, error),
            }
        };
        delegation::ok_outcome(&delegation::render_status(&input.id, &status))
    }
}

p1_bindings_tool::generated::export!(WorkerResult);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn item(content: &str) -> String {
        json!({"item": "tool_result", "call_id": "c1", "name": "worker_result",
            "status": "ok", "content": content})
        .to_string()
    }

    #[test]
    fn declares_the_native_name_and_description() {
        let declaration = WorkerResult::declaration();
        assert_eq!(declaration.name, "worker_result");
        assert_eq!(declaration.description, RESULT_DESCRIPTION);
    }

    #[test]
    fn summarises_a_finished_worker_with_its_report() {
        let content =
            "tools: read\nfinish: done — commands passed: t\n---\nWorker w1: finished\n\nok";
        assert_eq!(
            WorkerResult::describe_result(String::new(), item(content)),
            json!({"summary": "done · 6 lines",
                "detail": {"kind": "text", "text": "lines\ttools: read\nfinish: done — commands passed: t"}})
            .to_string()
        );
        assert_eq!(
            WorkerResult::describe_result(String::new(), item("Worker w1: running")),
            json!({"summary": "running · 1 lines"}).to_string()
        );
    }
}
