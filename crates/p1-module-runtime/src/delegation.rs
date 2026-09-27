//! The host side of the worker and workflow capabilities (S6, B-S6-8, D065): the linkers of
//! `workers-start`, `workers-observe`, `workers-control` and `workflows`
//! (`modules/wit/delegation.wit`) over S6's native traits.
//!
//! - The services are passed in explicitly through [`Services`](crate::Services): there is no
//!   registry. Each linker captures the service it was given, so a module instance reaches
//!   exactly the scope the caller built for it and nothing else. Which scope that is (the
//!   (generation, operation, parent) of a `p1_workers::WorkerScope`) is the host's choice;
//!   this file never widens it: an id the scope did not start is the scope's own
//!   `unknown-child`, passed through as the WIT variant.
//! - Workers are split into three optional trait objects ([`WorkerServices`]) because the WIT
//!   splits them into three capabilities: a member that only reads results (`worker_result`)
//!   can be given `workers-observe` alone, and a grant without its service is refused as
//!   `MissingService` when the linker is built. Workflows are one WIT interface, so
//!   [`WorkflowServices`] holds all three of its traits; a host that grants a member only some
//!   operations answers the others itself (the per-member refusing adapter).
//! - Values cross as dynamic [`Val`]s, as in `capabilities.rs`: wasmtime's typed derives
//!   expand to `unsafe impl`s, which this crate forbids. A value the guest sends that is not
//!   the WIT shape is a trap (the bindings cannot produce one); every answer the service gives
//!   is mapped to its WIT form, errors included.
//! - `wait` answers `running` when the call is cancelled first, whatever the service does
//!   with the token: the WIT contract is the runtime's to keep. The other calls are not raced
//!   against the cancellation, so a start the service already made is always reported with
//!   its id instead of leaving a child nobody can name.

use std::sync::Arc;

use p1_contracts::serde_json::{self, Value};
use p1_contracts::{BoxFuture, CancellationToken, StopReason, TurnEnd, Usage};
use p1_module_protocol::{WireProviderError, WireUsage};
use p1_workers::{
    ChildId, ChildResult, ChildSpec, ChildStatus, FinishReport, WorkerError, WorkerReport,
    WorkersControl, WorkersObserve, WorkersStart,
};
use p1_workflow::{
    CancelRuns, ObserveRuns, RunId, RunStatus, StartRequest, StartRuns, WorkflowError,
};
use wasmtime::bail;
use wasmtime::component::{Linker, LinkerInstance, Val};

use crate::capabilities::CallState;
use crate::loader::interface_import;

/// The type-only interface the three worker interfaces take their records from. It grants
/// nothing, but a component that uses those records imports it, so it is linked (empty)
/// with any of them.
pub(crate) const WORKER_TYPES_INTERFACE: &str = "worker-types";

/// The worker services of one module instance, one per WIT interface. `None` withholds that
/// interface: a manifest that grants it then fails to link with `MissingService`.
#[derive(Clone, Default)]
pub struct WorkerServices {
    /// `workers-start`.
    pub start: Option<Arc<dyn WorkersStart>>,
    /// `workers-observe`.
    pub observe: Option<Arc<dyn WorkersObserve>>,
    /// `workers-control`.
    pub control: Option<Arc<dyn WorkersControl>>,
    /// `workers-observe.grantable` and `workers-observe.environments` (D084).
    pub lists: WorkerLists,
}

impl WorkerServices {
    /// All three interfaces over one scope, e.g. a `p1_workers::WorkerScope`: what the
    /// members of one family share for one parent.
    pub fn scoped<S>(scope: S) -> Self
    where
        S: WorkersStart + WorkersObserve + WorkersControl + 'static,
    {
        let scope = Arc::new(scope);
        Self {
            start: Some(scope.clone()),
            observe: Some(scope.clone()),
            control: Some(scope),
            lists: WorkerLists::default(),
        }
    }

    /// The same services, answering the two list functions with `lists`.
    pub fn with_lists(mut self, lists: WorkerLists) -> Self {
        self.lists = lists;
        self
    }
}

/// What a parent's children may be given, as `workers-observe` lists it (D084): the values
/// the host fixes when it assembles the parent's members. They travel with the member's
/// services rather than through `p1_workers`' traits because they are the host's catalog
/// knowledge, not the worker service's, and because the restricted path, which links no
/// service, must answer them too (D085).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WorkerLists {
    /// The tool module names a child may be granted, in the host's order.
    pub grantable: Vec<String>,
    /// The environment names a child may run on, in the host's order.
    pub environments: Vec<String>,
}

