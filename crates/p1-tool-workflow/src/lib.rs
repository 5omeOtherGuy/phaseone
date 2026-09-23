//! The four model-facing workflow tools. They depend only on the workflow service
//! trait; the host owns the implementation, background execution and notification.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
use p1_workflow::{
    RunId, RunOutcome, RunProgress, RunReport, RunStatus, StartRequest, StepLine, StepStatus,
    WorkflowError, WorkflowService,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::{Value, json};

/// Model-facing name and description override without a dependency on the host.
#[derive(Debug, Clone)]
pub struct ToolFace {
    pub name: String,
    pub description: String,
}

impl ToolFace {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

const START_DESCRIPTION: &str = "Start a workflow ONLY when the user asks for a workflow. Write a rhai (JavaScript-like) script that starts workers by ROLE (configured: worker, reviewer, verifier, judge); never name a model in the script. agent(prompt, opts) returns an envelope, not a value: check its status. The run happens in the background and ONE notification arrives at the end; do not poll. Caps are enforced per model: a capped step is failed with quota_exceeded. Never split a workflow into several to get around a cap. resume_from replays the unchanged prefix of an earlier run.";
const STATUS_DESCRIPTION: &str = "Read a workflow's compact progress or ended summary. Do not poll: one notification arrives when it ends.";
const RESULT_DESCRIPTION: &str = "Read a workflow's result; set wait to true to await its end (the wait can be cancelled). Verify the result before relying on it.";
const CANCEL_DESCRIPTION: &str = "Cancel a workflow run and its in-flight steps.";

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

fn declaration(name: &str, description: &str, schema: Value) -> ToolDeclaration {
    ToolDeclaration {
        name: name.to_string(),
        description: description.to_string(),
        kind: DeclarationKind::Function {
            input_schema: schema,
        },
    }
}

/// The four tools, in start/status/result/cancel order.
pub fn all(service: Arc<dyn WorkflowService>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WorkflowStartTool::new(Arc::clone(&service))),
        Arc::new(WorkflowStatusTool::new(Arc::clone(&service))),
        Arc::new(WorkflowResultTool::new(Arc::clone(&service))),
        Arc::new(WorkflowCancelTool::new(service)),
    ]
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    script: String,
    #[serde(default = "empty_object")]
    args: serde_json::Map<String, Value>,
    resume_from: Option<String>,
}

fn empty_object() -> serde_json::Map<String, Value> {
    serde_json::Map::new()
}

/// Starts a script in the background; the host sends the completion notification.
pub struct WorkflowStartTool {
    service: Arc<dyn WorkflowService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkflowStartTool {
    pub fn new(service: Arc<dyn WorkflowService>) -> Self {
        Self {
            service,
            declaration: declaration(
                "workflow_start",
                START_DESCRIPTION,
                json!({"type":"object","properties":{
                    "script":{"type":"string","description":"Rhai script using roles, not models."},
                    "args":{"type":"object","description":"Named script arguments."},
                    "resume_from":{"type":"string","description":"Earlier run id whose unchanged prefix is replayed."}
                },"required":["script"],"additionalProperties":false}),
            ),
            identity: identity("default"),
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, schema(&self.declaration)),
            identity: identity(variant),
        }
    }
}

impl Tool for WorkflowStartTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::Delegates
    }
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let input: StartInput = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(outcome) => return outcome,
            };
            let resume_from = input.resume_from.map(RunId);
            let request = StartRequest {
                script: input.script,
                args: Value::Object(input.args),
                resume_from: resume_from.clone(),
                role_models: Default::default(),
                workspace: None,
            };
            match self.service.start(request).await {
                Ok(id) => {
                    let resumed = resume_from
                        .map(|old| format!(", resuming {}", old.0))
                        .unwrap_or_default();
                    ToolOutcome::ok(format!(
                        "Started workflow {}{resumed}. You will be notified when it ends; do not poll.",
                        id.0
                    ))
                }
                Err(error) => start_error(error),
            }
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct IdInput {
    id: String,
}

/// Nonblocking, compact status; ended runs share the result's first line.
pub struct WorkflowStatusTool {
    service: Arc<dyn WorkflowService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkflowStatusTool {
    pub fn new(service: Arc<dyn WorkflowService>) -> Self {
        Self {
            service,
            declaration: declaration("workflow_status", STATUS_DESCRIPTION, id_schema()),
            identity: identity("default"),
        }
    }
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, schema(&self.declaration)),
            identity: identity(variant),
        }
    }
}

impl Tool for WorkflowStatusTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::Delegates
    }
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let input: IdInput = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(outcome) => return outcome,
            };
            match self.service.status(&RunId(input.id.clone())).await {
                Ok(RunStatus::Running(progress)) => {
                    ToolOutcome::ok(render_progress(&input.id, &progress))
                }
                Ok(RunStatus::Ended(report)) => ToolOutcome::ok(report_line(&report)),
                Err(error) => id_error(&input.id, error),
            }
        })
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultInput {
    id: String,
    #[serde(default)]
    wait: bool,
}

