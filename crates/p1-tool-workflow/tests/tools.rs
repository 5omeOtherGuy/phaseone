use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolInput,
    ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_tool_workflow::{
    ToolFace, WorkflowCancelTool, WorkflowResultTool, WorkflowStartTool, WorkflowStatusTool, all,
};
use p1_workflow::{
    CallId, Counts, RunId, RunOutcome, RunProgress, RunReport, RunStatus, StartRequest, StepLine,
    StepStatus, WorkflowError, WorkflowService,
};
use serde_json::json;
use tokio::sync::Notify;

struct FakeService {
    starts: Mutex<VecDeque<Result<RunId, WorkflowError>>>,
    requests: Mutex<Vec<StartRequest>>,
    status: Mutex<Result<RunStatus, WorkflowError>>,
    cancel: Mutex<Result<(), WorkflowError>>,
    wait_entered: Notify,
    release_wait: Notify,
}

impl FakeService {
    fn new(status: RunStatus) -> Arc<Self> {
        Arc::new(Self {
            starts: Mutex::new(VecDeque::new()),
            requests: Mutex::new(Vec::new()),
            status: Mutex::new(Ok(status)),
            cancel: Mutex::new(Ok(())),
            wait_entered: Notify::new(),
            release_wait: Notify::new(),
        })
    }

    fn set_status(&self, status: RunStatus) {
        *self.status.lock().unwrap() = Ok(status);
    }
}

#[tokio::test]
async fn workflow_describes_real_result_text_and_is_not_destructive() {
    let tool = WorkflowStartTool::new(FakeService::new(RunStatus::Running(progress())));
    let call = ToolCall {
        call_id: "c1".into(),
        name: tool.declaration().name.clone(),
        input: ToolInput::Json(r#"{"script":"agent(\"task\")"}"#.into()),
    };
    assert!(!tool.describe(&call).destructive);
    let outcome = tool
        .execute(
            &call,
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    let result = ToolResultItem {
        call_id: "c1".into(),
        name: call.name.clone(),
        status: outcome.status,
        content: outcome.content,
    };
    assert_eq!(
        tool.describe_result(&call, &result),
        ResultDescription {
            summary: "1 lines".into(),
            detail: Some(ResultDetail::Text(result.content)),
        }
    );
}

impl WorkflowService for FakeService {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            self.starts
                .lock()
                .unwrap()
                .pop_front()
                .unwrap_or_else(|| Ok(RunId("wf1".into())))
        })
    }

    fn status<'a>(&'a self, _id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move { self.status.lock().unwrap().clone() })
    }

    fn wait<'a>(
        &'a self,
        _id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            self.wait_entered.notify_one();
            tokio::select! {
                _ = self.release_wait.notified() => self.status.lock().unwrap().clone(),
                _ = cancel.cancelled() => Ok(RunStatus::Running(progress())),
            }
        })
    }

    fn cancel<'a>(&'a self, _id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move { self.cancel.lock().unwrap().clone() })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        Box::pin(async { Vec::new() })
    }
}

fn progress() -> RunProgress {
    RunProgress {
        phase: Some("review".into()),
        steps_started: 3,
        steps_ended: 2,
        replayed: 1,
        log: vec!["first".into(), "second".into()],
    }
}

fn report() -> RunReport {
    RunReport {
        id: RunId("wf1".into()),
        outcome: RunOutcome::CompletedWithIssues,
        value: json!({"answer":42}),
        counts: Counts {
            steps: 2,
            replayed: 1,
            done: 1,
            blocked: 0,
            failed: 1,
            cancelled: 0,
            not_verified: 1,
            capped: 1,
            invalid_output: 0,
            fell_back: 0,
        },
        steps: vec![
            StepLine {
                call: CallId("c1".into()),
                ordinal: 1,
                label: Some("review".into()),
                role: "reviewer".into(),
                model: "route/model".into(),
                worker: Some("w3 (route/model)".into()),
                status: StepStatus::Done,
                schema: "passed".into(),
                evidence: Some("not verified; parent verification required".into()),
                attempts: 2,
                replayed: true,
                error: None,
                models: Vec::new(),
            },
            StepLine {
                call: CallId("c2".into()),
                ordinal: 2,
                label: None,
                role: "judge".into(),
                model: "other/model".into(),
                worker: None,
                status: StepStatus::Failed,
                schema: "not requested".into(),
                evidence: None,
                attempts: 0,
                replayed: false,
                error: Some("quota_exceeded: cap 3".into()),
                models: Vec::new(),
            },
        ],
        error: Some("script error".into()),
        run_dir: PathBuf::from("/runs/wf1"),
    }
}

fn tools(fake: &Arc<FakeService>) -> Vec<Arc<dyn Tool>> {
    let service: Arc<dyn WorkflowService> = fake.clone();
    all(service)
}

fn tool(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.declaration().name == name)
        .unwrap()
        .clone()
}

