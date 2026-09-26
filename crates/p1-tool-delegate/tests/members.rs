//! Each delegate tool member stands alone: it is constructed from only its own worker
//! traits, and dispatching it reaches no other operation.
//!
//! The first group builds every member from a fake that implements ONLY that member's
//! traits, so the compiler is the proof: `WorkerResultTool` needs no start surface, and
//! the continue and cancel members need nothing but `WorkersControl`. The second group
//! gives each member a fake implementing all three traits whose foreign operations
//! panic, and dispatches the member: a panic would fail the test, so passing shows the
//! member never called them.

use std::sync::{Arc, Mutex};

use p1_contracts::{
    BoxFuture, CancellationToken, StopReason, Tool, ToolCall, ToolContext, ToolInput, ToolStatus,
    TurnEnd,
};
use p1_tool_delegate::{
    ControlSurface, ObserveSurface, StartSurface, WorkerCancelTool, WorkerContinueTool,
    WorkerResultTool, WorkerStartTool, all,
};
use p1_workers::{
    AgentFactory, ChildId, ChildResult, ChildSpec, ChildStatus, InProcessWorkers, WorkerError,
    WorkerReport, WorkerService, WorkersControl, WorkersObserve, WorkersStart,
};

fn grantable() -> Vec<String> {
    vec!["read".into(), "edit".into()]
}

fn environments() -> Vec<String> {
    vec!["child".into()]
}

fn call(name: &str, input: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(input.into()),
    }
}

fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

fn finished() -> ChildStatus {
    ChildStatus::Finished(ChildResult {
        final_text: "all done".into(),
        turn_end: TurnEnd::Completed {
            stop: StopReason::EndTurn,
        },
        usage_total: None,
        report: WorkerReport::new(vec!["read".into(), "finish".into()]),
    })
}

// ---------------------------------------------------------------- one trait each

/// Implements `WorkersStart` and, for `describe` only, `WorkersObserve`: nothing else.
#[derive(Default)]
struct StartOnly {
    specs: Mutex<Vec<ChildSpec>>,
}

impl WorkersStart for StartOnly {
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        Box::pin(async move {
            self.specs.lock().unwrap().push(spec);
            Ok(ChildId("w1".into()))
        })
    }
}

impl WorkersObserve for StartOnly {
    fn describe<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        Box::pin(async { Ok("route/model".to_string()) })
    }

    fn status<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        unreachable!("worker_start reads no status")
    }

    fn wait<'a>(
        &'a self,
        _id: &'a ChildId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        unreachable!("worker_start never waits")
    }

    fn result<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        unreachable!("worker_start reads no result")
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        unreachable!("worker_start lists nothing")
    }
}

/// Implements `WorkersObserve` alone: no start, no control.
struct ObserveOnly;

impl WorkersObserve for ObserveOnly {
    fn describe<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        unreachable!("worker_result describes nothing")
    }

    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move {
            if id.0 == "w1" {
                Ok(ChildStatus::Running)
            } else {
                Err(WorkerError::UnknownChild)
            }
        })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a ChildId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async { Ok(finished()) })
    }

    fn result<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        unreachable!("worker_result reads status or waits")
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        unreachable!("worker_result lists nothing")
    }
}

/// Implements `WorkersControl` alone: no start, no observe.
#[derive(Default)]
struct ControlOnly {
    calls: Mutex<Vec<String>>,
}

impl WorkersControl for ControlOnly {
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!("cancel {}", id.0));
            Ok(())
        })
    }

    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(format!(
                "continue {} {message} [{}]",
                id.0,
                add_tools.join(",")
            ));
            Ok(())
        })
    }
}