/// Links `workers-observe.grantable` and `workers-observe.environments` to `lists`, into a
/// linker of any store: the execution linker (next to the rest of `workers-observe`) and the
/// restricted one (D085). Both are synchronous and answer copies of fixed data: they take no
/// argument, have no effect and cannot block, so they are safe on the restricted path, where
/// nothing may wait. Nothing else of a worker interface is defined here.
pub(crate) fn link_worker_lists<T: 'static>(
    linker: &mut Linker<T>,
    lists: &WorkerLists,
) -> wasmtime::Result<()> {
    let mut instance = linker.instance(&interface_import("workers-observe"))?;
    let grantable = lists.grantable.clone();
    instance.func_wrap("grantable", move |_, (): ()| Ok((grantable.clone(),)))?;
    let environments = lists.environments.clone();
    instance.func_wrap("environments", move |_, (): ()| Ok((environments.clone(),)))
}

/// The services behind the `workflows` interface of one module instance.
#[derive(Clone)]
pub struct WorkflowServices {
    /// `workflows.start`.
    pub start: Arc<dyn StartRuns>,
    /// `workflows.status` and `workflows.wait`.
    pub observe: Arc<dyn ObserveRuns>,
    /// `workflows.cancel`.
    pub cancel: Arc<dyn CancelRuns>,
}

impl WorkflowServices {
    /// All three over one service, e.g. the host's `WorkflowService` or a per-member adapter.
    pub fn of<S>(service: Arc<S>) -> Self
    where
        S: StartRuns + ObserveRuns + CancelRuns + 'static,
    {
        Self {
            start: service.clone(),
            observe: service.clone(),
            cancel: service,
        }
    }
}

/// One host function's work once its parameters are read: a future owning everything it
/// needs, so the guest's parameter slice is not held across the service call.
type Answer = BoxFuture<'static, wasmtime::Result<Val>>;

/// Defines `name` in `instance` as a call of `call` on `service` with the call's
/// cancellation; the one result is the future's value.
fn define<S: ?Sized + Send + Sync + 'static>(
    instance: &mut LinkerInstance<'_, CallState>,
    name: &str,
    service: Arc<S>,
    call: fn(Arc<S>, &[Val], CancellationToken) -> Answer,
) -> wasmtime::Result<()> {
    instance.func_new_async(name, move |store, _ty, params, results| {
        let answer = call(service.clone(), params, store.data().cancel.clone());
        Box::new(async move {
            results[0] = answer.await?;
            Ok(())
        })
    })
}

/// Opens the interface `interface`, linking the empty `worker-types` first: the worker
/// interfaces' records come from it.
fn worker_interface<'a>(
    linker: &'a mut Linker<CallState>,
    interface: &str,
) -> wasmtime::Result<LinkerInstance<'a, CallState>> {
    // Re-opening an instance is allowed, so each worker linker may do this.
    linker.instance(&interface_import(WORKER_TYPES_INTERFACE))?;
    linker.instance(&interface_import(interface))
}

/// Links `workers-start` to `start`. Called by `capability_linker` only when the capability
/// is granted and the service given.
pub(crate) fn link_workers_start(
    linker: &mut Linker<CallState>,
    start: Arc<dyn WorkersStart>,
) -> wasmtime::Result<()> {
    let mut instance = worker_interface(linker, "workers-start")?;
    define(&mut instance, "start", start, start_worker)
}

/// Links `workers-observe` to `observe`.
pub(crate) fn link_workers_observe(
    linker: &mut Linker<CallState>,
    observe: Arc<dyn WorkersObserve>,
) -> wasmtime::Result<()> {
    let mut instance = worker_interface(linker, "workers-observe")?;
    define(&mut instance, "describe", observe.clone(), describe_worker)?;
    define(&mut instance, "status", observe.clone(), worker_status)?;
    define(&mut instance, "wait", observe, wait_worker)
}

/// Links `workers-control` to `control`.
pub(crate) fn link_workers_control(
    linker: &mut Linker<CallState>,
    control: Arc<dyn WorkersControl>,
) -> wasmtime::Result<()> {
    let mut instance = worker_interface(linker, "workers-control")?;
    define(&mut instance, "cancel", control.clone(), cancel_worker)?;
    define(&mut instance, "continue-child", control, continue_worker)
}

/// Links `workflows` to `services`.
pub(crate) fn link_workflows(
    linker: &mut Linker<CallState>,
    services: WorkflowServices,
) -> wasmtime::Result<()> {
    let mut instance = linker.instance(&interface_import("workflows"))?;
    define(&mut instance, "start", services.start, start_run)?;
    define(
        &mut instance,
        "status",
        services.observe.clone(),
        run_status,
    )?;
    define(&mut instance, "wait", services.observe, wait_run)?;
    define(&mut instance, "cancel", services.cancel, cancel_run)
}

// ---------------------------------------------------------------- worker calls

fn start_worker(start: Arc<dyn WorkersStart>, params: &[Val], _: CancellationToken) -> Answer {
    let spec = child_spec(params.first());
    Box::pin(async move {
        let answer = start.start(spec?).await;
        Ok(worker_result(answer.map(|id| Some(Val::String(id.0)))))
    })
}

