//! The four model-facing delegation tools.
//!
//! Each depends only on its own worker traits (`WorkersStart`, `WorkersObserve`,
//! `WorkersControl`); a [`WorkerService`] reaches them through the unscoped adapter,
//! and the in-process implementation is a host concern nothing here knows about.
//! Each tool has its own declaration, parses its own input and renders its own
//! output. Invalid input is an `Error` outcome the model can act on — never a panic.
//!
//! Descriptions tell the model the four facts that matter: a worker gets ONLY the
//! task text, workers share this workspace, completion arrives as a notification
//! (so polling is pointless), and the worker has ONLY the tools the parent lists in
//! `tools`, plus `finish`. `worker_continue` adds: `add_tools` can give a worker the
//! tools it lacks, and it keeps its context. `worker_result` adds: verify first.

use std::sync::Arc;

pub use p1_contracts::tool::ToolFace;
use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, JournalRecord, RecordBody, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_workers::scope::UnscopedWorkers;
use p1_workers::{
    ChildId, ChildSpec, ChildStatus, WorkerError, WorkerReport, WorkerService, WorkersControl,
    WorkersObserve, WorkersStart,
};
use serde::Deserialize;
use serde::de::DeserializeOwned;

const START_NAME: &str = "worker_start";
const START_DESCRIPTION: &str = "Start a worker agent on an environment with a self-contained task.\nThe worker gets ONLY the task text — no conversation history — so the task must contain everything it needs.\nWorkers share this workspace: do not give two workers overlapping files.\nYou will be notified when it finishes; do not poll for it.\nThe worker has ONLY the tools you list in `tools` (plus finish); tools you do not list do not exist for it. List every tool the task needs; if you are unsure whether it needs one, include it.";

const RESULT_NAME: &str = "worker_result";
const RESULT_DESCRIPTION: &str = "Read a worker's status and its final text.\nSet wait to true to block until the worker is no longer running (you can be cancelled while waiting).\nVerify the worker's result before relying on it.";

const CONTINUE_NAME: &str = "worker_continue";
const CONTINUE_DESCRIPTION: &str = "Send another message into a worker's session to repair or extend its work.\nFails while the worker is still running a turn.\nUse add_tools to give the worker tools it lacks (e.g. after it finished blocked naming a missing tool); it keeps its context.";

const CANCEL_NAME: &str = "worker_cancel";
const CANCEL_DESCRIPTION: &str =
    "Cancel a running worker's current turn. The worker's session is kept.";

/// How a successful `worker_start` result begins; [`workers_started_in`] reads it back.
const STARTED_PREFIX: &str = "Started worker ";

fn plain_result(result: &ToolResultItem) -> ResultDescription {
    ResultDescription {
        summary: result.content.lines().next().unwrap_or_default().to_owned(),
        detail: None,
    }
}

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

/// The four tools over one service. Order matches the spec table.
///
/// `grantable` is the tool MODULE names a parent may grant (`worker_start`'s
/// `tools` enum) and `environments` the environment names it may run (its
/// `environment` enum). Both come from the host: this crate still names no
/// concrete tool, provider or environment.
///
/// A composition helper only: each member is built by its own constructor from the
/// unscoped adapter over `service`, so the members share nothing but the service they
/// all reached before, and each holds only its own traits.
pub fn all(
    service: Arc<dyn WorkerService>,
    grantable: Vec<String>,
    environments: Vec<String>,
) -> Vec<Arc<dyn Tool>> {
    let workers = Arc::new(UnscopedWorkers::new(Arc::clone(&service)));
    vec![
        Arc::new(WorkerStartTool::new(
            Arc::clone(&workers),
            grantable.clone(),
            environments,
        )),
        // The service still learns the result tool's name, as it did when this member
        // was built from the service itself.
        Arc::new(WorkerResultTool::new(
            ObserveSurface::new(Arc::clone(&workers) as Arc<dyn WorkersObserve>)
                .with_result_tool_name(move |name| service.set_result_tool_name(name)),
        )),
        // `worker_continue` can ADD to a worker's grant, so it carries the same
        // grantable list `worker_start` does.
        Arc::new(WorkerContinueTool::new(Arc::clone(&workers), grantable)),
        Arc::new(WorkerCancelTool::new(workers)),
    ]
}

