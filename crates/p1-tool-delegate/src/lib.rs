//! The four model-facing delegation tools.
//!
//! They depend on the [`WorkerService`] trait only: the in-process implementation
//! is a host concern and nothing here knows about it. Each tool has its own
//! declaration, parses its own input and renders its own output. Invalid input is
//! an `Error` outcome the model can act on — never a panic.
//!
//! Descriptions tell the model the three facts that matter: a worker gets ONLY the
//! task text, workers share this workspace, and completion arrives as a
//! notification (so polling is pointless). `worker_result` adds: verify first.

use std::sync::Arc;

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, JournalRecord, RecordBody, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_workers::{ChildId, ChildSpec, ChildStatus, WorkerError, WorkerService};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// Model-facing name + description override, mirroring `p1-workspace::ToolFace`
/// without taking a dependency on it.
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

const START_NAME: &str = "worker_start";
const START_DESCRIPTION: &str = "Start a worker agent on an environment with a self-contained task.\nThe worker gets ONLY the task text — no conversation history — so the task must contain everything it needs.\nWorkers share this workspace: do not give two workers overlapping files.\nYou will be notified when it finishes; do not poll for it.";

const RESULT_NAME: &str = "worker_result";
const RESULT_DESCRIPTION: &str = "Read a worker's status and its final text.\nSet wait to true to block until the worker is no longer running (you can be cancelled while waiting).\nVerify the worker's result before relying on it.";

const CONTINUE_NAME: &str = "worker_continue";
const CONTINUE_DESCRIPTION: &str = "Send another message into a worker's session to repair or extend its work.\nFails while the worker is still running a turn.";

const CANCEL_NAME: &str = "worker_cancel";
const CANCEL_DESCRIPTION: &str =
    "Cancel a running worker's current turn. The worker's session is kept.";

/// How a successful `worker_start` result begins; [`workers_started_in`] reads it back.
const STARTED_PREFIX: &str = "Started worker ";

/// The ids of every worker a journalled session started, in order. Workers live in
/// the process that started them, so after a resume these ids name nothing — the
/// host uses this to say so and to keep new ids from colliding with them.
pub fn workers_started_in(records: &[JournalRecord]) -> Vec<String> {
    let mut delegate_calls = std::collections::HashSet::new();
    let mut ids = Vec::new();
    for record in records {
        match &record.body {
            RecordBody::ToolStarted { call_id, identity }
                if identity.implementation == env!("CARGO_PKG_NAME") =>
            {
                delegate_calls.insert(call_id.as_str());
            }
            RecordBody::ToolFinished { result }
                if result.status == ToolStatus::Ok
                    && delegate_calls.contains(result.call_id.as_str()) =>
            {
                let id = result
                    .content
                    .strip_prefix(STARTED_PREFIX)
                    .and_then(|rest| rest.split(' ').next());
                if let Some(id) = id {
                    ids.push(id.to_string());
                }
            }
            _ => {}
        }
    }
    ids
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

fn declaration(name: &str, description: &str, schema: serde_json::Value) -> ToolDeclaration {
    ToolDeclaration {
        name: name.to_string(),
        description: description.to_string(),
        kind: DeclarationKind::Function {
            input_schema: schema,
        },
    }
}

/// The four tools, all backed by one service. Order matches the spec table.
pub fn all(service: Arc<dyn WorkerService>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(WorkerStartTool::new(Arc::clone(&service))),
        Arc::new(WorkerResultTool::new(Arc::clone(&service))),
        Arc::new(WorkerContinueTool::new(Arc::clone(&service))),
        Arc::new(WorkerCancelTool::new(service)),
    ]
}

// ---------------------------------------------------------------- worker_start

/// `worker_start`: starts one worker NOW and reports where it runs.
pub struct WorkerStartTool {
    service: Arc<dyn WorkerService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerStartTool {
    pub fn new(service: Arc<dyn WorkerService>) -> Self {
        Self {
            service,
            declaration: declaration(START_NAME, START_DESCRIPTION, start_schema()),
            identity: identity("default"),
        }
    }

    /// Present the same implementation under another name/description/variant.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, start_schema()),
            identity: identity(variant),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    environment: String,
    task: String,
}

impl Tool for WorkerStartTool {
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
            let spec = ChildSpec {
                environment: input.environment,
                task: input.task,
                workspace: None,
            };
            match self.service.start(spec).await {
                Ok(id) => {
                    // The description is the factory's route/model, shown to the
                    // parent; a service that is gone cannot happen here.
                    let description = self
                        .service
                        .describe(&id)
                        .await
                        .unwrap_or_else(|_| String::new());
                    ToolOutcome::ok(format!(
                        "{STARTED_PREFIX}{} on {description}. You will be notified when it finishes.",
                        id.0
                    ))
                }
                Err(error) => start_error(error),
            }
        })
    }
}

// ---------------------------------------------------------------- worker_result

