//! Shared guest execution for three separately packaged agent tools. The existing
//! worker capability owns lifecycle, authorization, cancellation and journal identity.

#[path = "../../p1-module-worker-start/src/delegation.rs"]
mod delegation;

use p1_bindings_tool::generated::p1::module::worker_types::{ChildSpec, ChildStatus};
use p1_bindings_tool::generated::p1::module::{control, workers_observe, workers_start};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use serde::Deserialize;
use serde_json::json;

pub struct Subagent;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Query {
    query: String,
    context: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Task {
    prompt: String,
    description: String,
}

fn spec(call: &str) -> Result<ChildSpec, ToolOutcome> {
    let task = if crate::NAME == "Task" {
        let input: Task = delegation::parse_input(crate::NAME, call)?;
        if input.prompt.trim().is_empty() || input.description.trim().is_empty() {
            return Err(delegation::error_outcome(
                "prompt and description must be non-empty",
            ));
        }
        input.prompt
    } else {
        let input: Query = delegation::parse_input(crate::NAME, call)?;
        if input.query.trim().is_empty() {
            return Err(delegation::error_outcome("query must be non-empty"));
        }
        match input.context.filter(|text| !text.trim().is_empty()) {
            Some(context) => format!("Context: {context}\n\nQuery: {}", input.query),
            None => input.query,
        }
    };
    Ok(ChildSpec {
        environment: crate::ENVIRONMENT.into(),
        task,
        tools: crate::TOOLS.iter().map(|tool| (*tool).into()).collect(),
    })
}

impl Guest for Subagent {
    fn declaration() -> ToolDeclaration {
        let schema = if crate::NAME == "Task" {
            json!({"type":"object", "properties":{
                "prompt":{"type":"string", "minLength":1},
                "description":{"type":"string", "minLength":1}
            }, "required":["prompt","description"], "additionalProperties":false})
        } else {
            json!({"type":"object", "properties":{
                "query":{"type":"string", "minLength":1},
                "context":{"type":"string"}
            }, "required":["query"], "additionalProperties":false})
        };
        delegation::declaration(crate::NAME, crate::DESCRIPTION, schema)
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Delegates
    }

    fn describe(_call: ToolCall) -> CallDescription {
        delegation::call_description(Some(crate::ENVIRONMENT.into()))
    }

    fn describe_result(_call: ToolCall, result: HistoryItem) -> ResultDescription {
        delegation::result_description(
            &delegation::plain_summary(&delegation::ResultItem::parse(&result)),
            None,
        )
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let spec = match spec(&call) {
            Ok(spec) => spec,
            Err(error) => return error,
        };
        if control::cancelled() {
            return delegation::cancelled_outcome();
        }
        let id = match workers_start::start(&spec) {
            Ok(id) => id,
            Err(error) => return delegation::start_error(error),
        };
        let status = match workers_observe::wait(&id) {
            Ok(ChildStatus::Running) => return delegation::cancelled_outcome(),
            Ok(status) => status,
            Err(error) => return delegation::id_error(&id, error),
        };
        // Keep the existing start-result prefix so resume can reserve the child's id.
        delegation::ok_outcome(&format!(
            "{}{} on {}.\n{}",
            delegation::STARTED_PREFIX,
            id,
            crate::ENVIRONMENT,
            delegation::render_status(&id, &status)
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(input: serde_json::Value) -> String {
        json!({"name":crate::NAME,"input":{"kind":"json","raw":input.to_string()}}).to_string()
    }

    #[test]
    fn fixed_environment_and_grant_cannot_be_overridden() {
        let input = if crate::NAME == "Task" {
            json!({"prompt":"fix asymmetric case","description":"fix"})
        } else {
            json!({"query":"find asymmetric case","context":"only parser"})
        };
        let result = spec(&call(input.clone())).unwrap();
        assert_eq!(result.environment, crate::ENVIRONMENT);
        assert_eq!(result.tools, crate::TOOLS);
        if crate::NAME == "Task" {
            assert_eq!(result.task, "fix asymmetric case");
        } else {
            assert_eq!(
                result.task,
                "Context: only parser\n\nQuery: find asymmetric case"
            );
        }
        for key in ["environment", "tools", "model"] {
            let mut bad = input.clone();
            bad[key] = json!("override");
            assert!(spec(&call(bad)).is_err());
        }
    }

    #[test]
    fn rejects_blank_or_missing_task_before_calling_host_imports() {
        for input in [
            json!({}),
            json!({"query":"  "}),
            json!({"prompt":" ","description":"fix"}),
        ] {
            let result = Subagent::execute(call(input));
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(&result).unwrap()["status"],
                "error"
            );
        }
    }
}
