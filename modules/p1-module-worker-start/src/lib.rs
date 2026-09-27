//! `worker_start` as a guest component (`p1/worker-start`): starts one worker now and reports
//! it, the native `WorkerStartTool` of `crates/p1-tool-delegate` over the `tool` world.
//!
//! Its capabilities are `control`, `workers-start` and `workers-observe`, so the built
//! component cannot control a worker. It calls `workers-observe` for the host's two lists and
//! otherwise exactly where the native tool does: `describe` of the child it has just started,
//! whose route and model the success text names.
//!
//! The `tools` and `environment` enums of the schema are the host's lists, read from
//! `workers-observe.grantable` and `.environments` (D084; answered on the restricted path
//! `declaration` runs on, D085). A module outside the grantable list is refused by the host's
//! `start` before anything starts, and its native text is relayed as the start error.
#![forbid(unsafe_code)]

mod delegation;

use delegation::{ResultItem, START_DESCRIPTION, START_NAME, STARTED_PREFIX, StartInput};
use p1_bindings_tool::generated::p1::module::worker_types::ChildSpec;
use p1_bindings_tool::generated::p1::module::{control, workers_observe, workers_start};
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
            delegation::start_schema(
                &workers_observe::grantable(),
                &workers_observe::environments(),
            ),
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
        // Nothing is started until the grant is non-empty; which modules may be granted is the
        // host's check at `start`, made before anything starts, as the native tool makes it.
        let mut tools = match grant(input.tools, &grantable()) {
            Ok(tools) => tools,
            Err(outcome) => return outcome,
        };
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
                // The description is the factory's route/model, shown to the parent; as in
                // the native tool, a failed `describe` writes nothing in its place.
                let description = workers_observe::describe(&id).unwrap_or_else(|_| String::new());
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

/// The host's `workers-observe.grantable` list (D084), for the empty-grant refusal `execute`
/// builds. A built component reads it from the host; the crate's own unit tests link no
/// import (which would trap), so there it is the empty list the member declared before
/// S6.11, which is what the refusal names.
fn grantable() -> Vec<String> {
    #[cfg(target_arch = "wasm32")]
    {
        workers_observe::grantable()
    }
    #[cfg(not(target_arch = "wasm32"))]
    {
        Vec::new()
    }
}

/// The grant a start asks for, without duplicates, or the native refusal of an empty one,
/// which names the host's `grantable` list.
fn grant(tools: Vec<String>, grantable: &[String]) -> Result<Vec<String>, ToolOutcome> {
    let tools = delegation::dedup(tools);
    if tools.is_empty() {
        return Err(delegation::error_outcome(&format!(
            "`tools` is required: list every tool module the worker needs, from: {}",
            grantable.join(", ")
        )));
    }
    Ok(tools)
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
    fn a_grant_gets_the_native_refusals() {
        let grantable = vec!["read".to_owned(), "shell".to_owned()];
        assert_eq!(
            grant(Vec::new(), &grantable),
            Err(delegation::error_outcome(
                "`tools` is required: list every tool module the worker needs, from: read, shell"
            ))
        );
        assert_eq!(
            grant(vec!["read".into(), "read".into()], &grantable),
            Ok(vec!["read".to_owned()])
        );
    }

    /// The empty grant is refused at EXECUTION, not only in `grant`: `execute` builds the
    /// refusal before it reaches `control::cancelled` or `workers_start::start`. The crate's
    /// own tests link no host, so `grantable()` answers an empty list here and the refusal
    /// names that; the built component names the host's list instead.
    #[test]
    fn an_empty_grant_is_refused_at_execution() {
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
