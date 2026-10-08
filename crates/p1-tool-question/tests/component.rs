//! Production-loader end-to-end tests, isolated from real user data.
use p1_contracts::{
    BoxFuture, CancellationToken, Tool, ToolCall, ToolContext, ToolInput, ToolStatus,
};
use p1_module_runtime::questions::{Asked, Question, UserQuestionsService};
use p1_module_runtime::{ExecutionLimits, Loader, ReleaseManifest, Services, wasm_tool};
use serde_json::{Value, json};
use std::path::Path;
use std::sync::Arc;
struct Headless;
impl UserQuestionsService for Headless {
    fn ask(&self, _: Vec<Question>, _: CancellationToken) -> BoxFuture<'_, Asked> {
        Box::pin(async { Asked::NoInteractiveUser })
    }
}
struct Uninvited;
impl UserQuestionsService for Uninvited {
    fn ask(&self, _: Vec<Question>, _: CancellationToken) -> BoxFuture<'_, Asked> {
        Box::pin(async { Asked::NotInvited })
    }
}
fn call() -> ToolCall {
    ToolCall { call_id: "q1".into(), name: "ask_user_question".into(), input: ToolInput::Json(json!({"questions":[{"question":"Choose", "header":"Choice", "options":[{"label":"A","description":"First"},{"label":"B","description":"Second"}]}]}).to_string()) }
}
fn component(
    service: Arc<dyn UserQuestionsService>,
    limits: ExecutionLimits,
) -> (
    Arc<dyn Tool>,
    tempfile::TempDir,
    p1_module_runtime::ManualEpochs,
) {
    let artifact = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../modules/target/p1-modules/p1-module-question");
    let dir = tempfile::tempdir().unwrap();
    let manifest: Value = serde_json::from_str(
        &std::fs::read_to_string(artifact.join("p1-module-question.manifest.json"))
            .expect("build modules first"),
    )
    .unwrap();
    let bytes = std::fs::read(artifact.join("p1-module-question.wasm")).unwrap();
    std::fs::write(dir.path().join("question.wasm"), bytes).unwrap();
    let entry = json!({"name":manifest["name"],"digest":manifest["digest"],"path":"question.wasm","kind":manifest["kind"],"world":manifest["world"],"protocol":manifest["protocol"],"capabilities":manifest["capabilities"],"variant":manifest["variant"]});
    std::fs::write(
        dir.path().join("manifest.json"),
        json!({"format":"p1-release-manifest/1","components":[entry]}).to_string(),
    )
    .unwrap();
    let (loader, epochs) = Loader::with_manual_epochs(
        ReleaseManifest::read(&dir.path().join("manifest.json")).unwrap(),
        dir.path(),
    )
    .unwrap();
    let module = loader.load("p1/ask-user-question").unwrap();
    let tool = wasm_tool(
        &module,
        Services {
            user_questions: Some(service),
            ..Services::default()
        },
        limits,
        &Arc::new(p1_redact::MaskCounter::new()),
    )
    .unwrap();
    (tool, dir, epochs)
}
#[tokio::test(flavor = "multi_thread")]
async fn headless_refusal_and_injected_answers_end_to_end() {
    let (tool, _dir, _epochs) = component(Arc::new(Headless), ExecutionLimits::default());
    assert_eq!(tool.effect(&call()), p1_contracts::Effect::ReadOnly);
    let result = tool
        .execute(
            &call(),
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        "no interactive user — decide without asking, or end the turn with the question"
    );
    let mut injected = call();
    let ToolInput::Json(raw) = &mut injected.input else {
        unreachable!()
    };
    let mut input: Value = serde_json::from_str(raw).unwrap();
    input["answers"] = json!(["A"]);
    *raw = input.to_string();
    let result = tool
        .execute(
            &injected,
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert_eq!(result.status, ToolStatus::Error);
    assert!(result.content.contains("unknown field"));
}
struct Collected;
impl UserQuestionsService for Collected {
    fn ask(&self, _: Vec<Question>, _: CancellationToken) -> BoxFuture<'_, Asked> {
        Box::pin(async {
            Asked::Answered(vec![p1_module_runtime::questions::Answer {
                chosen: vec!["B".into()],
                free_text: Some("user note".into()),
            }])
        })
    }
}
#[tokio::test]
async fn native_and_component_format_only_host_collected_answers() {
    let (module, _dir, _epochs) = component(Arc::new(Collected), ExecutionLimits::default());
    let native = p1_tool_question::QuestionTool::new(Arc::new(Collected));
    let a = module
        .execute(
            &call(),
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    let b = native
        .execute(
            &call(),
            ToolContext {
                cancel: CancellationToken::new(),
            },
        )
        .await;
    assert_eq!(a, b);
    assert_eq!(a.status, ToolStatus::Ok);
    assert_eq!(a.content, "Choice: B; user note");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let result = native.execute(&call(), ToolContext { cancel }).await;
    assert_eq!(result.status, ToolStatus::Cancelled);
    assert_eq!(result.content, "cancelled — no answer");
}

#[test]
fn schema_and_authoritative_host_validation_agree_on_nested_bounds() {
    let schema = p1_question_guest::input_schema();
    assert_eq!(schema["additionalProperties"], false);
    assert_eq!(schema["properties"]["questions"]["minItems"], 1);
    assert_eq!(schema["properties"]["questions"]["maxItems"], 4);
    let item = &schema["properties"]["questions"]["items"];
    assert_eq!(item["additionalProperties"], false);
    assert_eq!(
        item["properties"]["options"]["items"]["additionalProperties"],
        false
    );
    let ToolInput::Json(raw) = call().input else {
        unreachable!()
    };
    let guest = p1_question_guest::parse(&raw).unwrap();
    let valid = guest
        .into_iter()
        .map(|q| Question {
            question: q.question,
            header: q.header,
            multi_select: q.multi_select,
            options: q
                .options
                .into_iter()
                .map(|o| p1_module_runtime::questions::QuestionOption {
                    label: o.label,
                    description: o.description,
                    preview: o.preview,
                })
                .collect(),
        })
        .collect::<Vec<_>>();
    for n in 0..14 {
        let mut q = valid[0].clone();
        match n {
            0 => q.question.clear(),
            1 => q.question = "é".repeat(1001),
            2 => q.header.clear(),
            3 => q.header = "é".repeat(13),
            4 => q.options.truncate(1),
            5 => q.options = vec![q.options[0].clone(); 5],
            6 => q.options[0].label.clear(),
            7 => q.options[0].label = "B".into(),
            8 => q.options[0].label = "Other".into(),
            9 => q.options[0].description.clear(),
            10 => q.options[0].description = "é".repeat(1001),
            11 => q.options[0].preview = Some("é".repeat(4001)),
            12 => {
                q.multi_select = true;
                q.options[0].preview = Some("preview".into());
            }
            13 => q.options[0].label = "é".repeat(1001),
            _ => unreachable!(),
        }
        let host = p1_module_runtime::questions::validate(std::slice::from_ref(&q));
        let options = q
            .options
            .iter()
            .map(|o| {
                let mut value = json!({"label":o.label,"description":o.description});
                if let Some(preview) = &o.preview {
                    value["preview"] = json!(preview);
                }
                value
            })
            .collect::<Vec<_>>();
        let guest = p1_question_guest::parse(&json!({"questions":[{"question":q.question,"header":q.header,"options":options,"multi_select":q.multi_select}]}).to_string());
        assert_eq!(
            host.unwrap_err().to_string(),
            guest.unwrap_err(),
            "mutation {n}"
        );
    }
}

#[test]
fn guest_bound_is_the_normal_output_bound() {
    for text in ["short\n".into(), "é".repeat(30_000), "line\n".repeat(2100)] {
        assert_eq!(
            p1_question_guest::bound_output(&text),
            p1_workspace::bound_output(&text, 50_000, 2000)
        );
    }
}

struct Pending {
    started: tokio::sync::mpsc::UnboundedSender<()>,
}
impl UserQuestionsService for Pending {
    fn ask(&self, _: Vec<Question>, _: CancellationToken) -> BoxFuture<'_, Asked> {
        Box::pin(async {
            self.started.send(()).unwrap();
            std::future::pending().await
        })
    }
}
#[tokio::test(flavor = "multi_thread")]
async fn silent_component_waits_without_deadline_and_cancellation_never_answers() {
    let (started, mut seen) = tokio::sync::mpsc::unbounded_channel();
    let (tool, _dir, epochs) = component(
        Arc::new(Pending { started }),
        ExecutionLimits {
            deadline: std::time::Duration::from_millis(10),
            ..ExecutionLimits::default()
        },
    );
    let cancel = CancellationToken::new();
    let job = tokio::spawn({
        let cancel = cancel.clone();
        async move { tool.execute(&call(), ToolContext { cancel }).await }
    });
    seen.recv().await.unwrap();
    epochs.advance(1000);
    assert!(!job.is_finished());
    cancel.cancel();
    let result = job.await.unwrap();
    assert_eq!(result.status, ToolStatus::Cancelled);
    assert!(!result.content.contains("Choice:"));
}

/// ADR-0135: the host's refusal reaches the model verbatim through the component and the
/// native adapter, and the guest's copy of the text is the host's.
#[tokio::test(flavor = "multi_thread")]
async fn an_uninvited_call_is_refused_with_the_host_text() {
    assert_eq!(
        p1_question_guest::NOT_INVITED,
        p1_module_runtime::questions::NOT_INVITED
    );
    let (component, _dir, _epochs) = component(Arc::new(Uninvited), ExecutionLimits::default());
    let native: Arc<dyn Tool> = Arc::new(p1_tool_question::QuestionTool::new(Arc::new(Uninvited)));
    for tool in [component, native] {
        let result = tool
            .execute(
                &call(),
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(result.status, ToolStatus::Error);
        assert_eq!(result.content, p1_module_runtime::questions::NOT_INVITED);
    }
}