fn describe_worker(
    observe: Arc<dyn WorkersObserve>,
    params: &[Val],
    _: CancellationToken,
) -> Answer {
    let id = child_id(params, "describe");
    Box::pin(async move {
        let id = id?;
        let answer = observe.describe(&id).await;
        Ok(worker_result(answer.map(|text| Some(Val::String(text)))))
    })
}

fn worker_status(observe: Arc<dyn WorkersObserve>, params: &[Val], _: CancellationToken) -> Answer {
    let id = child_id(params, "status");
    Box::pin(async move {
        let id = id?;
        let answer = observe.status(&id).await;
        Ok(worker_result(
            answer.map(|status| Some(child_status_val(status))),
        ))
    })
}

fn wait_worker(
    observe: Arc<dyn WorkersObserve>,
    params: &[Val],
    cancel: CancellationToken,
) -> Answer {
    let id = child_id(params, "wait");
    Box::pin(async move {
        let id = id?;
        // The service's answer first: an out-of-scope id is `unknown-child` even for a
        // cancelled call, and a finished child is reported as finished.
        let answer = tokio::select! {
            biased;
            answer = observe.wait(&id, cancel.clone()) => answer,
            () = cancel.cancelled() => Ok(ChildStatus::Running),
        };
        Ok(worker_result(
            answer.map(|status| Some(child_status_val(status))),
        ))
    })
}

fn cancel_worker(control: Arc<dyn WorkersControl>, params: &[Val], _: CancellationToken) -> Answer {
    let id = child_id(params, "cancel");
    Box::pin(async move {
        let id = id?;
        let answer = control.cancel(&id).await;
        Ok(worker_result(answer.map(|()| None)))
    })
}

fn continue_worker(
    control: Arc<dyn WorkersControl>,
    params: &[Val],
    _: CancellationToken,
) -> Answer {
    let request = (|| {
        let id = child_id(params, "continue-child")?;
        let message = string(params.get(1), "continue-child: message")?;
        let add_tools = strings(params.get(2), "continue-child: add-tools")?;
        Ok::<_, wasmtime::Error>((id, message, add_tools))
    })();
    Box::pin(async move {
        let (id, message, add_tools) = request?;
        let answer = control.continue_child(&id, message, add_tools).await;
        Ok(worker_result(answer.map(|()| None)))
    })
}

// ---------------------------------------------------------------- workflow calls

fn start_run(start: Arc<dyn StartRuns>, params: &[Val], _: CancellationToken) -> Answer {
    let request = start_request(params.first());
    Box::pin(async move {
        let answer = match request? {
            Ok(request) => start.start(request).await,
            Err(refused) => Err(refused),
        };
        Ok(workflow_result(answer.map(|id| Some(Val::String(id.0)))))
    })
}

fn run_status(observe: Arc<dyn ObserveRuns>, params: &[Val], _: CancellationToken) -> Answer {
    let id = run_id(params, "status");
    Box::pin(async move {
        let id = id?;
        let answer = observe.status(&id).await;
        Ok(workflow_result(
            answer.map(|status| Some(run_status_val(&status))),
        ))
    })
}

fn wait_run(observe: Arc<dyn ObserveRuns>, params: &[Val], cancel: CancellationToken) -> Answer {
    let id = run_id(params, "wait");
    Box::pin(async move {
        let id = id?;
        // As for workers: the service's answer wins a tie, and a cancellation that comes
        // first is answered with the run's status now, which is the running one.
        let answer = tokio::select! {
            biased;
            answer = observe.wait(&id, cancel.clone()) => answer,
            () = cancel.cancelled() => observe.status(&id).await,
        };
        Ok(workflow_result(
            answer.map(|status| Some(run_status_val(&status))),
        ))
    })
}

fn cancel_run(cancel: Arc<dyn CancelRuns>, params: &[Val], _: CancellationToken) -> Answer {
    let id = run_id(params, "cancel");
    Box::pin(async move {
        let id = id?;
        let answer = cancel.cancel(&id).await;
        Ok(workflow_result(answer.map(|()| None)))
    })
}

// ---------------------------------------------------------------- guest values

fn string(value: Option<&Val>, what: &str) -> wasmtime::Result<String> {
    match value {
        Some(Val::String(text)) => Ok(text.clone()),
        _ => bail!("{what} is not a string"),
    }
}

fn strings(value: Option<&Val>, what: &str) -> wasmtime::Result<Vec<String>> {
    let Some(Val::List(items)) = value else {
        bail!("{what} is not a list");
    };
    items.iter().map(|item| string(Some(item), what)).collect()
}

fn child_id(params: &[Val], function: &str) -> wasmtime::Result<ChildId> {
    string(params.first(), &format!("{function}: the id")).map(ChildId)
}

