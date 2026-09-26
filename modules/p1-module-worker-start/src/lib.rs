//! `worker_start` as a guest component (`p1/worker-start`): starts one worker now and reports
//! it, the native `WorkerStartTool` of `crates/p1-tool-delegate` over the `tool` world.
//!
//! Its only capabilities are `control` and `workers-start`, so the built component cannot
//! observe or control a worker. Two parts of the native tool live in the host instead:
//!
//! - The `tools` and `environment` enums of the schema are the host's lists. The component
//!   declares them empty (see [`delegation::GRANTABLE`]) and leaves membership of a granted
//!   module to the host's `start`.
//! - The native success text names the worker's route and model through the service's
//!   `describe`, an operation of `workers-observe`. This member does not import it, so the
//!   text is the one the native tool writes when `describe` yields nothing.
#![forbid(unsafe_code)]

mod delegation;

use delegation::{
    ENVIRONMENTS, GRANTABLE, ResultItem, START_DESCRIPTION, START_NAME, STARTED_PREFIX, StartInput,
};
use p1_bindings_tool::generated::p1::module::worker_types::ChildSpec;
use p1_bindings_tool::generated::p1::module::{control, workers_start};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};

struct WorkerStart;

impl Guest for WorkerStart {
    fn declaration() -> ToolDeclaration {
        delegation::declaration(
            START_NAME,
            START_DESCRIPTION,
            delegation::start_schema(GRANTABLE, ENVIRONMENTS),
        )
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    /// ADR-0057: the environment this call starts a worker on.
    fn describe(call: ToolCall) -> CallDescription {
        delegation::call_description(
            delegation::parse_input::<StartInput>(START_NAME, &call)
                .ok()
                .map(|input| input.environment),
        )
    }

    fn describe_result(call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let result = ResultItem::parse(&tool_result);
        let mut summary = delegation::plain_summary(&result);
        let mut detail = None;
        if result.is_ok()
            && let Ok(input) = delegation::parse_input::<StartInput>(START_NAME, &call)
        {
            let mut grants = input.tools;
            grants.push("finish".into());
            summary = format!("started · {}", grants.join(", "));
            let target = result
                .content
                .strip_prefix(STARTED_PREFIX)
                .and_then(|rest| rest.split_once(" on "))
                .and_then(|(id, rest)| {
                    rest.split_once(" with tools")
                        .map(|(route, _)| format!("{id} · {route}"))
                });
            let mut lines = Vec::new();
            if let Some(target) = target {
                lines.push(format!("target\t{target}"));
            }
            lines.push(input.task.lines().next().unwrap_or_default().to_string());
            lines.push(format!("grants  {}", grants.join(" ")));
            detail = Some(lines.join("\n"));
        }
        delegation::result_description(&summary, detail)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let input: StartInput = match delegation::parse_input(START_NAME, &call) {
            Ok(input) => input,
            Err(outcome) => return outcome,
        };
        // Nothing is started until the grant is non-empty; which modules may be granted is
        // the host's check at `start`, where the grantable list lives.
        let mut tools = delegation::dedup(input.tools);
        if tools.is_empty() {
            return delegation::error_outcome(&format!(
                "`tools` is required: list every tool module the worker needs, from: {}",
                GRANTABLE.join(", ")
            ));
        }
        // A call cancelled before it reaches the host starts nothing (the world's rule: return
        // promptly with the `cancelled` status).
        if control::cancelled() {
            return delegation::cancelled_outcome();
        }
        let spec = ChildSpec {
            environment: input.environment,
            task: input.task,
            tools: tools.clone(),
        };
        match workers_start::start(&spec) {
            Ok(id) => {
                // The native tool asks `describe` for the route and model and writes nothing
                // when it fails; `describe` is not this member's to call, so it is that text.
                let description = String::new();
                // Every worker also gets `finish`, so the grant named here is the
                // grant plus it — the same list the worker's own prompt carries.
                tools.push("finish".to_string());
                delegation::ok_outcome(&format!(
                    "{STARTED_PREFIX}{id} on {description} with tools: {}. You will be notified \
                     when it finishes.",
                    tools.join(", ")
                ))
            }
            Err(error) => delegation::start_error(error),
        }
    }
}

p1_bindings_tool::generated::export!(WorkerStart);

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn call(raw: &str) -> String {
        json!({"call_id": "c1", "name": "worker_start", "input": {"kind": "json", "raw": raw}})
            .to_string()
    }

    #[test]
    fn declares_the_native_declaration_over_empty_lists() {
        let declaration = WorkerStart::declaration();
        assert_eq!(declaration.name, "worker_start");
        assert_eq!(declaration.description, START_DESCRIPTION);
    }

    #[test]
    fn an_empty_grant_is_refused_before_any_import() {
        assert_eq!(
            WorkerStart::execute(call(r#"{"environment":"e","task":"t","tools":[]}"#)),
            delegation::error_outcome(
                "`tools` is required: list every tool module the worker needs, from: "
            )
        );
    }

    #[test]
    fn describes_the_environment_and_the_started_result() {
        let call = call(r#"{"environment":"coder","task":"fix it\nmore","tools":["read"]}"#);
        assert_eq!(
            WorkerStart::describe(call.clone()),
            r#"{"destructive":false,"target":"coder","verb":"worker"}"#
        );
        let item = json!({"item": "tool_result", "call_id": "c1", "name": "worker_start",
            "status": "ok", "content": "Started worker w1 on r/m with tools: read, finish. You will be notified when it finishes."})
        .to_string();
        assert_eq!(
            WorkerStart::describe_result(call, item),
            json!({"summary": "started · read, finish",
                "detail": {"kind": "text", "text": "target\tw1 · r/m\nfix it\ngrants  read finish"}})
            .to_string()
        );
    }
}