/// `worker_result`: retained status plus the final text, optionally waiting.
pub struct WorkerResultTool {
    service: Arc<dyn WorkerService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerResultTool {
    pub fn new(service: Arc<dyn WorkerService>) -> Self {
        Self {
            service,
            declaration: declaration(RESULT_NAME, RESULT_DESCRIPTION, result_schema()),
            identity: identity("default"),
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, result_schema()),
            identity: identity(variant),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ResultInput {
    id: String,
    #[serde(default)]
    wait: bool,
}

impl Tool for WorkerResultTool {
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
            let id = ChildId(input.id.clone());
            let status = if input.wait {
                // The tool's own cancel token is the wait's cancel: the service
                // reports `Running` when it fires first.
                match self.service.wait(&id, context.cancel.clone()).await {
                    Ok(ChildStatus::Running) => {
                        return ToolOutcome {
                            status: ToolStatus::Cancelled,
                            content: String::new(),
                        };
                    }
                    Ok(status) => status,
                    Err(error) => return id_error(&input.id, error),
                }
            } else {
                match self.service.status(&id).await {
                    Ok(status) => status,
                    Err(error) => return id_error(&input.id, error),
                }
            };
            ToolOutcome::ok(render_status(&input.id, &status))
        })
    }
}

// ---------------------------------------------------------------- worker_continue

/// `worker_continue`: another turn in the SAME child session.
pub struct WorkerContinueTool {
    service: Arc<dyn WorkerService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerContinueTool {
    pub fn new(service: Arc<dyn WorkerService>) -> Self {
        Self {
            service,
            declaration: declaration(CONTINUE_NAME, CONTINUE_DESCRIPTION, continue_schema()),
            identity: identity("default"),
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, continue_schema()),
            identity: identity(variant),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContinueInput {
    id: String,
    message: String,
}

impl Tool for WorkerContinueTool {
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
            let input: ContinueInput = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(outcome) => return outcome,
            };
            let id = ChildId(input.id.clone());
            match self.service.continue_child(&id, input.message).await {
                Ok(()) => ToolOutcome::ok(format!("Worker {} continues.", input.id)),
                Err(error) => id_error(&input.id, error),
            }
        })
    }
}

// ---------------------------------------------------------------- worker_cancel

/// `worker_cancel`: cancels the current turn; the session is retained.
pub struct WorkerCancelTool {
    service: Arc<dyn WorkerService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerCancelTool {
    pub fn new(service: Arc<dyn WorkerService>) -> Self {
        Self {
            service,
            declaration: declaration(CANCEL_NAME, CANCEL_DESCRIPTION, cancel_schema()),
            identity: identity("default"),
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            service: self.service,
            declaration: declaration(&face.name, &face.description, cancel_schema()),
            identity: identity(variant),
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct CancelInput {
    id: String,
}

impl Tool for WorkerCancelTool {
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
            let input: CancelInput = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(outcome) => return outcome,
            };
            let id = ChildId(input.id.clone());
            match self.service.cancel(&id).await {
                Ok(()) => ToolOutcome::ok(format!("Worker {} cancelled.", input.id)),
                Err(error) => id_error(&input.id, error),
            }
        })
    }
}

// ---------------------------------------------------------------- shared

fn parse_input<T: DeserializeOwned>(tool: &str, call: &ToolCall) -> Result<T, ToolOutcome> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(ToolOutcome::error(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            )));
        }
    };
    serde_json::from_str(raw).map_err(|error| ToolOutcome::error(invalid(tool, &error.to_string())))
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// A `worker_start` failure, with the exact texts the model can act on.
fn start_error(error: WorkerError) -> ToolOutcome {
    match error {
        WorkerError::LimitReached { max } => ToolOutcome::error(format!(
            "Cannot start another worker: {max} are already running."
        )),
        WorkerError::InvalidEnvironment(message) => {
            ToolOutcome::error(format!("Cannot start worker: {message}"))
        }
        WorkerError::ShutDown => {
            ToolOutcome::error("Cannot start worker: the worker service has shut down.")
        }
        // `start` never returns these; keep the mapping total and honest.
        other => ToolOutcome::error(other.to_string()),
    }
}

/// A failure addressed by worker id.
fn id_error(id: &str, error: WorkerError) -> ToolOutcome {
    match error {
        WorkerError::UnknownChild => ToolOutcome::error(format!("No worker {id}.")),
        WorkerError::Busy => ToolOutcome::error(format!("Worker {id} is still running.")),
        WorkerError::ShutDown => ToolOutcome::error(format!(
            "Worker {id} is unavailable: the service has shut down."
        )),
        // The remaining variants cannot occur for an existing id.
        other => ToolOutcome::error(other.to_string()),
    }
}

/// Status line, then the retained text: final text for `finished`, the failure
/// message for `failed`. `cancelled` retains no text, so it is the status line.
fn render_status(id: &str, status: &ChildStatus) -> String {
    match status {
        ChildStatus::Running => format!("Worker {id}: running"),
        ChildStatus::Finished(result) => format!("Worker {id}: finished\n\n{}", result.final_text),
        ChildStatus::Failed(message) => format!("Worker {id}: failed\n\n{message}"),
        ChildStatus::Cancelled => format!("Worker {id}: cancelled"),
    }
}

fn start_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "environment": {
                "type": "string",
                "description": "Environment (prompt, model and tools) the worker runs on."
            },
            "task": {
                "type": "string",
                "description": "The complete, self-contained task for the worker."
            }
        },
        "required": ["environment", "task"],
        "additionalProperties": false
    })
}

fn result_schema() -> serde_json::Value {
    serde_json::json!({
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

fn continue_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "id": {
                "type": "string",
                "description": "Worker id, e.g. \"w1\"."
            },
            "message": {
                "type": "string",
                "description": "The message to send into the worker's session."
            }
        },
        "required": ["id", "message"],
        "additionalProperties": false
    })
}

fn cancel_schema() -> serde_json::Value {
    serde_json::json!({
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