fn run_id(params: &[Val], function: &str) -> wasmtime::Result<RunId> {
    string(params.first(), &format!("workflows.{function}: the id")).map(RunId)
}

/// `worker-types.child-spec`. The WIT has no workspace: the host's own applies.
fn child_spec(value: Option<&Val>) -> wasmtime::Result<ChildSpec> {
    let Some(Val::Record(fields)) = value else {
        bail!("workers-start.start: the spec is not a record");
    };
    let mut environment = None;
    let mut task = None;
    let mut tools = None;
    for (name, value) in fields {
        match name.as_str() {
            "environment" => environment = Some(string(Some(value), "child-spec.environment")?),
            "task" => task = Some(string(Some(value), "child-spec.task")?),
            "tools" => tools = Some(strings(Some(value), "child-spec.tools")?),
            _ => bail!("workers-start.start: unexpected spec field {name}"),
        }
    }
    match (environment, task, tools) {
        (Some(environment), Some(task), Some(tools)) => Ok(ChildSpec {
            environment,
            task,
            tools,
            workspace: None,
        }),
        _ => bail!("workers-start.start: the spec is missing a field"),
    }
}

/// `workflows.start-request`. A malformed record traps; `args` that are not a JSON object
/// are refused as `preflight`, which is the WIT's "nothing started" answer, before the
/// service is asked. What the native request has beyond the WIT (role models, workspace,
/// base) is the host's to fill in, so it stays empty here.
fn start_request(value: Option<&Val>) -> wasmtime::Result<Result<StartRequest, WorkflowError>> {
    let Some(Val::Record(fields)) = value else {
        bail!("workflows.start: the request is not a record");
    };
    let mut script = None;
    let mut args = None;
    let mut resume_from = None;
    for (name, value) in fields {
        match (name.as_str(), value) {
            ("script", value) => script = Some(string(Some(value), "start-request.script")?),
            ("args", value) => args = Some(string(Some(value), "start-request.args")?),
            ("resume-from", Val::Option(id)) => {
                resume_from = Some(match id.as_deref() {
                    None => None,
                    Some(id) => Some(RunId(string(Some(id), "start-request.resume-from")?)),
                });
            }
            _ => bail!("workflows.start: unexpected request field {name}"),
        }
    }
    let (Some(script), Some(args), Some(resume_from)) = (script, args, resume_from) else {
        bail!("workflows.start: the request is missing a field");
    };
    let args = match serde_json::from_str::<Value>(&args) {
        Ok(args @ Value::Object(_)) => args,
        Ok(_) => {
            return Ok(Err(WorkflowError::Preflight(
                "args is not a JSON object".to_owned(),
            )));
        }
        Err(error) => {
            return Ok(Err(WorkflowError::Preflight(format!(
                "args is not JSON: {error}"
            ))));
        }
    };
    Ok(Ok(StartRequest {
        script,
        args,
        resume_from,
        role_models: Default::default(),
        workspace: None,
        base: None,
    }))
}

// ---------------------------------------------------------------- host answers

fn ok(payload: Option<Val>) -> Val {
    Val::Result(Ok(payload.map(Box::new)))
}

fn err(error: Val) -> Val {
    Val::Result(Err(Some(Box::new(error))))
}

fn case(name: &str, payload: Option<Val>) -> Val {
    Val::Variant(name.to_owned(), payload.map(Box::new))
}

fn worker_result(answer: Result<Option<Val>, WorkerError>) -> Val {
    match answer {
        Ok(payload) => ok(payload),
        Err(error) => err(worker_error_val(error)),
    }
}

fn workflow_result(answer: Result<Option<Val>, WorkflowError>) -> Val {
    match answer {
        Ok(payload) => ok(payload),
        Err(error) => err(workflow_error_val(error)),
    }
}

/// `worker-types.worker-error`.
fn worker_error_val(error: WorkerError) -> Val {
    match error {
        WorkerError::UnknownChild => case("unknown-child", None),
        WorkerError::Busy => case("busy", None),
        // A bound beyond u64 cannot be configured on any platform p1 builds for.
        WorkerError::LimitReached { max } => case(
            "limit-reached",
            Some(Val::U64(u64::try_from(max).unwrap_or(u64::MAX))),
        ),
        WorkerError::InvalidEnvironment(reason) => {
            case("invalid-environment", Some(Val::String(reason)))
        }
        WorkerError::IdsExhausted => case("ids-exhausted", None),
        WorkerError::Regrant(reason) => case("regrant", Some(Val::String(reason))),
        WorkerError::ShutDown => case("shut-down", None),
    }
}