async fn execute_with(
    tool: &Arc<dyn Tool>,
    input: ToolInput,
    cancel: CancellationToken,
) -> ToolOutcome {
    let call = ToolCall {
        call_id: "call".into(),
        name: tool.declaration().name.clone(),
        input,
    };
    tool.execute(&call, ToolContext { cancel }).await
}

async fn execute(tool: &Arc<dyn Tool>, input: &str) -> ToolOutcome {
    execute_with(
        tool,
        ToolInput::Json(input.into()),
        CancellationToken::new(),
    )
    .await
}

#[tokio::test]
async fn declarations_and_faces() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    let names = [
        "workflow_start",
        "workflow_status",
        "workflow_result",
        "workflow_cancel",
    ];
    for (t, name) in tools(&fake).iter().zip(names) {
        assert_eq!(t.declaration().name, name);
        assert!(!t.declaration().description.is_empty());
        assert_eq!(t.identity().implementation, "p1-tool-workflow");
        assert_eq!(t.identity().variant, "default");
        assert_eq!(
            t.effect(&ToolCall {
                call_id: "call".into(),
                name: name.into(),
                input: ToolInput::Json("{}".into()),
            }),
            Effect::Delegates
        );
        let DeclarationKind::Function { input_schema } = &t.declaration().kind else {
            panic!("workflow tool must be a function");
        };
        assert_eq!(input_schema["additionalProperties"], false);
    }
    let service: Arc<dyn WorkflowService> = fake;
    let face = || ToolFace::new("alias", "custom");
    let faced: Vec<Arc<dyn Tool>> = vec![
        Arc::new(WorkflowStartTool::new(service.clone()).with_face(face(), "v")),
        Arc::new(WorkflowStatusTool::new(service.clone()).with_face(face(), "v")),
        Arc::new(WorkflowResultTool::new(service.clone()).with_face(face(), "v")),
        Arc::new(WorkflowCancelTool::new(service).with_face(face(), "v")),
    ];
    for t in faced {
        assert_eq!(t.declaration().name, "alias");
        assert_eq!(t.declaration().description, "custom");
        assert_eq!(t.identity().variant, "v");
        assert_eq!(t.identity().implementation, "p1-tool-workflow");
    }
}

