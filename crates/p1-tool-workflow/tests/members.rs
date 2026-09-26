//! Each workflow tool member stands alone: it is constructed from only its own narrow
//! operation trait, and dispatching it reaches no other operation.
//!
//! The fakes below implement ONE of `StartRuns`, `ObserveRuns`, `CancelRuns` and not
//! `WorkflowService`, so the compiler proves `workflow_status` and `workflow_result`
//! need no start surface. The guarded fake implements the whole service with every
//! foreign operation panicking; dispatching a member against it passes only if the
//! member never called one.

use std::sync::{Arc, Mutex};

use p1_contracts::{BoxFuture, CancellationToken, Tool, ToolCall, ToolContext, ToolInput};
use p1_tool_workflow::{
    WorkflowCancelTool, WorkflowResultTool, WorkflowStartTool, WorkflowStatusTool, all,
};
use p1_workflow::{
    CancelRuns, ObserveRuns, RunId, RunProgress, RunStatus, StartRequest, StartRuns, WorkflowError,
    WorkflowService,
};

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

fn running() -> RunStatus {
    RunStatus::Running(RunProgress {
        phase: Some("review".into()),
        steps_started: 2,
        steps_ended: 1,
        replayed: 0,
        log: Vec::new(),
    })
}

const RUNNING_TEXT: &str =
    "Workflow wf1: running — phase review, 2 steps started, 1 ended, 0 replayed";

// ---------------------------------------------------------------- one trait each

#[derive(Default)]
struct StartOnly {
    scripts: Mutex<Vec<String>>,
}

impl StartRuns for StartOnly {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move {
            self.scripts.lock().unwrap().push(request.script);
            Ok(RunId("wf1".into()))
        })
    }
}

struct ObserveOnly;

impl ObserveRuns for ObserveOnly {
    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            if id.0 == "wf1" {
                Ok(running())
            } else {
                Err(WorkflowError::UnknownRun)
            }
        })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a RunId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        // A wait whose cancel fired first: the member reports it cancelled.
        Box::pin(async { Ok(running()) })
    }
}

#[derive(Default)]
struct CancelOnly {
    cancelled: Mutex<Vec<String>>,
}

impl CancelRuns for CancelOnly {
    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            self.cancelled.lock().unwrap().push(id.0.clone());
            Ok(())
        })
    }
}

/// Every member is built from its own trait alone, and its declaration is the one the
/// composition helper gives it over a whole service.
#[test]
fn each_member_is_built_from_its_own_trait_with_the_same_declaration() {
    let service: Arc<dyn WorkflowService> = Arc::new(Guarded { allowed: &[] });
    let composed = all(service);
    let observe = Arc::new(ObserveOnly);
    let members: Vec<Arc<dyn Tool>> = vec![
        Arc::new(WorkflowStartTool::new(Arc::new(StartOnly::default()))),
        Arc::new(WorkflowStatusTool::new(Arc::clone(&observe))),
        Arc::new(WorkflowResultTool::new(observe)),
        Arc::new(WorkflowCancelTool::new(Arc::new(CancelOnly::default()))),
    ];
    assert_eq!(members.len(), composed.len());
    for (member, whole) in members.iter().zip(&composed) {
        assert_eq!(member.declaration(), whole.declaration());
        assert_eq!(member.identity(), whole.identity());
    }
}

#[tokio::test]
async fn each_member_works_from_its_own_trait() {
    let start = Arc::new(StartOnly::default());
    let outcome = WorkflowStartTool::new(Arc::clone(&start))
        .execute(
            &call("workflow_start", r#"{"script":"agent(\"t\")"}"#),
            context(),
        )
        .await;
    assert_eq!(
        outcome.content,
        "Started workflow wf1. You will be notified when it ends; do not poll."
    );
    assert_eq!(*start.scripts.lock().unwrap(), ["agent(\"t\")"]);

    let observe = Arc::new(ObserveOnly);
    let status = WorkflowStatusTool::new(Arc::clone(&observe))
        .execute(&call("workflow_status", r#"{"id":"wf1"}"#), context())
        .await;
    assert_eq!(status.content, RUNNING_TEXT);
    let unknown = WorkflowStatusTool::new(Arc::clone(&observe))
        .execute(&call("workflow_status", r#"{"id":"wf9"}"#), context())
        .await;
    assert_eq!(unknown.content, "No workflow wf9.");
    let result = WorkflowResultTool::new(observe);
    let read = result
        .execute(&call("workflow_result", r#"{"id":"wf1"}"#), context())
        .await;
    assert_eq!(read.content, RUNNING_TEXT);
    let waited = result
        .execute(
            &call("workflow_result", r#"{"id":"wf1","wait":true}"#),
            context(),
        )
        .await;
    assert_eq!(waited.status, p1_contracts::ToolStatus::Cancelled);

    let cancel = Arc::new(CancelOnly::default());
    let outcome = WorkflowCancelTool::new(Arc::clone(&cancel))
        .execute(&call("workflow_cancel", r#"{"id":"wf1"}"#), context())
        .await;
    assert_eq!(outcome.content, "Workflow wf1 cancelled.");
    assert_eq!(*cancel.cancelled.lock().unwrap(), ["wf1"]);
}

// ---------------------------------------------------------------- foreign calls panic

/// The whole service; every operation outside `allowed` panics.
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

impl WorkflowService for Guarded {
    fn start<'a>(&'a self, _request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        self.enter("start");
        Box::pin(async { Ok(RunId("wf1".into())) })
    }

    fn status<'a>(&'a self, _id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        self.enter("status");
        Box::pin(async { Ok(running()) })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a RunId,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        self.enter("wait");
        Box::pin(async { Ok(running()) })
    }

    fn cancel<'a>(&'a self, _id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        self.enter("cancel");
        Box::pin(async { Ok(()) })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        self.enter("list");
        Box::pin(async { Vec::new() })
    }
}

/// Built from the whole service (through the blanket impls, as the host does), each
/// member dispatches only its own operations.
#[tokio::test]
async fn dispatching_a_member_never_calls_another_operation() {
    let reader: Arc<dyn WorkflowService> = Arc::new(Guarded {
        allowed: &["status", "wait"],
    });
    let status = WorkflowStatusTool::new(Arc::clone(&reader))
        .execute(&call("workflow_status", r#"{"id":"wf1"}"#), context())
        .await;
    assert_eq!(status.content, RUNNING_TEXT);
    let result = WorkflowResultTool::new(reader);
    let read = result
        .execute(&call("workflow_result", r#"{"id":"wf1"}"#), context())
        .await;
    assert_eq!(read.content, RUNNING_TEXT);
    let waited = result
        .execute(
            &call("workflow_result", r#"{"id":"wf1","wait":true}"#),
            context(),
        )
        .await;
    assert_eq!(waited.status, p1_contracts::ToolStatus::Cancelled);

    let starter = Arc::new(Guarded {
        allowed: &["start"],
    });
    let outcome = WorkflowStartTool::new(starter)
        .execute(&call("workflow_start", r#"{"script":"x"}"#), context())
        .await;
    assert_eq!(
        outcome.content,
        "Started workflow wf1. You will be notified when it ends; do not poll."
    );

    let canceller = Arc::new(Guarded {
        allowed: &["cancel"],
    });
    let outcome = WorkflowCancelTool::new(canceller)
        .execute(&call("workflow_cancel", r#"{"id":"wf1"}"#), context())
        .await;
    assert_eq!(outcome.content, "Workflow wf1 cancelled.");
}