/// `workflows.workflow-error`.
fn workflow_error_val(error: WorkflowError) -> Val {
    match error {
        WorkflowError::UnknownRun => case("unknown-run", None),
        WorkflowError::Parse {
            message,
            line,
            column,
        } => case(
            "parse",
            Some(Val::Record(vec![
                ("message".to_owned(), Val::String(message)),
                ("line".to_owned(), Val::U32(line)),
                ("column".to_owned(), Val::U32(column)),
            ])),
        ),
        WorkflowError::Preflight(reason) => case("preflight", Some(Val::String(reason))),
        WorkflowError::ShutDown => case("shut-down", None),
        WorkflowError::Io(reason) => case("io", Some(Val::String(reason))),
    }
}

/// `workflows.run-status`: `RunStatus` in its serde form, the schema `p1-workflow` owns.
fn run_status_val(status: &RunStatus) -> Val {
    // The status holds only strings, numbers, paths and JSON values, so it always
    // serializes; an empty text would be the guest's invalid input, never a trap here.
    Val::String(serde_json::to_string(status).unwrap_or_default())
}

/// `worker-types.child-status`.
fn child_status_val(status: ChildStatus) -> Val {
    match status {
        ChildStatus::Running => case("running", None),
        ChildStatus::Finished(result) => case("finished", Some(child_result_val(result))),
        ChildStatus::Cancelled => case("cancelled", None),
        ChildStatus::Failed(reason) => case("failed", Some(Val::String(reason))),
    }
}

/// `worker-types.child-result`; usage crosses as its `usage` family text, unknown as `none`.
fn child_result_val(result: ChildResult) -> Val {
    Val::Record(vec![
        ("final-text".to_owned(), Val::String(result.final_text)),
        ("turn-end".to_owned(), turn_end_val(result.turn_end)),
        (
            "usage-total".to_owned(),
            Val::Option(result.usage_total.map(|usage| Box::new(usage_val(usage)))),
        ),
        ("report".to_owned(), worker_report_val(result.report)),
    ])
}

// The wire types serialize to JSON without a failure case (no maps with non-string keys, no
// non-finite numbers), so the two conversions below cannot fail.

fn usage_val(usage: Usage) -> Val {
    Val::String(serde_json::to_string(&WireUsage::from(usage)).expect("usage serializes"))
}

fn provider_error_val(error: p1_contracts::ProviderError) -> Val {
    Val::String(
        serde_json::to_string(&WireProviderError::from(error)).expect("an error serializes"),
    )
}

/// `worker-types.turn-end`; a provider error crosses as its `provider-error` family text.
fn turn_end_val(end: TurnEnd) -> Val {
    match end {
        TurnEnd::Completed { stop } => {
            case("completed", Some(Val::Enum(stop_case(stop).to_owned())))
        }
        TurnEnd::Cancelled => case("cancelled", None),
        TurnEnd::ProviderFailed { error } => {
            case("provider-failed", Some(provider_error_val(error)))
        }
        TurnEnd::CommitFailed { message } => case("commit-failed", Some(Val::String(message))),
        TurnEnd::ContextFailed { message } => case("context-failed", Some(Val::String(message))),
    }
}

/// The `types.stop-reason` case of `stop`.
fn stop_case(stop: StopReason) -> &'static str {
    match stop {
        StopReason::EndTurn => "end-turn",
        StopReason::ToolUse => "tool-use",
        StopReason::MaxOutputTokens => "max-output-tokens",
        StopReason::ContextWindowExceeded => "context-window-exceeded",
        StopReason::Refusal => "refusal",
        StopReason::Paused => "paused",
        StopReason::Other => "other",
    }
}

/// `worker-types.worker-report`.
fn worker_report_val(report: WorkerReport) -> Val {
    let optional = |text: Option<String>| Val::Option(text.map(|text| Box::new(Val::String(text))));
    let finish = report.finish.map(|finish: FinishReport| {
        Box::new(Val::Record(vec![
            ("status".to_owned(), Val::String(finish.status)),
            ("needs".to_owned(), optional(finish.needs)),
            ("summary".to_owned(), optional(finish.summary)),
            ("evidence".to_owned(), optional(finish.evidence)),
        ]))
    });
    Val::Record(vec![
        (
            "tools".to_owned(),
            Val::List(report.tools.into_iter().map(Val::String).collect()),
        ),
        ("finish".to_owned(), Val::Option(finish)),
        (
            "missing-tool-calls".to_owned(),
            Val::List(
                report
                    .missing_tool_calls
                    .into_iter()
                    .map(|(name, count)| Val::Tuple(vec![Val::String(name), Val::U32(count)]))
                    .collect(),
            ),
        ),
    ])
}

#[cfg(test)]
mod tests {
    use std::future::pending;

    use super::*;

    /// An observer whose `wait` never ends and ignores its token: only the runtime's own
    /// race can answer a cancelled wait.
    struct Stuck;

    impl WorkersObserve for Stuck {
        fn describe<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
            Box::pin(async { Err(WorkerError::UnknownChild) })
        }