#[tokio::test]
async fn start_success_resume_and_errors() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    let start = tool(&tools(&fake), "workflow_start");
    assert_eq!(
        execute(&start, r#"{"script":"42","args":{"key":1}}"#)
            .await
            .content,
        "Started workflow wf1. You will be notified when it ends; do not poll."
    );
    assert_eq!(fake.requests.lock().unwrap()[0].args, json!({"key":1}));
    assert!(fake.requests.lock().unwrap()[0].role_models.is_empty());
    assert_eq!(
        execute(&start, r#"{"script":"42","resume_from":"old"}"#)
            .await
            .content,
        "Started workflow wf1, resuming old. You will be notified when it ends; do not poll."
    );
    assert_eq!(fake.requests.lock().unwrap()[1].args, json!({}));
    assert_eq!(
        fake.requests.lock().unwrap()[1].resume_from,
        Some(RunId("old".into()))
    );
    for (error, expected) in [
        (
            WorkflowError::Parse {
                message: "bad token".into(),
                line: 3,
                column: 8,
            },
            "Script does not parse: bad token [line 3, column 8]",
        ),
        (
            WorkflowError::Preflight("unknown role".into()),
            "Cannot start workflow: unknown role",
        ),
        (
            WorkflowError::ShutDown,
            "Cannot start workflow: the workflow service has shut down.",
        ),
        (
            WorkflowError::Io("disk full".into()),
            "Cannot start workflow: disk full",
        ),
    ] {
        fake.starts.lock().unwrap().push_back(Err(error));
        let output = execute(&start, r#"{"script":"42"}"#).await;
        assert_eq!(output.status, ToolStatus::Error);
        assert_eq!(output.content, expected);
    }
}

#[tokio::test]
async fn status_and_result_rendering() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    let tools = tools(&fake);
    let status = tool(&tools, "workflow_status");
    let result = tool(&tools, "workflow_result");
    let running = "Workflow wf1: running — phase review, 3 steps started, 2 ended, 1 replayed\n  first\n  second";
    assert_eq!(execute(&status, r#"{"id":"wf1"}"#).await.content, running);
    assert_eq!(execute(&result, r#"{"id":"wf1"}"#).await.content, running);

    fake.set_status(RunStatus::Ended(report()));
    let first = "Workflow wf1: completed with issues — 2 steps (1 replayed): 1 done, 0 blocked, 1 failed, 0 cancelled; 1 not verified; 1 capped; 0 invalid output; 0 fell back";
    assert_eq!(execute(&status, r#"{"id":"wf1"}"#).await.content, first);
    assert_eq!(
        execute(&result, r#"{"id":"wf1"}"#).await.content,
        format!(
            "{first}\nerror: script error\nsteps:\n  review reviewer → route/model [w3 (route/model)] done — schema passed; not verified; parent verification required; replayed; attempts 2\n  c2 judge → other/model failed — schema not requested; quota_exceeded: cap 3\nresult:\n{{\n  \"answer\": 42\n}}\nrun dir: /runs/wf1"
        )
    );
}

#[tokio::test]
async fn wait_is_released_or_cancelled() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    let result = tool(&tools(&fake), "workflow_result");
    let task = tokio::spawn({
        let result = result.clone();
        async move { execute(&result, r#"{"id":"wf1","wait":true}"#).await }
    });
    fake.wait_entered.notified().await;
    fake.set_status(RunStatus::Ended(report()));
    fake.release_wait.notify_one();
    assert!(task.await.unwrap().content.contains("result:\n"));

    fake.set_status(RunStatus::Running(progress()));
    let cancel = CancellationToken::new();
    let task = tokio::spawn({
        let result = result.clone();
        let cancel = cancel.clone();
        async move {
            execute_with(
                &result,
                ToolInput::Json(r#"{"id":"wf1","wait":true}"#.into()),
                cancel,
            )
            .await
        }
    });
    fake.wait_entered.notified().await;
    cancel.cancel();
    assert_eq!(
        task.await.unwrap(),
        ToolOutcome {
            status: ToolStatus::Cancelled,
            content: String::new()
        }
    );
}

#[tokio::test]
async fn result_bounds_steps_and_value() {
    let fake = FakeService::new(RunStatus::Ended(report()));
    let mut big = report();
    big.steps = vec![big.steps[0].clone(); 203];
    big.value = json!("€".repeat(6000));
    fake.set_status(RunStatus::Ended(big));
    let result = execute(&tool(&tools(&fake), "workflow_result"), r#"{"id":"wf1"}"#).await;
    assert_eq!(result.content.matches("review reviewer →").count(), 200);
    assert!(
        result
            .content
            .contains("\n  … 3 more in result.json\nresult:\n")
    );
    assert!(
        result
            .content
            .contains("… (truncated; full value in /runs/wf1/result.json)")
    );
    assert!(result.content.ends_with("\nrun dir: /runs/wf1"));
}

#[tokio::test]
async fn cancel_and_unknown_ids() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    let tools = tools(&fake);
    assert_eq!(
        execute(&tool(&tools, "workflow_cancel"), r#"{"id":"wf1"}"#)
            .await
            .content,
        "Workflow wf1 cancelled."
    );
    *fake.status.lock().unwrap() = Err(WorkflowError::UnknownRun);
    *fake.cancel.lock().unwrap() = Err(WorkflowError::UnknownRun);
    for name in ["workflow_status", "workflow_result", "workflow_cancel"] {
        let outcome = execute(&tool(&tools, name), r#"{"id":"missing"}"#).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(outcome.content, "No workflow missing.");
    }
}

#[tokio::test]
async fn invalid_json_and_text_are_tool_errors() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    for tool in tools(&fake) {
        let name = &tool.declaration().name;
        for input in ["", "null", "[]", "not json", r#"{"extra":1}"#] {
            let outcome = execute(&tool, input).await;
            assert_eq!(outcome.status, ToolStatus::Error);
            assert!(
                outcome
                    .content
                    .starts_with(&format!("Invalid input for {name}: "))
            );
        }
        let outcome = execute_with(
            &tool,
            ToolInput::Text("freeform".into()),
            CancellationToken::new(),
        )
        .await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert_eq!(
            outcome.content,
            format!("Invalid input for {name}: expected a JSON object input, got freeform text")
        );
    }
    let start = tool(&tools(&fake), "workflow_start");
    for input in [
        r#"{"script":"x","args":[]}"#,
        r#"{"script":"x","args":null}"#,
        r#"{"script":"x","resume_from":3}"#,
    ] {
        assert_eq!(execute(&start, input).await.status, ToolStatus::Error);
    }
}

/// ADR-0057: each workflow tool describes its own call's target — the run id, or
/// (for a start) the resumed id or the script's first line.
#[test]
fn describe_names_the_run_or_the_script() {
    let fake = FakeService::new(RunStatus::Running(progress()));
    let tools = tools(&fake);
    let describe = |name: &str, input: serde_json::Value| {
        let call = ToolCall {
            call_id: "c".into(),
            name: name.into(),
            input: ToolInput::Json(input.to_string()),
        };
        tool(&tools, name).describe(&call)
    };

    let start = describe(
        "workflow_start",
        json!({ "script": "phase(\"one\");\nphase(\"two\");" }),
    );
    assert_eq!(start.verb, "workflow");
    assert_eq!(start.target.as_deref(), Some("phase(\"one\");"));

    let resumed = describe(
        "workflow_start",
        json!({ "script": "x", "resume_from": "wf0" }),
    );
    assert_eq!(resumed.target.as_deref(), Some("wf0"));

    for (name, id) in [
        ("workflow_status", "wf1"),
        ("workflow_result", "wf2"),
        ("workflow_cancel", "wf3"),
    ] {
        let described = describe(name, json!({ "id": id }));
        assert_eq!(
            (described.verb, described.target.as_deref()),
            ("workflow", Some(id))
        );
    }
}