// ---------------------------------------------------------------- member surfaces
//
// What each member is constructed from. A surface holds ONLY its member's worker
// traits, so a member can call nothing else: `WorkerResultTool` has no start surface to
// call, and a test builds it from a fake that implements `WorkersObserve` alone.
//
// Each surface converts from any `Arc` of a type implementing its traits (a
// `WorkerScope`, a test fake, the unscoped adapter) and from `Arc<dyn WorkerService>`.
// The latter goes through `UnscopedWorkers`, so the host's current call sites, which
// pass their one service, keep today's unscoped behaviour exactly until S6.7 hands the
// members scopes instead.

/// `worker_start`'s surface: `WorkersStart`, plus `WorkersObserve` used for exactly one
/// call, `describe` of the child the member has just started, so the success text keeps
/// naming the child's route and model (lead decision, option (a)).
pub struct StartSurface {
    start: Arc<dyn WorkersStart>,
    observe: Arc<dyn WorkersObserve>,
}

impl StartSurface {
    /// The two traits from separate objects, for a host that links them apart.
    pub fn new(start: Arc<dyn WorkersStart>, observe: Arc<dyn WorkersObserve>) -> Self {
        Self { start, observe }
    }
}

impl<T: WorkersStart + WorkersObserve + 'static> From<Arc<T>> for StartSurface {
    fn from(workers: Arc<T>) -> Self {
        Self::new(Arc::clone(&workers) as Arc<dyn WorkersStart>, workers)
    }
}

impl From<Arc<dyn WorkerService>> for StartSurface {
    fn from(service: Arc<dyn WorkerService>) -> Self {
        Arc::new(UnscopedWorkers::new(service)).into()
    }
}

/// The `WorkerService::set_result_tool_name` hook, as a function: it can name the
/// result tool and do nothing else, so carrying it gives `worker_result` no reach into
/// the service's other operations.
type ResultToolNameHook = Arc<dyn Fn(&str) + Send + Sync>;

/// `worker_result`'s surface: `WorkersObserve`, and optionally the hook that points
/// the service's completion notification at the tool's name (ADR-0057).
///
/// The hook stays where it was: `WorkerResultTool::new` calls it with `worker_result`
/// and `with_face` with the face's name. It is installed by the conversion from
/// `Arc<dyn WorkerService>` (the host's path) and by [`all`]; a surface without it (a
/// scope, a fake) names no tool, which is what a service without a delegate tool did.
pub struct ObserveSurface {
    observe: Arc<dyn WorkersObserve>,
    result_tool_name: Option<ResultToolNameHook>,
}

impl ObserveSurface {
    pub fn new(observe: Arc<dyn WorkersObserve>) -> Self {
        Self {
            observe,
            result_tool_name: None,
        }
    }

    /// Install the hook the member calls with its model-facing name.
    pub fn with_result_tool_name(mut self, hook: impl Fn(&str) + Send + Sync + 'static) -> Self {
        self.result_tool_name = Some(Arc::new(hook));
        self
    }

    fn name_result_tool(&self, name: &str) {
        if let Some(hook) = &self.result_tool_name {
            hook(name);
        }
    }
}

impl<T: WorkersObserve + 'static> From<Arc<T>> for ObserveSurface {
    fn from(observe: Arc<T>) -> Self {
        Self::new(observe)
    }
}

impl From<Arc<dyn WorkerService>> for ObserveSurface {
    fn from(service: Arc<dyn WorkerService>) -> Self {
        Self::new(Arc::new(UnscopedWorkers::new(Arc::clone(&service))))
            .with_result_tool_name(move |name| service.set_result_tool_name(name))
    }
}

