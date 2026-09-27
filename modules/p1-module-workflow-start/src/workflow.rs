//! The pure part of the four workflow members, shared by their packages.
//!
//! Each workflow package (`p1-module-workflow-start`, `-workflow-status`, `-workflow-result`,
//! `-workflow-cancel`) includes this file as its own module, so the eight member packages stay
//! separate components while their declarations, input parsing and rendered texts are one
//! copy of the native crate's (`crates/p1-tool-workflow/src/lib.rs`): every constant, schema,
//! serde input type and text below mirrors it byte for byte. Nothing here calls an import;
//! each member calls its own `workflows` operation.
//!
//! Each package uses only its member's part, which is why unused items are allowed here.
#![allow(dead_code)]

use p1_bindings_tool::generated::ToolDeclaration;
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::workflows::WorkflowError;
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

pub const START_NAME: &str = "workflow_start";
pub const STATUS_NAME: &str = "workflow_status";
pub const RESULT_NAME: &str = "workflow_result";
pub const CANCEL_NAME: &str = "workflow_cancel";

pub const START_DESCRIPTION: &str = "Start a workflow ONLY when the user asks for a workflow. Write a rhai (JavaScript-like) script that starts workers by ROLE (configured: worker, reviewer, verifier, judge); never name a model in the script. agent(prompt, opts) returns an envelope, not a value: check its status. The run happens in the background and ONE notification arrives at the end; do not poll. Caps are enforced per model: a capped step is failed with quota_exceeded. Never split a workflow into several to get around a cap. resume_from replays the unchanged prefix of an earlier run.";
pub const STATUS_DESCRIPTION: &str = "Read a workflow's compact progress or ended summary. Do not poll: one notification arrives when it ends.";
pub const RESULT_DESCRIPTION: &str = "Read a workflow's result; set wait to true to await its end (the wait can be cancelled). Verify the result before relying on it.";
pub const CANCEL_DESCRIPTION: &str = "Cancel a workflow run and its in-flight steps.";

// ---------------------------------------------------------------- inputs

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StartInput {
    pub script: String,
    #[serde(default = "empty_object")]
    pub args: serde_json::Map<String, Value>,
    pub resume_from: Option<String>,
}