/// Reads a retained report, or waits for it using the tool's cancellation token.
pub struct WorkflowResultTool {
    service: Arc<dyn WorkflowService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkflowResultTool {
    pub fn new(service: Arc<dyn WorkflowService>) -> Self {
        Self {
            service,
            declaration: declaration(
                "workflow_result",
                RESULT_DESCRIPTION,
                json!({"type":"object","properties":{
                    "id":{"type":"string","description":"Workflow id, e.g. wf1."},
                    "wait":{"type":"boolean","default":false,"description":"Wait for the run to end."}
                },"required":["id"],"additionalProperties":false}),
            ),
            identity: identity("default"),
        }
    }
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, schema(&self.declaration)),
            identity: identity(variant),
        }
    }
}

impl Tool for WorkflowResultTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::Delegates
    }
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let input: ResultInput = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(outcome) => return outcome,
            };
            let id = RunId(input.id.clone());
            let status = if input.wait {
                self.service.wait(&id, context.cancel.clone()).await
            } else {
                self.service.status(&id).await
            };
            match status {
                Ok(RunStatus::Running(_)) if input.wait => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                },
                Ok(RunStatus::Running(progress)) => {
                    ToolOutcome::ok(render_progress(&input.id, &progress))
                }
                Ok(RunStatus::Ended(report)) => ToolOutcome::ok(render_report(&report)),
                Err(error) => id_error(&input.id, error),
            }
        })
    }
}

/// Cancels a run; an already ended run is an idempotent success.
pub struct WorkflowCancelTool {
    service: Arc<dyn WorkflowService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkflowCancelTool {
    pub fn new(service: Arc<dyn WorkflowService>) -> Self {
        Self {
            service,
            declaration: declaration("workflow_cancel", CANCEL_DESCRIPTION, id_schema()),
            identity: identity("default"),
        }
    }
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, schema(&self.declaration)),
            identity: identity(variant),
        }
    }
}

impl Tool for WorkflowCancelTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::Delegates
    }
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let input: IdInput = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(outcome) => return outcome,
            };
            match self.service.cancel(&RunId(input.id.clone())).await {
                Ok(()) => ToolOutcome::ok(format!("Workflow {} cancelled.", input.id)),
                Err(error) => id_error(&input.id, error),
            }
        })
    }
}

fn id_schema() -> Value {
    json!({"type":"object","properties":{
        "id":{"type":"string","description":"Workflow id, e.g. wf1."}
    },"required":["id"],"additionalProperties":false})
}

fn schema(declaration: &ToolDeclaration) -> Value {
    let DeclarationKind::Function { input_schema } = &declaration.kind else {
        unreachable!("workflow tools always have function declarations")
    };
    input_schema.clone()
}

fn parse_input<T: DeserializeOwned>(tool: &str, call: &ToolCall) -> Result<T, ToolOutcome> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(ToolOutcome::error(format!(
                "Invalid input for {tool}: expected a JSON object input, got freeform text"
            )));
        }
    };
    serde_json::from_str(raw)
        .map_err(|error| ToolOutcome::error(format!("Invalid input for {tool}: {error}")))
}

fn start_error(error: WorkflowError) -> ToolOutcome {
    match error {
        WorkflowError::Parse {
            message,
            line,
            column,
        } => ToolOutcome::error(format!(
            "Script does not parse: {message} [line {line}, column {column}]"
        )),
        WorkflowError::Preflight(reason) | WorkflowError::Io(reason) => {
            ToolOutcome::error(format!("Cannot start workflow: {reason}"))
        }
        WorkflowError::ShutDown => {
            ToolOutcome::error("Cannot start workflow: the workflow service has shut down.")
        }
        WorkflowError::UnknownRun => {
            ToolOutcome::error("Cannot start workflow: no such workflow run")
        }
    }
}

fn id_error(id: &str, error: WorkflowError) -> ToolOutcome {
    match error {
        WorkflowError::UnknownRun => ToolOutcome::error(format!("No workflow {id}.")),
        other => ToolOutcome::error(other.to_string()),
    }
}

fn render_progress(id: &str, progress: &RunProgress) -> String {
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

fn report_line(report: &RunReport) -> String {
    let outcome = match report.outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::CompletedWithIssues => "completed with issues",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
    };
    let c = &report.counts;
    format!(
        "Workflow {}: {outcome} — {} steps ({} replayed): {} done, {} blocked, {} failed, {} cancelled; {} not verified; {} capped; {} invalid output",
        report.id.0,
        c.steps,
        c.replayed,
        c.done,
        c.blocked,
        c.failed,
        c.cancelled,
        c.not_verified,
        c.capped,
        c.invalid_output
    )
}

fn render_step(step: &StepLine) -> String {
    let status = match step.status {
        StepStatus::Done => "done",
        StepStatus::Blocked => "blocked",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
    };
    let mut line = format!(
        "  {} {} → {}",
        step.label.as_deref().unwrap_or(&step.call.0),
        step.role,
        step.model
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

fn render_report(report: &RunReport) -> String {
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
    let value =
        serde_json::to_string_pretty(&report.value).expect("a serde_json::Value always serializes");
    text.push_str("\nresult:\n");
    if value.len() > 16 * 1024 {
        let mut end = 16 * 1024;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&value[..end]);
        text.push_str(&format!(
            "… (truncated; full value in {}/result.json)",
            report.run_dir.display()
        ));
    } else {
        text.push_str(&value);
    }
    text.push_str(&format!("\nrun dir: {}", report.run_dir.display()));
    text
}
