//! The pure part of the four worker members, shared by their packages.
//!
//! Each worker package (`p1-module-worker-start`, `-worker-result`, `-worker-continue`,
//! `-worker-cancel`) includes this file as its own module, so the eight member packages stay
//! separate components while their declarations, input parsing and rendered texts are one
//! copy of the native crate's (`crates/p1-tool-delegate/src/lib.rs`): every constant, schema,
//! serde input type and text below mirrors it byte for byte. Nothing here calls an import;
//! the capability calls are each member's own, so a member's component imports only the
//! worker interface it calls.
//!
//! Each package uses only its member's part, which is why unused items are allowed here.
#![allow(dead_code)]

use p1_bindings_tool::generated::ToolDeclaration;
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::worker_types::{
    ChildStatus, WorkerError, WorkerReport,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub const START_NAME: &str = "worker_start";
pub const START_DESCRIPTION: &str = "Start a worker agent on an environment with a self-contained task.\nThe worker gets ONLY the task text — no conversation history — so the task must contain everything it needs.\nWorkers share this workspace: do not give two workers overlapping files.\nYou will be notified when it finishes; do not poll for it.\nThe worker has ONLY the tools you list in `tools` (plus finish); tools you do not list do not exist for it. List every tool the task needs; if you are unsure whether it needs one, include it.";

pub const RESULT_NAME: &str = "worker_result";
pub const RESULT_DESCRIPTION: &str = "Read a worker's status and its final text.\nSet wait to true to block until the worker is no longer running (you can be cancelled while waiting).\nVerify the worker's result before relying on it.";

pub const CONTINUE_NAME: &str = "worker_continue";
pub const CONTINUE_DESCRIPTION: &str = "Send another message into a worker's session to repair or extend its work.\nFails while the worker is still running a turn.\nUse add_tools to give the worker tools it lacks (e.g. after it finished blocked naming a missing tool); it keeps its context.";

pub const CANCEL_NAME: &str = "worker_cancel";
pub const CANCEL_DESCRIPTION: &str =
    "Cancel a running worker's current turn. The worker's session is kept.";

/// How a successful `worker_start` result begins; the host's `workers_started_in` reads it back.
pub const STARTED_PREFIX: &str = "Started worker ";

/// The tool modules a worker may be granted and the environments it may run, as this component
/// knows them: none. The frozen `tool` world has no settings export and reads `declaration` on
/// the restricted path, so the host's lists cannot reach a component; the schemas carry empty
/// enums, exactly the native declaration built over empty lists, and module membership is the
/// host's to enforce where the lists live (`workers-start.start`, `workers-control.continue-child`).
pub const GRANTABLE: &[String] = &[];
pub const ENVIRONMENTS: &[String] = &[];

// ---------------------------------------------------------------- inputs

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartInput {
    pub environment: String,
    pub task: String,
    /// `serde(default)` so a missing `tools` reaches `execute` as an empty list and
    /// gets the actionable "`tools` is required" message rather than a serde error.
    #[serde(default)]
    pub tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultInput {
    pub id: String,
    #[serde(default)]
    pub wait: bool,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContinueInput {
    pub id: String,
    pub message: String,
    /// `serde(default)` so a continue without added tools is the ordinary repair.
    #[serde(default)]
    pub add_tools: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CancelInput {
    pub id: String,
}

// ---------------------------------------------------------------- wire

/// A `tool-call` (`p1:protocol/tool-call/1`): only what a member reads of it.
#[derive(Debug, Deserialize)]
struct WireCall {
    name: String,
    input: WireInput,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum WireInput {
    Json { raw: String },
    Text {},
}

/// A `tool_result` history item: its status and the content the model was shown.
#[derive(Debug, Default, Deserialize)]
pub struct ResultItem {
    #[serde(default)]
    pub status: String,
    #[serde(default)]
    pub content: String,
}

impl ResultItem {
    /// The item the host passed, or an empty one when it is unreadable, so a description
    /// falls back to the plain first-line summary instead of failing.
    pub fn parse(item: &str) -> Self {
        serde_json::from_str(item).unwrap_or_default()
    }

    pub fn is_ok(&self) -> bool {
        self.status == "ok"
    }
}

/// The member's input, or the error outcome the native tool gives. The tool name in the
/// message is the name the call was made under — the face the model saw, as the native
/// tool's declaration name is — and the member's own name when the call is unreadable.
pub fn parse_input<T: DeserializeOwned>(default_name: &str, call: &str) -> Result<T, String> {
    let call: WireCall = serde_json::from_str(call)
        .map_err(|error| error_outcome(&invalid(default_name, &error.to_string())))?;
    let raw = match call.input {
        WireInput::Json { raw } => raw,
        WireInput::Text {} => {
            return Err(error_outcome(&invalid(
                &call.name,
                "expected a JSON object input, got freeform text",
            )));
        }
    };
    serde_json::from_str(&raw)
        .map_err(|error| error_outcome(&invalid(&call.name, &error.to_string())))
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

pub fn ok_outcome(content: &str) -> String {
    json!({"status": "ok", "content": content}).to_string()
}

pub fn error_outcome(content: &str) -> String {
    json!({"status": "error", "content": content}).to_string()
}

/// The native tools answer a cancelled call with the `cancelled` status and no content.
pub fn cancelled_outcome() -> String {
    json!({"status": "cancelled", "content": ""}).to_string()
}

/// A `call-description`: every worker member describes a call as the verb `worker`, never
/// destructive, with a target when the input names one.
pub fn call_description(target: Option<String>) -> String {
    let mut description = json!({"verb": "worker", "destructive": false});
    if let Some(target) = target {
        description["target"] = Value::String(target);
    }
    description.to_string()
}

/// A `result-description`, with an optional text detail.
pub fn result_description(summary: &str, detail: Option<String>) -> String {
    let mut description = json!({"summary": summary});
    if let Some(text) = detail {
        description["detail"] = json!({"kind": "text", "text": text});
    }
    description.to_string()
}

/// The native `plain_result` summary: the result's first line.
pub fn plain_summary(item: &ResultItem) -> String {
    item.content.lines().next().unwrap_or_default().to_owned()
}

pub fn declaration(name: &str, description: &str, schema: Value) -> ToolDeclaration {
    ToolDeclaration {
        name: name.to_owned(),
        description: description.to_owned(),
        kind: DeclarationKind::Function(schema.to_string()),
    }
}

// ---------------------------------------------------------------- errors

/// The `p1_workers::WorkerError` display text of each variant, for the arms the native tool
/// renders with `to_string()`.
fn worker_error_text(error: &WorkerError) -> String {
    match error {
        WorkerError::UnknownChild => "no such worker".to_owned(),
        WorkerError::Busy => "the worker is still running a turn".to_owned(),
        WorkerError::LimitReached(max) => format!("at most {max} workers may run at once"),
        WorkerError::InvalidEnvironment(message) => {
            format!("invalid child environment: {message}")
        }
        WorkerError::IdsExhausted => {
            "the worker id namespace is exhausted: no id can be allocated".to_owned()
        }
        WorkerError::Regrant(reason) => format!("the worker's tools were not changed: {reason}"),
        WorkerError::ShutDown => "the worker service has shut down".to_owned(),
    }
}

/// A `worker_start` failure, with the exact texts the model can act on.
pub fn start_error(error: WorkerError) -> String {
    match error {
        WorkerError::LimitReached(max) => error_outcome(&format!(
            "Cannot start another worker: {max} are already running."
        )),
        WorkerError::InvalidEnvironment(message) => {
            error_outcome(&format!("Cannot start worker: {message}"))
        }
        WorkerError::ShutDown => {
            error_outcome("Cannot start worker: the worker service has shut down.")
        }
        // `start` never returns these; keep the mapping total and honest.
        other => error_outcome(&worker_error_text(&other)),
    }
}

/// A failure addressed by worker id.
pub fn id_error(id: &str, error: WorkerError) -> String {
    match error {
        WorkerError::UnknownChild => error_outcome(&format!("No worker {id}.")),
        WorkerError::Busy => error_outcome(&format!("Worker {id} is still running.")),
        // The re-grant was refused: no turn ran and the worker kept its tools, so the
        // parent can act on the reason (ADR-0050 item 6).
        WorkerError::Regrant(reason) => error_outcome(&format!(
            "Cannot add tools to worker {id}: {reason}. The worker keeps its tools."
        )),
        WorkerError::ShutDown => error_outcome(&format!(
            "Worker {id} is unavailable: the service has shut down."
        )),
        // The remaining variants cannot occur for an existing id.
        other => error_outcome(&worker_error_text(&other)),
    }
}

/// Removes duplicates, keeping the first occurrence's order, as the native tools do.
pub fn dedup(modules: Vec<String>) -> Vec<String> {
    let mut unique: Vec<String> = Vec::with_capacity(modules.len());
    for module in modules {
        if !unique.contains(&module) {
            unique.push(module);
        }
    }
    unique
}

// ---------------------------------------------------------------- rendering

/// Status line, then the retained text: final text for `finished`, the failure
/// message for `failed`. `cancelled` retains no text, so it is the status line.
///
/// A FINISHED worker's result begins with its report (ADR-0050 item 6): the tools it
/// was assembled with, its `finish`, and every call it made to a tool it was not
/// given — then a `---` line, then the status line and text.
pub fn render_status(id: &str, status: &ChildStatus) -> String {
    match status {
        ChildStatus::Running => format!("Worker {id}: running"),
        ChildStatus::Finished(result) => format!(
            "{}\n---\nWorker {id}: finished\n\n{}",
            render_report(&result.report),
            result.final_text
        ),
        ChildStatus::Failed(message) => format!("Worker {id}: failed\n\n{message}"),
        ChildStatus::Cancelled => format!("Worker {id}: cancelled"),
    }
}

/// The report lines a finished worker's result begins with. The missing-call line
/// is omitted entirely when the worker called no tool it was not given, and the
/// evidence (ADR-0051 item 3) is appended to the status line.
pub fn render_report(report: &WorkerReport) -> String {
    let mut lines = vec![format!("tools: {}", report.tools.join(", "))];
    lines.push(match &report.finish {
        Some(finish) => {
            let status = match (finish.status.as_str(), &finish.needs) {
                ("blocked", Some(needs)) => format!("blocked — needs: {needs}"),
                (status, _) => status.to_string(),
            };
            match &finish.evidence {
                Some(evidence) => format!("finish: {status} — {evidence}"),
                None => format!("finish: {status}"),
            }
        }
        None => "finish: not called".to_string(),
    });
    if !report.missing_tool_calls.is_empty() {
        let calls: Vec<String> = report
            .missing_tool_calls
            .iter()
            .map(|(name, count)| format!("{name} x{count}"))
            .collect();
        lines.push(format!(
            "calls to tools it was not given: {}",
            calls.join(", ")
        ));
    }
    lines.join("\n")
}

// ---------------------------------------------------------------- schemas

pub fn start_schema(grantable: &[String], environments: &[String]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "environment": {
                "type": "string",
                "enum": environments,
                "description": "Environment (prompt, model and tools) the worker runs on."
            },
            "task": {
                "type": "string",
                "description": "The complete, self-contained task for the worker."
            },
            "tools": {
                "type": "array",
                "minItems": 1,
                "uniqueItems": true,
                "items": {
                    "type": "string",
                    "enum": grantable
                },
                "description": "Every tool module the worker needs. The worker gets ONLY these, plus finish."
            }
        },
        "required": ["environment", "task", "tools"],
        "additionalProperties": false
    })
}

pub fn result_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Worker id, e.g. \"w1\"."
            },
            "wait": {
                "type": "boolean",
                "default": false,
                "description": "Block until the worker is no longer running."
            }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

pub fn continue_schema(grantable: &[String]) -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Worker id, e.g. \"w1\"."
            },
            "message": {
                "type": "string",
                "description": "The message to send into the worker's session."
            },
            "add_tools": {
                "type": "array",
                "uniqueItems": true,
                "items": {
                    "type": "string",
                    "enum": grantable
                },
                "description": "Tool modules to ADD to the worker's grant for this and every later turn. The worker keeps its context."
            }
        },
        "required": ["id", "message"],
        "additionalProperties": false
    })
}

