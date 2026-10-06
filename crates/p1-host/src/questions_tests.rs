use super::*;
use crate::policy::{AskBridge, PolicyId, Verdict, VerdictSource};
use p1_contracts::{
    AuthorizationPolicy, AuthorizationRequest, Decision, Effect, ToolCall, ToolIdentity, ToolInput,
};
use tokio::sync::mpsc;

fn question() -> Question {
    Question {
        question: "Choose".into(),
        header: "Choice".into(),
        multi_select: false,
        options: vec![
            p1_module_runtime::questions::QuestionOption {
                label: "A".into(),
                description: "First".into(),
                preview: None,
            },
            p1_module_runtime::questions::QuestionOption {
                label: "B".into(),
                description: "Second".into(),
                preview: None,
            },
        ],
    }
}
struct Lines {
    rx: tokio::sync::Mutex<mpsc::UnboundedReceiver<String>>,
    reads: mpsc::UnboundedSender<()>,
}
impl LineSource for Lines {
    fn next_line(&self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let _ = self.reads.send(());
            self.rx.lock().await.recv().await
        })
    }
}
fn lines() -> (
    Arc<Lines>,
    mpsc::UnboundedSender<String>,
    mpsc::UnboundedReceiver<()>,
) {
    let (tx, rx) = mpsc::unbounded_channel();
    let (reads, seen) = mpsc::unbounded_channel();
    (
        Arc::new(Lines {
            rx: tokio::sync::Mutex::new(rx),
            reads,
        }),
        tx,
        seen,
    )
}
struct Ask;
impl VerdictSource for Ask {
    fn policy(&self) -> PolicyId {
        PolicyId {
            package: "test".into(),
            digest: "test".into(),
        }
    }
    fn verdict<'a>(&'a self, _: AuthorizationRequest<'a>) -> BoxFuture<'a, Verdict> {
        Box::pin(async { Verdict::Ask })
    }
}
#[test]
fn numeric_free_text_and_invalid_selections_are_distinct() {
    assert_eq!(
        parse_line(&question(), "/text 42"),
        Some(Answer {
            chosen: Vec::new(),
            free_text: Some("42".into())
        })
    );
    assert!(parse_line(&question(), "9").is_none());
    assert!(parse_line(&question(), "1,2").is_none());
    assert!(parse_line(&question(), " ").is_none());
}

#[tokio::test]
async fn silence_stays_pending_cancellation_has_no_answer_headless_never_reads() {
    let (lines, _tx, mut seen) = lines();
    let bridge = Arc::new(QuestionBridge::new(
        Some(Arc::new(LineQuestionAsker {
            lines,
            stderr: Arc::new(std::sync::Mutex::new(Box::new(std::io::sink()))),
        })),
        Arc::new(tokio::sync::Mutex::new(())),
    ));
    let cancel = CancellationToken::new();
    let job = tokio::spawn({
        let bridge = bridge.clone();
        let cancel = cancel.clone();
        async move { bridge.ask(vec![question()], cancel).await }
    });
    seen.recv().await.unwrap();
    assert!(!job.is_finished());
    cancel.cancel();
    assert_eq!(job.await.unwrap(), Asked::Cancelled);
    assert_eq!(
        QuestionBridge::headless()
            .ask(vec![question()], CancellationToken::new())
            .await,
        Asked::NoInteractiveUser
    );
}
#[tokio::test]
async fn worker_sets_share_the_frontend_and_disappearance_is_not_an_answer() {
    let (sink, mut events) = p1_tui::runtime::TuiSink::new();
    let bridge = Arc::new(QuestionBridge::new(
        Some(Arc::new(TuiQuestionAsker::new(Arc::new(sink)))),
        Arc::new(tokio::sync::Mutex::new(())),
    ));
    let first = tokio::spawn({
        let b = bridge.clone();
        async move {
            b.for_worker("w1")
                .ask(vec![question()], CancellationToken::new())
                .await
        }
    });
    let p1_tui::runtime::UiEvent::Questions(request) = events.recv().await.unwrap() else {
        panic!("question");
    };
    assert_eq!(request.view.worker.as_deref(), Some("w1"));
    assert!(bridge.gate.try_lock().is_err());
    let second = tokio::spawn({
        let b = bridge.clone();
        async move {
            b.for_worker("w2")
                .ask(vec![question()], CancellationToken::new())
                .await
        }
    });
    assert!(events.try_recv().is_err());
    request.reply.send(None).unwrap();
    assert_eq!(first.await.unwrap(), Asked::Cancelled);
    let p1_tui::runtime::UiEvent::Questions(request) = events.recv().await.unwrap() else {
        panic!("question");
    };
    assert_eq!(request.view.worker.as_deref(), Some("w2"));
    drop(request);
    assert_eq!(second.await.unwrap(), Asked::NoInteractiveUser);
    drop(events);
    assert_eq!(
        bridge.ask(vec![question()], CancellationToken::new()).await,
        Asked::NoInteractiveUser
    );
}

#[tokio::test]
async fn line_answers_and_serializes_questions_with_authorization() {
    let (lines, tx, mut seen) = lines();
    let writer: SharedWriter = Arc::new(std::sync::Mutex::new(Box::new(std::io::sink())));
    let policy = Arc::new(AskBridge::new(
        Arc::new(Ask),
        false,
        lines.clone(),
        writer.clone(),
        CancellationToken::new(),
    ));
    let bridge = Arc::new(QuestionBridge::new(
        Some(Arc::new(LineQuestionAsker {
            lines,
            stderr: writer,
        })),
        policy.prompt_gate.clone(),
    ));
    let job = tokio::spawn({
        let b = bridge.clone();
        async move { b.ask(vec![question()], CancellationToken::new()).await }
    });
    seen.recv().await.unwrap();
    assert!(policy.prompt_gate.try_lock().is_err());
    let authorize = tokio::spawn(async move {
        let call = ToolCall {
            call_id: "auth".into(),
            name: "shell".into(),
            input: ToolInput::Json("{}".into()),
        };
        let identity = ToolIdentity {
            implementation: "test".into(),
            variant: "test".into(),
        };
        policy
            .authorize(AuthorizationRequest {
                call: &call,
                identity: &identity,
                effect: Effect::Executes,
            })
            .await
    });
    tx.send("2".into()).unwrap();
    assert_eq!(
        job.await.unwrap(),
        Asked::Answered(vec![Answer {
            chosen: vec!["B".into()],
            free_text: None
        }])
    );
    seen.recv().await.unwrap();
    tx.send("y".into()).unwrap();
    assert_eq!(authorize.await.unwrap(), Decision::Permit);
    let mut q = question();
    q.multi_select = true;
    let job = tokio::spawn(async move {
        bridge
            .ask(
                vec![q, {
                    let mut q = question();
                    q.question = "Another".into();
                    q
                }],
                CancellationToken::new(),
            )
            .await
    });
    seen.recv().await.unwrap();
    tx.send("2,1".into()).unwrap();
    seen.recv().await.unwrap();
    tx.send("my choice".into()).unwrap();
    assert_eq!(
        job.await.unwrap(),
        Asked::Answered(vec![
            Answer {
                chosen: vec!["A".into(), "B".into()],
                free_text: None
            },
            Answer {
                chosen: vec![],
                free_text: Some("my choice".into())
            }
        ])
    );
}