fn empty_object() -> serde_json::Map<String, Value> {
    serde_json::Map::new()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct IdInput {
    pub id: String,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResultInput {
    pub id: String,
    #[serde(default)]
    pub wait: bool,
}

// ---------------------------------------------------------------- schemas

pub fn start_schema() -> Value {
    json!({"type":"object","properties":{
        "script":{"type":"string","description":"Rhai script using roles, not models."},
        "args":{"type":"object","description":"Named script arguments."},
        "resume_from":{"type":"string","description":"Earlier run id whose unchanged prefix is replayed."}
    },"required":["script"],"additionalProperties":false})
}

pub fn result_schema() -> Value {
    json!({"type":"object","properties":{
        "id":{"type":"string","description":"Workflow id, e.g. wf1."},
        "wait":{"type":"boolean","default":false,"description":"Wait for the run to end."}
    },"required":["id"],"additionalProperties":false})
}

pub fn id_schema() -> Value {
    json!({"type":"object","properties":{
        "id":{"type":"string","description":"Workflow id, e.g. wf1."}
    },"required":["id"],"additionalProperties":false})
}

pub fn declaration(name: &str, description: &str, schema: Value) -> ToolDeclaration {
    ToolDeclaration {
        name: name.to_owned(),
        description: description.to_owned(),
        kind: DeclarationKind::Function(schema.to_string()),
    }
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

/// A `tool_result` history item: only the content the model was shown.
#[derive(Debug, Default, Deserialize)]
struct ResultItem {
    #[serde(default)]
    content: String,
}

/// The member's input, or the error outcome the native tool gives. The tool name in the
/// message is the name the call was made under — the face the model saw, as the native
/// tool's declaration name is — and the member's own name when the call is unreadable.
pub fn parse_input<T: DeserializeOwned>(default_name: &str, call: &str) -> Result<T, String> {
    let call: WireCall = serde_json::from_str(call)
        .map_err(|error| error_outcome(&format!("Invalid input for {default_name}: {error}")))?;
    let raw = match call.input {
        WireInput::Json { raw } => raw,
        WireInput::Text {} => {
            return Err(error_outcome(&format!(
                "Invalid input for {}: expected a JSON object input, got freeform text",
                call.name
            )));
        }
    };
    serde_json::from_str(&raw)
        .map_err(|error| error_outcome(&format!("Invalid input for {}: {error}", call.name)))
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

/// A `call-description`: every workflow member describes a call as the verb `workflow`, never
/// destructive, with a target when the input names one.
pub fn call_description(target: Option<String>) -> String {
    let mut description = json!({"verb": "workflow", "destructive": false});
    if let Some(target) = target {
        description["target"] = Value::String(target);
    }
    description.to_string()
}

/// The native `text_result`: the line count, and the whole content as text detail.
pub fn text_result(tool_result: &str) -> String {
    let content = serde_json::from_str::<ResultItem>(tool_result)
        .unwrap_or_default()
        .content;
    json!({
        "summary": format!("{} lines", content.lines().count()),
        "detail": {"kind": "text", "text": content}
    })
    .to_string()
}

// ---------------------------------------------------------------- errors

/// The `p1_workflow::WorkflowError` display text of each variant.
fn workflow_error_text(error: &WorkflowError) -> String {
    match error {
        WorkflowError::UnknownRun => "no such workflow run".to_owned(),
        WorkflowError::Parse(parse) => format!(
            "script does not parse: {} [line {}, column {}]",
            parse.message, parse.line, parse.column
        ),
        WorkflowError::Preflight(reason) => format!("workflow cannot start: {reason}"),
        WorkflowError::ShutDown => "the workflow service has shut down".to_owned(),
        WorkflowError::Io(reason) => format!("workflow i/o: {reason}"),
    }
}

pub fn start_error(error: WorkflowError) -> String {
    match error {
        WorkflowError::Parse(parse) => error_outcome(&format!(
            "Script does not parse: {} [line {}, column {}]",
            parse.message, parse.line, parse.column
        )),
        WorkflowError::Preflight(reason) | WorkflowError::Io(reason) => {
            error_outcome(&format!("Cannot start workflow: {reason}"))
        }
        WorkflowError::ShutDown => {
            error_outcome("Cannot start workflow: the workflow service has shut down.")
        }
        WorkflowError::UnknownRun => error_outcome("Cannot start workflow: no such workflow run"),
    }
}

pub fn id_error(id: &str, error: WorkflowError) -> String {
    match error {
        WorkflowError::UnknownRun => error_outcome(&format!("No workflow {id}.")),
        other => error_outcome(&workflow_error_text(&other)),
    }
}

// ---------------------------------------------------------------- run status

/// `p1_workflow::RunStatus` in its serde form, the JSON text `workflows.status` and
/// `workflows.wait` return: only the fields the texts read. Unknown fields are ignored, as
/// serde ignores them by default, so a report that grows keeps parsing.
#[derive(Debug, Deserialize)]
pub enum RunStatus {
    Running(RunProgress),
    Ended(RunReport),
}

#[derive(Debug, Deserialize)]
pub struct RunProgress {
    pub phase: Option<String>,
    pub steps_started: u32,
    pub steps_ended: u32,
    pub replayed: u32,
    pub log: Vec<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    CompletedWithIssues,
    Failed,
    Cancelled,
}

#[derive(Debug, Deserialize)]
pub struct Counts {
    pub steps: u32,
    pub replayed: u32,
    pub done: u32,
    pub blocked: u32,
    pub failed: u32,
    pub cancelled: u32,
    pub not_verified: u32,
    pub capped: u32,
    pub invalid_output: u32,
    pub fell_back: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Done,
    Blocked,
    Failed,
    Cancelled,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovedOn {
    RouteFailed,
    Capped,
}

#[derive(Debug, Deserialize)]
pub struct ModelTry {
    pub model: String,
    pub moved_on: Option<MovedOn>,
}

#[derive(Debug, Deserialize)]
pub struct StepLine {
    /// The step's `CallId`, a newtype that serializes as its string.
    pub call: String,
    pub label: Option<String>,
    pub role: String,
    pub model: String,
    pub worker: Option<String>,
    pub status: StepStatus,
    pub schema: String,
    pub evidence: Option<String>,
    pub attempts: u32,
    pub replayed: bool,
    pub error: Option<String>,
    pub models: Vec<ModelTry>,
}

impl StepLine {
    /// The chain as a line names it: `a`, or `a route failed → b` when the step moved on
    /// (ADR-0054 item 4). The role's own model when nothing was dispatched.
    pub fn model_chain(&self) -> String {
        if self.models.is_empty() {
            return self.model.clone();
        }
        self.models
            .iter()
            .map(|tried| match tried.moved_on {
                None => tried.model.clone(),
                Some(MovedOn::RouteFailed) => format!("{} route failed", tried.model),
                Some(MovedOn::Capped) => format!("{} capped", tried.model),
            })
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

#[derive(Debug, Deserialize)]
pub struct RunReport {
    /// The run's `RunId`, a newtype that serializes as its string.
    pub id: String,
    pub outcome: RunOutcome,
    pub value: Value,
    pub counts: Counts,
    pub steps: Vec<StepLine>,
    pub error: Option<String>,
    /// The run directory, a path that serializes as its text.
    pub run_dir: String,
}

/// The run status the host returned, or the error outcome for text that is not one. The
/// host writes it from a `RunStatus`, so this is the host breaking its contract; the model is
/// told the status was unreadable rather than shown a wrong one.
pub fn parse_status(text: &str) -> Result<RunStatus, String> {
    serde_json::from_str(text)
        .map_err(|error| error_outcome(&format!("Unreadable workflow status: {error}")))
}

// ---------------------------------------------------------------- rendering

pub fn render_progress(id: &str, progress: &RunProgress) -> String {
    let phase = progress.phase.as_deref().unwrap_or("none");
    let mut text = format!(
        "Workflow {id}: running — phase {phase}, {} steps started, {} ended, {} replayed",
        progress.steps_started, progress.steps_ended, progress.replayed
    );
    for line in &progress.log {
        text.push_str("\n  ");
        text.push_str(line);
    }
    text
}

pub fn report_line(report: &RunReport) -> String {
    let outcome = match report.outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::CompletedWithIssues => "completed with issues",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
    };
    let c = &report.counts;
    format!(
        "Workflow {}: {outcome} — {} steps ({} replayed): {} done, {} blocked, {} failed, {} cancelled; {} not verified; {} capped; {} invalid output; {} fell back",
        report.id,
        c.steps,
        c.replayed,
        c.done,
        c.blocked,
        c.failed,
        c.cancelled,
        c.not_verified,
        c.capped,
        c.invalid_output,
        c.fell_back
    )
}

fn render_step(step: &StepLine) -> String {
    let status = match step.status {
        StepStatus::Done => "done",
        StepStatus::Blocked => "blocked",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
    };
    // The model part is the chain the step walked (ADR-0054 item 4).
    let mut line = format!(
        "  {} {} → {}",
        step.label.as_deref().unwrap_or(&step.call),
        step.role,
        step.model_chain()
    );
    if let Some(worker) = &step.worker {
        line.push_str(&format!(" [{worker}]"));
    }
    line.push_str(&format!(" {status} — schema {}", step.schema));
    if let Some(evidence) = &step.evidence {
        line.push_str(&format!("; {evidence}"));
    }
    if step.replayed {
        line.push_str("; replayed");
    }
    if step.attempts > 1 {
        line.push_str(&format!("; attempts {}", step.attempts));
    }
    if let Some(error) = &step.error {
        line.push_str(&format!("; {error}"));
    }
    line
}

pub fn render_report(report: &RunReport) -> String {
    let mut text = report_line(report);
    if let Some(error) = &report.error {
        text.push_str(&format!("\nerror: {error}"));
    }
    text.push_str("\nsteps:");
    for step in report.steps.iter().take(200) {
        text.push('\n');
        text.push_str(&render_step(step));
    }
    if report.steps.len() > 200 {
        text.push_str(&format!(
            "\n  … {} more in result.json",
            report.steps.len() - 200
        ));
    }
    // A `Value` always serializes; the fallback only keeps a guest panic (a trap) out.
    let value = serde_json::to_string_pretty(&report.value).unwrap_or_default();
    text.push_str("\nresult:\n");
    if value.len() > 16 * 1024 {
        let mut end = 16 * 1024;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&value[..end]);
        text.push_str(&format!(
            "… (truncated; full value in {}/result.json)",
            report.run_dir
        ));
    } else {
        text.push_str(&value);
    }
    text.push_str(&format!("\nrun dir: {}", report.run_dir));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_bindings_tool::generated::p1::module::workflows::ParseError;

    /// A run status as `p1_workflow::RunStatus` serializes one.
    const ENDED: &str = r#"{"Ended":{"id":"wf1","outcome":"completed_with_issues","value":{"b":1,"a":[true]},
        "counts":{"steps":2,"replayed":0,"done":1,"blocked":0,"failed":1,"cancelled":0,"not_verified":1,
        "capped":0,"invalid_output":0,"fell_back":1},
        "steps":[{"call":"c-1","ordinal":1,"label":"plan","role":"worker","model":"e/p","worker":"w1",
          "status":"done","schema":"ok","evidence":"not verified","attempts":2,"replayed":false,"error":null,
          "models":[{"model":"e/a","moved_on":"route_failed"},{"model":"e/b","moved_on":null}]},
          {"call":"c-2","ordinal":2,"label":null,"role":"judge","model":"e/j","worker":null,
          "status":"failed","schema":"none","evidence":null,"attempts":1,"replayed":true,"error":"route: down",
          "models":[]}],
        "error":null,"run_dir":"/runs/wf1"}}"#;

    #[test]
    fn renders_an_ended_run_as_the_native_report() {
        let Ok(RunStatus::Ended(report)) = parse_status(ENDED) else {
            panic!("the ended status parses");
        };
        assert_eq!(
            render_report(&report),
            "Workflow wf1: completed with issues — 2 steps (0 replayed): 1 done, 0 blocked, 1 failed, 0 cancelled; 1 not verified; 0 capped; 0 invalid output; 1 fell back\n\
             steps:\n  plan worker → e/a route failed → e/b [w1] done — schema ok; not verified; attempts 2\n  \
             c-2 judge → e/j failed — schema none; replayed; route: down\n\
             result:\n{\n  \"a\": [\n    true\n  ],\n  \"b\": 1\n}\nrun dir: /runs/wf1"
        );
    }

    #[test]
    fn renders_a_running_run_as_its_progress() {
        let running = r#"{"Running":{"phase":null,"steps_started":3,"steps_ended":1,"replayed":0,"log":["a","b"]}}"#;
        let Ok(RunStatus::Running(progress)) = parse_status(running) else {
            panic!("the running status parses");
        };
        assert_eq!(
            render_progress("wf2", &progress),
            "Workflow wf2: running — phase none, 3 steps started, 1 ended, 0 replayed\n  a\n  b"
        );
    }

    #[test]
    fn errors_render_as_the_native_texts() {
        let parse = || {
            WorkflowError::Parse(ParseError {
                message: "bad".to_owned(),
                line: 2,
                column: 5,
            })
        };
        assert_eq!(
            start_error(parse()),
            error_outcome("Script does not parse: bad [line 2, column 5]")
        );
        assert_eq!(
            id_error("wf1", parse()),
            error_outcome("script does not parse: bad [line 2, column 5]")
        );
        assert_eq!(
            id_error("wf9", WorkflowError::UnknownRun),
            error_outcome("No workflow wf9.")
        );
        assert_eq!(
            id_error("wf1", WorkflowError::Io("disk".to_owned())),
            error_outcome("workflow i/o: disk")
        );
    }

    #[test]
    fn a_result_is_described_by_its_line_count() {
        let item = json!({"item": "tool_result", "call_id": "c1", "name": "workflow_status",
            "status": "ok", "content": "a\nb"})
        .to_string();
        assert_eq!(
            text_result(&item),
            r#"{"detail":{"kind":"text","text":"a\nb"},"summary":"2 lines"}"#
        );
    }
}