pub fn cancel_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Worker id, e.g. \"w1\"."
            }
        },
        "required": ["id"],
        "additionalProperties": false
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_bindings_tool::generated::p1::module::types::StopReason;
    use p1_bindings_tool::generated::p1::module::worker_types::{
        ChildResult, FinishReport, TurnEnd,
    };

    /// A wire tool call with JSON input `raw` under the name `name`.
    pub fn json_call(name: &str, raw: &str) -> String {
        json!({"call_id": "c1", "name": name, "input": {"kind": "json", "raw": raw}}).to_string()
    }

    #[test]
    fn input_errors_are_the_native_texts() {
        let text_call =
            json!({"call_id": "c1", "name": "worker_cancel", "input": {"kind": "text", "raw": "w1"}})
                .to_string();
        assert_eq!(
            parse_input::<CancelInput>(CANCEL_NAME, &text_call).unwrap_err(),
            error_outcome(
                "Invalid input for worker_cancel: expected a JSON object input, got freeform text"
            )
        );
        // The serde message is serde_json's own, as the native tool's is.
        let unknown = parse_input::<CancelInput>(
            CANCEL_NAME,
            &json_call("worker_cancel", r#"{"id":"w1","x":1}"#),
        )
        .unwrap_err();
        assert!(
            unknown.contains("Invalid input for worker_cancel: unknown field `x`, expected `id`")
        );
        let input: StartInput = parse_input(
            START_NAME,
            &json_call("worker_start", r#"{"environment":"e","task":"t"}"#),
        )
        .unwrap();
        assert!(input.tools.is_empty());
    }

    #[test]
    fn a_finished_worker_renders_its_report_first() {
        let status = ChildStatus::Finished(ChildResult {
            final_text: "all done".to_owned(),
            turn_end: TurnEnd::Completed(StopReason::EndTurn),
            usage_total: None,
            report: WorkerReport {
                tools: vec!["read".to_owned(), "finish".to_owned()],
                finish: Some(FinishReport {
                    status: "blocked".to_owned(),
                    needs: Some("shell".to_owned()),
                    summary: None,
                    evidence: None,
                }),
                missing_tool_calls: vec![("shell".to_owned(), 2)],
            },
        });
        assert_eq!(
            render_status("w1", &status),
            "tools: read, finish\nfinish: blocked — needs: shell\ncalls to tools it was not given: shell x2\n---\nWorker w1: finished\n\nall done"
        );
        assert_eq!(
            render_status("w2", &ChildStatus::Running),
            "Worker w2: running"
        );
        assert_eq!(
            render_status("w3", &ChildStatus::Failed("boom".to_owned())),
            "Worker w3: failed\n\nboom"
        );
    }

    #[test]
    fn errors_render_as_the_native_texts() {
        assert_eq!(
            start_error(WorkerError::LimitReached(4)),
            error_outcome("Cannot start another worker: 4 are already running.")
        );
        assert_eq!(
            id_error("w9", WorkerError::UnknownChild),
            error_outcome("No worker w9.")
        );
        assert_eq!(
            id_error("w1", WorkerError::Regrant("no such module".to_owned())),
            error_outcome(
                "Cannot add tools to worker w1: no such module. The worker keeps its tools."
            )
        );
        assert_eq!(
            id_error("w1", WorkerError::IdsExhausted),
            error_outcome("the worker id namespace is exhausted: no id can be allocated")
        );
    }

    #[test]
    fn descriptions_are_protocol_json() {
        assert_eq!(
            call_description(Some("w1".to_owned())),
            r#"{"destructive":false,"target":"w1","verb":"worker"}"#
        );
        assert_eq!(
            result_description("cancelled", None),
            r#"{"summary":"cancelled"}"#
        );
        assert_eq!(
            dedup(vec!["a".into(), "b".into(), "a".into()]),
            vec!["a", "b"]
        );
    }
}