        fn status<'a>(
            &'a self,
            _id: &'a ChildId,
        ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
            Box::pin(async { Ok(ChildStatus::Running) })
        }

        fn wait<'a>(
            &'a self,
            _id: &'a ChildId,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
            Box::pin(pending())
        }

        fn result<'a>(
            &'a self,
            _id: &'a ChildId,
        ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
            Box::pin(async { Ok(ChildStatus::Running) })
        }

        fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
            Box::pin(async { Vec::new() })
        }
    }

    #[tokio::test]
    async fn a_cancelled_wait_answers_running_whatever_the_service_does() {
        let cancel = CancellationToken::new();
        cancel.cancel();
        let answer = wait_worker(Arc::new(Stuck), &[Val::String("w1".to_owned())], cancel)
            .await
            .expect("the wait answers");
        assert_eq!(answer, ok(Some(case("running", None))));
    }

    #[test]
    fn every_worker_error_has_its_wit_case() {
        let cases = [
            (WorkerError::UnknownChild, case("unknown-child", None)),
            (WorkerError::Busy, case("busy", None)),
            (
                WorkerError::LimitReached { max: 3 },
                case("limit-reached", Some(Val::U64(3))),
            ),
            (
                WorkerError::InvalidEnvironment("nope".to_owned()),
                case("invalid-environment", Some(Val::String("nope".to_owned()))),
            ),
            (WorkerError::IdsExhausted, case("ids-exhausted", None)),
            (
                WorkerError::Regrant("no".to_owned()),
                case("regrant", Some(Val::String("no".to_owned()))),
            ),
            (WorkerError::ShutDown, case("shut-down", None)),
        ];
        for (error, expected) in cases {
            assert_eq!(worker_error_val(error), expected);
        }
    }

    #[test]
    fn every_workflow_error_has_its_wit_case() {
        let parse = WorkflowError::Parse {
            message: "bad".to_owned(),
            line: 2,
            column: 5,
        };
        let Val::Variant(name, Some(record)) = workflow_error_val(parse) else {
            panic!("parse carries its record");
        };
        assert_eq!(name, "parse");
        assert_eq!(
            *record,
            Val::Record(vec![
                ("message".to_owned(), Val::String("bad".to_owned())),
                ("line".to_owned(), Val::U32(2)),
                ("column".to_owned(), Val::U32(5)),
            ])
        );
        assert_eq!(
            workflow_error_val(WorkflowError::Preflight("not granted: start".to_owned())),
            case(
                "preflight",
                Some(Val::String("not granted: start".to_owned()))
            )
        );
        assert_eq!(
            workflow_error_val(WorkflowError::UnknownRun),
            case("unknown-run", None)
        );
        assert_eq!(
            workflow_error_val(WorkflowError::ShutDown),
            case("shut-down", None)
        );
        assert_eq!(
            workflow_error_val(WorkflowError::Io("disk".to_owned())),
            case("io", Some(Val::String("disk".to_owned())))
        );
    }

    #[test]
    fn a_child_spec_is_read_by_field_name_and_gets_no_workspace() {
        let spec = Val::Record(vec![
            ("environment".to_owned(), Val::String("child".to_owned())),
            ("task".to_owned(), Val::String("do it".to_owned())),
            (
                "tools".to_owned(),
                Val::List(vec![Val::String("read".to_owned())]),
            ),
        ]);
        assert_eq!(
            child_spec(Some(&spec)).expect("a spec"),
            ChildSpec {
                environment: "child".to_owned(),
                task: "do it".to_owned(),
                tools: vec!["read".to_owned()],
                workspace: None,
            }
        );
        assert!(child_spec(Some(&Val::Record(vec![]))).is_err());
    }

    #[test]
    fn start_arguments_that_are_not_an_object_start_nothing() {
        let request = |args: &str| {
            Val::Record(vec![
                ("script".to_owned(), Val::String("1".to_owned())),
                ("args".to_owned(), Val::String(args.to_owned())),
                ("resume-from".to_owned(), Val::Option(None)),
            ])
        };
        let read = start_request(Some(&request(r#"{"a":1}"#)))
            .expect("the record is well formed")
            .expect("an object is accepted");
        assert_eq!(read.args, serde_json::json!({"a": 1}));
        assert_eq!(read.resume_from, None);
        for bad in ["[1]", "not json"] {
            assert!(matches!(
                start_request(Some(&request(bad))),
                Ok(Err(WorkflowError::Preflight(_)))
            ));
        }
    }

    /// A component that imports `workers-observe`'s `status` and `grantable` and exports
    /// `declaration`, which calls `status`, and `grantable-count`, which answers the length of
    /// `grantable`'s list. The runtime has no WAT parser (no `wat` feature), so it is the
    /// binary `wasm-tools parse` then `wasm-tools strip --all` make of this text; its `status`
    /// type is simplified, which the restricted linker cannot tell, since it traps an import by
    /// name:
    //
    // (component
    //   (import "p1:module/workers-observe@1.0.0" (instance $observe
    //     (export "status" (func (param "id" string) (result u32)))
    //     (export "grantable" (func (result (list string))))))
    //   (alias export $observe "status" (func $status))
    //   (alias export $observe "grantable" (func $grantable))
    //   (core module $memory
    //     (memory (export "memory") 1)
    //     (global $next (mut i32) (i32.const 1024))
    //     (func (export "realloc") (param i32 i32 i32 i32) (result i32)
    //       (local $at i32)
    //       (local.set $at
    //         (i32.and
    //           (i32.add (global.get $next) (i32.sub (local.get 2) (i32.const 1)))
    //           (i32.sub (i32.const 0) (local.get 2))))
    //       (global.set $next (i32.add (local.get $at) (local.get 3)))
    //       (local.get $at)))
    //   (core instance $memory (instantiate $memory))
    //   (alias core export $memory "memory" (core memory $mem))
    //   (alias core export $memory "realloc" (core func $realloc))
    //   (core func $status (canon lower (func $status) (memory $mem)))
    //   (core func $grantable (canon lower (func $grantable) (memory $mem) (realloc $realloc)))
    //   (core module $probe
    //     (import "host" "memory" (memory 1))
    //     (import "host" "status" (func $status (param i32 i32) (result i32)))
    //     (import "host" "grantable" (func $grantable (param i32)))
    //     (func (export "declaration") (result i32)
    //       (call $status (i32.const 0) (i32.const 0)))
    //     (func (export "grantable-count") (result i32)
    //       (call $grantable (i32.const 16))
    //       (i32.load (i32.const 20))))
    //   (core instance $host
    //     (export "memory" (memory $mem))
    //     (export "status" (func $status))
    //     (export "grantable" (func $grantable)))
    //   (core instance $probe (instantiate $probe (with "host" (instance $host))))
    //   (func (export "declaration") (result u32) (canon lift (core func $probe "declaration")))
    //   (func (export "grantable-count") (result u32) (canon lift (core func $probe "grantable-count"))))
    const OBSERVE_PROBE: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, 0x0d, 0x00, 0x01, 0x00, 0x07, 0x2d, 0x01, 0x42, 0x05, 0x01, 0x40,
        0x01, 0x02, 0x69, 0x64, 0x73, 0x00, 0x79, 0x04, 0x00, 0x06, 0x73, 0x74, 0x61, 0x74, 0x75,
        0x73, 0x01, 0x00, 0x01, 0x70, 0x73, 0x01, 0x40, 0x00, 0x00, 0x01, 0x04, 0x00, 0x09, 0x67,
        0x72, 0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x01, 0x02, 0x0a, 0x24, 0x01, 0x00, 0x1f,
        0x70, 0x31, 0x3a, 0x6d, 0x6f, 0x64, 0x75, 0x6c, 0x65, 0x2f, 0x77, 0x6f, 0x72, 0x6b, 0x65,
        0x72, 0x73, 0x2d, 0x6f, 0x62, 0x73, 0x65, 0x72, 0x76, 0x65, 0x40, 0x31, 0x2e, 0x30, 0x2e,
        0x30, 0x05, 0x00, 0x06, 0x18, 0x02, 0x01, 0x00, 0x00, 0x06, 0x73, 0x74, 0x61, 0x74, 0x75,
        0x73, 0x01, 0x00, 0x00, 0x09, 0x67, 0x72, 0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x01,
        0x5c, 0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00, 0x01, 0x09, 0x01, 0x60, 0x04, 0x7f,
        0x7f, 0x7f, 0x7f, 0x01, 0x7f, 0x03, 0x02, 0x01, 0x00, 0x05, 0x03, 0x01, 0x00, 0x01, 0x06,
        0x07, 0x01, 0x7f, 0x01, 0x41, 0x80, 0x08, 0x0b, 0x07, 0x14, 0x02, 0x06, 0x6d, 0x65, 0x6d,
        0x6f, 0x72, 0x79, 0x02, 0x00, 0x07, 0x72, 0x65, 0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x00, 0x00,
        0x0a, 0x1f, 0x01, 0x1d, 0x01, 0x01, 0x7f, 0x23, 0x00, 0x20, 0x02, 0x41, 0x01, 0x6b, 0x6a,
        0x41, 0x00, 0x20, 0x02, 0x6b, 0x71, 0x21, 0x04, 0x20, 0x04, 0x20, 0x03, 0x6a, 0x24, 0x00,
        0x20, 0x04, 0x0b, 0x02, 0x04, 0x01, 0x00, 0x00, 0x00, 0x06, 0x18, 0x02, 0x00, 0x02, 0x01,
        0x00, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x00, 0x00, 0x01, 0x00, 0x07, 0x72, 0x65,
        0x61, 0x6c, 0x6c, 0x6f, 0x63, 0x08, 0x0f, 0x02, 0x01, 0x00, 0x00, 0x01, 0x03, 0x00, 0x01,
        0x00, 0x01, 0x02, 0x03, 0x00, 0x04, 0x00, 0x01, 0x8a, 0x01, 0x00, 0x61, 0x73, 0x6d, 0x01,
        0x00, 0x00, 0x00, 0x01, 0x0f, 0x03, 0x60, 0x02, 0x7f, 0x7f, 0x01, 0x7f, 0x60, 0x01, 0x7f,
        0x00, 0x60, 0x00, 0x01, 0x7f, 0x02, 0x2f, 0x03, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x06, 0x6d,
        0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00, 0x01, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x06, 0x73,
        0x74, 0x61, 0x74, 0x75, 0x73, 0x00, 0x00, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x09, 0x67, 0x72,
        0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x00, 0x01, 0x03, 0x03, 0x02, 0x02, 0x02, 0x07,
        0x21, 0x02, 0x0b, 0x64, 0x65, 0x63, 0x6c, 0x61, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x00,
        0x02, 0x0f, 0x67, 0x72, 0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x2d, 0x63, 0x6f, 0x75,
        0x6e, 0x74, 0x00, 0x03, 0x0a, 0x16, 0x02, 0x08, 0x00, 0x41, 0x00, 0x41, 0x00, 0x10, 0x00,
        0x0b, 0x0b, 0x00, 0x41, 0x10, 0x10, 0x01, 0x41, 0x14, 0x28, 0x02, 0x00, 0x0b, 0x02, 0x2b,
        0x02, 0x01, 0x03, 0x06, 0x6d, 0x65, 0x6d, 0x6f, 0x72, 0x79, 0x02, 0x00, 0x06, 0x73, 0x74,
        0x61, 0x74, 0x75, 0x73, 0x00, 0x01, 0x09, 0x67, 0x72, 0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c,
        0x65, 0x00, 0x02, 0x00, 0x01, 0x01, 0x04, 0x68, 0x6f, 0x73, 0x74, 0x12, 0x01, 0x07, 0x05,
        0x01, 0x40, 0x00, 0x00, 0x79, 0x06, 0x11, 0x01, 0x00, 0x00, 0x01, 0x02, 0x0b, 0x64, 0x65,
        0x63, 0x6c, 0x61, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x08, 0x06, 0x01, 0x00, 0x00, 0x03,
        0x00, 0x01, 0x07, 0x05, 0x01, 0x40, 0x00, 0x00, 0x79, 0x06, 0x15, 0x01, 0x00, 0x00, 0x01,
        0x02, 0x0f, 0x67, 0x72, 0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x2d, 0x63, 0x6f, 0x75,
        0x6e, 0x74, 0x08, 0x06, 0x01, 0x00, 0x00, 0x04, 0x00, 0x02, 0x0b, 0x25, 0x02, 0x00, 0x0b,
        0x64, 0x65, 0x63, 0x6c, 0x61, 0x72, 0x61, 0x74, 0x69, 0x6f, 0x6e, 0x01, 0x02, 0x00, 0x00,
        0x0f, 0x67, 0x72, 0x61, 0x6e, 0x74, 0x61, 0x62, 0x6c, 0x65, 0x2d, 0x63, 0x6f, 0x75, 0x6e,
        0x74, 0x01, 0x03, 0x00,
    ];

    /// D085: on the restricted path `declaration` runs on, the two list functions answer and
    /// every other `workers-observe` import still traps, here `status` during `declaration`;
    /// without lists (every module that is not granted `workers-observe`) both trap.
    #[test]
    fn the_restricted_path_links_only_the_two_lists_and_status_still_traps() {
        let engine = crate::engine().expect("engine");
        let component =
            wasmtime::component::Component::new(&engine, OBSERVE_PROBE).expect("the probe");
        let lists = WorkerLists {
            grantable: vec!["read".to_owned(), "shell".to_owned()],
            environments: vec!["coder".to_owned()],
        };
        let restricted =
            crate::restricted::Restricted::with_worker_lists(&engine, &component, Some(&lists))
                .expect("the probe links with the lists");
        assert_eq!(
            restricted.call("grantable-count", &[]),
            Some(vec![Val::U32(2)])
        );
        assert_eq!(
            restricted.call("declaration", &[]),
            None,
            "`status` traps during `declaration`"
        );
        // The trapped instance is replaced, and the lists still answer.
        assert_eq!(
            restricted.call("grantable-count", &[]),
            Some(vec![Val::U32(2)])
        );

        let bare = crate::restricted::Restricted::new(&engine, &component)
            .expect("the probe links as traps");
        assert_eq!(bare.call("grantable-count", &[]), None);
    }
}