/// Every member is built from its own traits alone, and its declaration is the one the
/// composition helper gives it over a whole service: the texts do not depend on what
/// the member was built from.
#[test]
fn each_member_is_built_from_its_own_traits_with_the_same_declaration() {
    let factory: AgentFactory = Arc::new(|_spec: &ChildSpec| Err("unused".to_string()));
    let service: Arc<dyn WorkerService> = InProcessWorkers::new(factory, 1);
    let composed = all(service, grantable(), environments());

    let control = Arc::new(ControlOnly::default());
    let members: Vec<Arc<dyn Tool>> = vec![
        Arc::new(WorkerStartTool::new(
            Arc::new(StartOnly::default()),
            grantable(),
            environments(),
        )),
        Arc::new(WorkerResultTool::new(Arc::new(ObserveOnly))),
        Arc::new(WorkerContinueTool::new(Arc::clone(&control), grantable())),
        Arc::new(WorkerCancelTool::new(control)),
    ];
    for (member, whole) in members.iter().zip(&composed) {
        assert_eq!(member.declaration(), whole.declaration());
        assert_eq!(member.identity(), whole.identity());
    }
}

#[tokio::test]
async fn worker_start_from_its_traits_writes_the_route_and_model() {
    let workers = Arc::new(StartOnly::default());
    let tool = WorkerStartTool::new(Arc::clone(&workers), grantable(), environments());
    let outcome = tool
        .execute(
            &call(
                "worker_start",
                r#"{"environment":"child","task":"do it","tools":["read","read"]}"#,
            ),
            context(),
        )
        .await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert_eq!(
        outcome.content,
        "Started worker w1 on route/model with tools: read, finish. You will be notified when \
         it finishes."
    );
    assert_eq!(workers.specs.lock().unwrap()[0].tools, ["read"]);
}

#[tokio::test]
async fn worker_result_from_observe_alone_reads_and_waits() {
    let tool = WorkerResultTool::new(Arc::new(ObserveOnly));
    let read = tool
        .execute(&call("worker_result", r#"{"id":"w1"}"#), context())
        .await;
    assert_eq!(
        (read.status, read.content.as_str()),
        (ToolStatus::Ok, "Worker w1: running")
    );
    let waited = tool
        .execute(
            &call("worker_result", r#"{"id":"w1","wait":true}"#),
            context(),
        )
        .await;
    assert_eq!(
        waited.content,
        "tools: read, finish\nfinish: not called\n---\nWorker w1: finished\n\nall done"
    );
    let unknown = tool
        .execute(&call("worker_result", r#"{"id":"w9"}"#), context())
        .await;
    assert_eq!(
        (unknown.status, unknown.content.as_str()),
        (ToolStatus::Error, "No worker w9.")
    );
}

#[tokio::test]
async fn worker_continue_and_cancel_from_control_alone() {
    let control = Arc::new(ControlOnly::default());
    let cont = WorkerContinueTool::new(Arc::clone(&control), grantable());
    let cancel = WorkerCancelTool::new(Arc::clone(&control));
    let continued = cont
        .execute(
            &call(
                "worker_continue",
                r#"{"id":"w1","message":"again","add_tools":["edit"]}"#,
            ),
            context(),
        )
        .await;
    assert_eq!(
        continued.content,
        "Added tools: edit. Message sent to worker w1."
    );
    let cancelled = cancel
        .execute(&call("worker_cancel", r#"{"id":"w1"}"#), context())
        .await;
    assert_eq!(cancelled.content, "Worker w1 cancelled.");
    assert_eq!(
        *control.calls.lock().unwrap(),
        ["continue w1 again [edit]", "cancel w1"]
    );
}

// ---------------------------------------------------------------- foreign calls panic

/// Implements all three traits, like a scope does; every operation outside `allowed`
/// panics, so a member that reached one would fail its test.
struct Guarded {
    allowed: &'static [&'static str],
}

impl Guarded {
    fn enter(&self, operation: &str) {
        assert!(
            self.allowed.contains(&operation),
            "the member called `{operation}`, which is not its own"
        );
    }
}

impl WorkersStart for Guarded {
    fn start<'a>(&'a self, _spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        self.enter("start");
        Box::pin(async { Ok(ChildId("w1".into())) })
    }
}

impl WorkersObserve for Guarded {
    fn describe<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        self.enter("describe");
        Box::pin(async { Ok("route/model".to_string()) })
    }

    fn status<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        self.enter("status");
        Box::pin(async { Ok(ChildStatus::Cancelled) })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a ChildId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        self.enter("wait");
        Box::pin(async { Ok(ChildStatus::Failed("boom".into())) })
    }

    fn result<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        self.enter("result");
        Box::pin(async { Ok(ChildStatus::Cancelled) })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        self.enter("list");
        Box::pin(async { Vec::new() })
    }
}

impl WorkersControl for Guarded {
    fn cancel<'a>(&'a self, _id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        self.enter("cancel");
        Box::pin(async { Ok(()) })
    }

    fn continue_child<'a>(
        &'a self,
        _id: &'a ChildId,
        _message: String,
        _add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        self.enter("continue_child");
        Box::pin(async { Ok(()) })
    }
}