/// `worker_continue`'s and `worker_cancel`'s surface: `WorkersControl` alone.
pub struct ControlSurface {
    control: Arc<dyn WorkersControl>,
}

impl ControlSurface {
    pub fn new(control: Arc<dyn WorkersControl>) -> Self {
        Self { control }
    }
}

impl<T: WorkersControl + 'static> From<Arc<T>> for ControlSurface {
    fn from(control: Arc<T>) -> Self {
        Self::new(control)
    }
}

impl From<Arc<dyn WorkerService>> for ControlSurface {
    fn from(service: Arc<dyn WorkerService>) -> Self {
        Self::new(Arc::new(UnscopedWorkers::new(service)))
    }
}

// ---------------------------------------------------------------- worker_start

/// `worker_start`: starts one worker NOW and reports where it runs.
pub struct WorkerStartTool {
    /// `WorkersStart` and, for `describe` of the started child only, `WorkersObserve`.
    workers: StartSurface,
    /// Tool module names a worker may be granted; the schema's `tools` enum and
    /// the list `execute` validates against. Never contains `finish` or `worker_*`.
    grantable: Vec<String>,
    /// Environment names a worker may run; the schema's `environment` enum.
    environments: Vec<String>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerStartTool {
    pub fn new(
        workers: impl Into<StartSurface>,
        grantable: Vec<String>,
        environments: Vec<String>,
    ) -> Self {
        Self {
            workers: workers.into(),
            declaration: declaration(
                START_NAME,
                START_DESCRIPTION,
                start_schema(&grantable, &environments),
            ),
            identity: identity("default"),
            grantable,
            environments,
        }
    }

    /// Present the same implementation under another name/description/variant.
    /// Both lists are kept: the face changes only how the model sees the tool.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        let declaration = declaration(
            &face.name,
            &face.description,
            start_schema(&self.grantable, &self.environments),
        );
        Self {
            workers: self.workers,
            grantable: self.grantable,
            environments: self.environments,
            declaration,
            identity: identity(variant),
        }
    }

    /// The grantable modules joined for a model-readable message.
    fn grantable_list(&self) -> String {
        self.grantable.join(", ")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct StartInput {
    environment: String,
    task: String,
    /// `serde(default)` so a missing `tools` reaches `execute` as an empty list and
    /// gets the actionable "`tools` is required" message rather than a serde error.
    #[serde(default)]
    tools: Vec<String>,
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

    /// ADR-0057: the environment this call starts a worker on.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "worker",
            target: parse_input::<StartInput>(&self.declaration.name, call)
                .ok()
                .map(|input| input.environment),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let mut description = plain_result(result);
        if result.status == ToolStatus::Ok
            && let Ok(input) = parse_input::<StartInput>(&self.declaration.name, call)
        {
            let mut grants = input.tools;
            grants.push("finish".into());
            description.summary = format!("started · {}", grants.join(", "));
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
            description.detail = Some(ResultDetail::Text(lines.join("\n")));
        }
        description
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
            // Models do not always honour the schema, so the rules are enforced here
            // too. An empty or missing grant is the same refusal, and nothing is
            // started until every module is known and the grant is non-empty.
            let mut tools = Vec::with_capacity(input.tools.len());
            for module in input.tools {
                if !self.grantable.contains(&module) {
                    return ToolOutcome::error(format!(
                        "Cannot start worker: `{module}` is not a tool module a worker can be \
                         granted. Valid tools: {}",
                        self.grantable_list()
                    ));
                }
                // Duplicates are removed, keeping the first occurrence's order.
                if !tools.contains(&module) {
                    tools.push(module);
                }
            }
            if tools.is_empty() {
                return ToolOutcome::error(format!(
                    "`tools` is required: list every tool module the worker needs, from: {}",
                    self.grantable_list()
                ));
            }
            let spec = ChildSpec {
                environment: input.environment,
                task: input.task,
                tools: tools.clone(),
                workspace: None,
            };
            match self.workers.start.start(spec).await {
                Ok(id) => {
                    // The description is the factory's route/model, shown to the
                    // parent; a service that is gone cannot happen here. This is the
                    // member's only observe call, and only for the id it just got.
                    let description = self
                        .workers
                        .observe
                        .describe(&id)
                        .await
                        .unwrap_or_else(|_| String::new());
                    // Every worker also gets `finish`, so the grant named here is the
                    // grant plus it — the same list the worker's own prompt carries.
                    tools.push("finish".to_string());
                    ToolOutcome::ok(format!(
                        "{STARTED_PREFIX}{} on {description} with tools: {}. You will be notified \
                         when it finishes.",
                        id.0,
                        tools.join(", ")
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
    workers: ObserveSurface,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerResultTool {
    pub fn new(workers: impl Into<ObserveSurface>) -> Self {
        let workers = workers.into();
        // The service points its completion notification at this tool's name
        // (ADR-0057); the default face is `worker_result`.
        workers.name_result_tool(RESULT_NAME);
        Self {
            workers,
            declaration: declaration(RESULT_NAME, RESULT_DESCRIPTION, result_schema()),
            identity: identity("default"),
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        // A renamed face moves the notification's tool name too (ADR-0057).
        self.workers.name_result_tool(&face.name);
        Self {
            workers: self.workers,
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

    /// ADR-0057: the worker this call reads.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "worker",
            target: parse_input::<ResultInput>(&self.declaration.name, call)
                .ok()
                .map(|input| input.id),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(&self, _call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let mut description = plain_result(result);
        if result.status == ToolStatus::Ok {
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
            description.summary = status.map_or_else(
                || format!("{lines} lines"),
                |word| format!("{word} · {lines} lines"),
            );
            description.detail = result.content.split_once("\n---\n").map(|(report, _)| {
                ResultDetail::Text(format!(
                    "lines\t{}",
                    report.lines().take(8).collect::<Vec<_>>().join("\n")
                ))
            });
        }
        description
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
                match self.workers.observe.wait(&id, context.cancel.clone()).await {
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
                match self.workers.observe.status(&id).await {
                    Ok(status) => status,
                    Err(error) => return id_error(&input.id, error),
                }
            };
            ToolOutcome::ok(render_status(&input.id, &status))
        })
    }
}

// ---------------------------------------------------------------- worker_continue

/// `worker_continue`: another turn in the SAME child session, optionally with a
/// larger tool grant (ADR-0050 item 6).
pub struct WorkerContinueTool {
    workers: ControlSurface,
    /// Tool module names a worker may be granted; the schema's `add_tools` enum and
    /// the list `execute` validates against. Never contains `finish` or `worker_*`.
    grantable: Vec<String>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerContinueTool {
    pub fn new(workers: impl Into<ControlSurface>, grantable: Vec<String>) -> Self {
        Self {
            workers: workers.into(),
            declaration: declaration(
                CONTINUE_NAME,
                CONTINUE_DESCRIPTION,
                continue_schema(&grantable),
            ),
            identity: identity("default"),
            grantable,
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        let declaration = declaration(
            &face.name,
            &face.description,
            continue_schema(&self.grantable),
        );
        Self {
            workers: self.workers,
            grantable: self.grantable,
            declaration,
            identity: identity(variant),
        }
    }

    /// The grantable modules joined for a model-readable message.
    fn grantable_list(&self) -> String {
        self.grantable.join(", ")
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ContinueInput {
    id: String,
    message: String,
    /// `serde(default)` so a continue without added tools is the ordinary repair.
    #[serde(default)]
    add_tools: Vec<String>,
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

    /// ADR-0057: the worker this call continues.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "worker",
            target: parse_input::<ContinueInput>(&self.declaration.name, call)
                .ok()
                .map(|input| {
                    if input.add_tools.is_empty() {
                        input.id
                    } else {
                        format!("{} +{}", input.id, input.add_tools.join(" +"))
                    }
                }),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let mut description = plain_result(result);
        if result.status == ToolStatus::Ok {
            let grants = parse_input::<ContinueInput>(&self.declaration.name, call)
                .map(|input| input.add_tools)
                .unwrap_or_default();
            description.summary = if grants.is_empty() {
                "resumed".into()
            } else {
                format!("resumed · +{}", grants.join(" +"))
            };
        }
        description
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
            // Models do not always honour the schema, so the rules are enforced here
            // too: an unknown module is refused with the valid list, and nothing is
            // sent to the worker.
            let mut add_tools = Vec::with_capacity(input.add_tools.len());
            for module in input.add_tools {
                if !self.grantable.contains(&module) {
                    return ToolOutcome::error(format!(
                        "Cannot add tools to worker {}: `{module}` is not a tool module a worker \
                         can be granted. Valid tools: {}",
                        input.id,
                        self.grantable_list()
                    ));
                }
                // Duplicates are removed, keeping the first occurrence's order.
                if !add_tools.contains(&module) {
                    add_tools.push(module);
                }
            }
            match self
                .workers
                .control
                .continue_child(&id, input.message, add_tools.clone())
                .await
            {
                Ok(()) if add_tools.is_empty() => {
                    ToolOutcome::ok(format!("Worker {} continues.", input.id))
                }
                Ok(()) => ToolOutcome::ok(format!(
                    "Added tools: {}. Message sent to worker {}.",
                    add_tools.join(", "),
                    input.id
                )),
                Err(error) => id_error(&input.id, error),
            }
        })
    }
}

// ---------------------------------------------------------------- worker_cancel

/// `worker_cancel`: cancels the current turn; the session is retained.
pub struct WorkerCancelTool {
    workers: ControlSurface,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl WorkerCancelTool {
    pub fn new(workers: impl Into<ControlSurface>) -> Self {
        Self {
            workers: workers.into(),
            declaration: declaration(CANCEL_NAME, CANCEL_DESCRIPTION, cancel_schema()),
            identity: identity("default"),
        }
    }

    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            workers: self.workers,
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

    /// ADR-0057: the worker this call cancels.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "worker",
            target: parse_input::<CancelInput>(&self.declaration.name, call)
                .ok()
                .map(|input| input.id),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(&self, _call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let mut description = plain_result(result);
        if result.status == ToolStatus::Ok {
            description.summary = "cancelled".into();
        }
        description
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
            match self.workers.control.cancel(&id).await {
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
        // The re-grant was refused: no turn ran and the worker kept its tools, so the
        // parent can act on the reason (ADR-0050 item 6).
        WorkerError::Regrant(reason) => ToolOutcome::error(format!(
            "Cannot add tools to worker {id}: {reason}. The worker keeps its tools."
        )),
        WorkerError::ShutDown => ToolOutcome::error(format!(
            "Worker {id} is unavailable: the service has shut down."
        )),
        // The remaining variants cannot occur for an existing id.
        other => ToolOutcome::error(other.to_string()),
    }
}

/// Status line, then the retained text: final text for `finished`, the failure
/// message for `failed`. `cancelled` retains no text, so it is the status line.
///
/// A FINISHED worker's result begins with its report (ADR-0050 item 6): the tools it
/// was assembled with, its `finish`, and every call it made to a tool it was not
/// given — then a `---` line, then today's status line and text. A short-handed
/// worker is therefore visible whatever the parent does with the result.
fn render_status(id: &str, status: &ChildStatus) -> String {
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
/// evidence (ADR-0051 item 3) is appended to the status line — the accepted outcome's
/// own words, so a `done` the worker could not verify says so here.
fn render_report(report: &WorkerReport) -> String {
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

fn start_schema(grantable: &[String], environments: &[String]) -> serde_json::Value {
    serde_json::json!({
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

fn continue_schema(grantable: &[String]) -> serde_json::Value {
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