/// `worker_result` dispatched against a surface whose start, control, describe and
/// list panic: it reads the status (or waits) and calls nothing else.
#[tokio::test]
async fn dispatching_worker_result_never_calls_another_operation() {
    let workers = Arc::new(Guarded {
        allowed: &["status", "wait"],
    });
    let tool = WorkerResultTool::new(ObserveSurface::from(workers));
    let read = tool
        .execute(&call("worker_result", r#"{"id":"w1"}"#), context())
        .await;
    assert_eq!(read.content, "Worker w1: cancelled");
    let waited = tool
        .execute(
            &call("worker_result", r#"{"id":"w1","wait":true}"#),
            context(),
        )
        .await;
    assert_eq!(waited.content, "Worker w1: failed\n\nboom");
}

/// `worker_start` calls `start` and then `describe` for the child it started, and no
/// other operation.
#[tokio::test]
async fn dispatching_worker_start_calls_only_start_and_describe() {
    let workers = Arc::new(Guarded {
        allowed: &["start", "describe"],
    });
    let tool = WorkerStartTool::new(StartSurface::from(workers), grantable(), environments());
    let outcome = tool
        .execute(
            &call(
                "worker_start",
                r#"{"environment":"child","task":"t","tools":["edit"]}"#,
            ),
            context(),
        )
        .await;
    assert_eq!(
        outcome.content,
        "Started worker w1 on route/model with tools: edit, finish. You will be notified when \
         it finishes."
    );
}

/// `worker_continue` and `worker_cancel` call only their own control operation.
#[tokio::test]
async fn dispatching_the_control_members_calls_only_their_operation() {
    let cont = WorkerContinueTool::new(
        ControlSurface::from(Arc::new(Guarded {
            allowed: &["continue_child"],
        })),
        grantable(),
    );
    let outcome = cont
        .execute(
            &call("worker_continue", r#"{"id":"w1","message":"go"}"#),
            context(),
        )
        .await;
    assert_eq!(outcome.content, "Worker w1 continues.");

    let cancel = WorkerCancelTool::new(Arc::new(Guarded {
        allowed: &["cancel"],
    }));
    let outcome = cancel
        .execute(&call("worker_cancel", r#"{"id":"w1"}"#), context())
        .await;
    assert_eq!(outcome.content, "Worker w1 cancelled.");
}

// ---------------------------------------------------------------- the result-name hook

/// Built from the whole service, as the host and `all` do, `worker_result` still points
/// the completion notification at its name; a bare observe surface has no service to
/// tell and names nothing.
#[test]
fn the_result_tool_name_reaches_the_service_through_its_surface() {
    let factory: AgentFactory = Arc::new(|_spec: &ChildSpec| Err("unused".to_string()));
    let workers = InProcessWorkers::new(factory, 1);
    let service: Arc<dyn WorkerService> = workers.clone();
    let _tools = all(service, grantable(), environments());
    assert_eq!(workers.result_tool_name().as_deref(), Some("worker_result"));

    let named = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&named);
    let tool = WorkerResultTool::new(
        ObserveSurface::new(Arc::new(ObserveOnly))
            .with_result_tool_name(move |name| seen.lock().unwrap().push(name.to_string())),
    )
    .with_face(
        p1_tool_delegate::ToolFace::new("ResultTool", "renamed"),
        "gpt",
    );
    assert_eq!(tool.declaration().name, "ResultTool");
    assert_eq!(*named.lock().unwrap(), ["worker_result", "ResultTool"]);

    // No hook, no call: the observe-only member is complete without one.
    let _bare = WorkerResultTool::new(Arc::new(ObserveOnly));
}
