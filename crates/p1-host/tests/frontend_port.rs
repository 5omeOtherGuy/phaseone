//! The front-end port (D7, ADR-0152): a fake adapter in this file implements
//! `p1_contracts::frontend::FrontEndPort`, the host plugs it in through
//! `PortFrontEnd`, and the adapter drives the session through the host's
//! `SessionHandle` on scripted providers.
//!
//! No network, tempdirs only, no sleeps: every wait is on a signal the host sends.

mod common;
#[cfg(feature = "workflows")]
mod workflow_common;

use std::sync::{Arc, Mutex};
#[cfg(feature = "workflows")]
use std::time::Duration;

use common::{Harness, provider_hook, write_environment};
use p1_contracts::frontend::{
    BackgroundKind, BackgroundPhase, BackgroundSignal, FrontEndPort, SessionHandle,
};
use p1_contracts::{
    AgentEvent, AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision,
    EventSink, Item, TurnEnd,
};
use p1_host::frontend::FrontEnd;
use p1_host::frontend_port::PortFrontEnd;
use p1_host::run::run_with_front_end;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

struct NullSink;

impl EventSink for NullSink {
    fn emit(&self, _event: AgentEvent) {}
}

struct PermitAll;

impl AuthorizationPolicy for PermitAll {
    fn authorize<'a>(&'a self, _request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async { Decision::Permit })
    }
}

/// What the fake adapter does with the session once the host hands it over.
#[derive(Clone, Copy)]
enum Script {
    /// One prompt; nothing else.
    Prompt,
    /// One prompt that starts a worker and a workflow, then cancel both, then drain.
    #[cfg(feature = "workflows")]
    PromptCancelDrain,
}

/// The fake adapter: records every background signal and what each session call
/// returned.
struct FakePort {
    script: Script,
    signals: Mutex<Vec<BackgroundSignal>>,
    arrived: tokio::sync::Notify,
    prompt_end: Mutex<Option<TurnEnd>>,
    #[cfg_attr(
        not(feature = "workflows"),
        allow(dead_code, reason = "read by the workflow test only")
    )]
    drain_end: Mutex<Option<Option<TurnEnd>>>,
}

impl FakePort {
    fn new(script: Script) -> Arc<Self> {
        Arc::new(Self {
            script,
            signals: Mutex::new(Vec::new()),
            arrived: tokio::sync::Notify::new(),
            prompt_end: Mutex::new(None),
            drain_end: Mutex::new(None),
        })
    }

    fn signals(&self) -> Vec<BackgroundSignal> {
        self.signals.lock().unwrap().clone()
    }

    #[cfg_attr(
        not(feature = "workflows"),
        allow(dead_code, reason = "read by the workflow test only")
    )]
    fn has(&self, phase: BackgroundPhase, kind: BackgroundKind, id: &str) -> bool {
        self.signals()
            .iter()
            .any(|signal| signal.phase == phase && signal.kind == kind && signal.id == id)
    }

    /// Waits until `done` holds. `notify_one` keeps a permit when nobody waits, so a
    /// signal between the check and the wait is not lost.
    #[cfg(feature = "workflows")]
    async fn until(&self, done: impl Fn(&Self) -> bool) {
        while !done(self) {
            self.arrived.notified().await;
        }
    }

    #[cfg(feature = "workflows")]
    async fn prompt_cancel_drain(&self, session: &dyn SessionHandle) {
        let cancel = CancellationToken::new();
        *self.prompt_end.lock().unwrap() =
            Some(session.prompt("go".to_string(), cancel.clone()).await);
        use BackgroundKind::{Worker, Workflow};
        use BackgroundPhase::{Ended, Started};
        // w1 is the direct worker, wf1 the run and w2 its step's worker.
        self.until(|port| {
            port.has(Started, Worker, "w1")
                && port.has(Started, Workflow, "wf1")
                && port.has(Started, Worker, "w2")
        })
        .await;
        session.cancel_runs().await;
        session.stop_workers().await;
        self.until(|port| {
            port.has(Ended, Worker, "w1")
                && port.has(Ended, Workflow, "wf1")
                && port.has(Ended, Worker, "w2")
        })
        .await;
        // The notices reach the inbox after the signals, from the run's and the
        // worker's own tasks: wait for the first one.
        session.inbox_ready().await;
        *self.drain_end.lock().unwrap() = Some(session.drain_inbox(cancel).await);
    }
}

impl FrontEndPort for FakePort {
    fn event_sink(&self) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }

    fn child_event_sink(&self, _worker_id: &str) -> Arc<dyn EventSink> {
        Arc::new(NullSink)
    }

    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        Arc::new(PermitAll)
    }

    fn background(&self, signal: BackgroundSignal) {
        self.signals.lock().unwrap().push(signal);
        self.arrived.notify_one();
    }

    fn run<'a>(&'a self, session: &'a dyn SessionHandle) -> BoxFuture<'a, i32> {
        Box::pin(async move {
            match self.script {
                Script::Prompt => {
                    let end = session
                        .prompt("go".to_string(), CancellationToken::new())
                        .await;
                    *self.prompt_end.lock().unwrap() = Some(end);
                }
                #[cfg(feature = "workflows")]
                Script::PromptCancelDrain => self.prompt_cancel_drain(session).await,
            }
            0
        })
    }
}

fn options(
    env: &str,
    workspace: &std::path::Path,
    session: Option<&std::path::Path>,
) -> p1_host::cli::Options {
    let mut args = vec![
        "--yes".to_string(),
        "--env".to_string(),
        env.to_string(),
        "--workspace".to_string(),
        workspace.to_str().unwrap().to_string(),
    ];
    if let Some(session) = session {
        args.extend([
            "--session".to_string(),
            session.to_str().unwrap().to_string(),
        ]);
    }
    p1_host::cli::parse(&args).unwrap()
}

/// Cancel reaches both hooks: the run and both workers end, each signalled once with
/// the turn that started it, and the drain then runs the inbox turn their notices open.
#[cfg(feature = "workflows")]
#[tokio::test]
async fn frontend_port_cancel_calls_both_hooks_and_drain_runs_the_inbox_turn() {
    use p1_testkit::Step;
    use workflow_common::{Fakes, Scratch};

    tokio::time::timeout(Duration::from_secs(60), async {
        let scratch = Scratch::new();
        let script = serde_json::json!({ "script": r#"agent("wait here", #{ label: "one" })"# });
        let fakes = Fakes::new(
            vec![
                tool_call_response(vec![json_call(
                    "c1",
                    "worker_start",
                    r#"{"environment":"fake","task":"wait here","tools":["read"]}"#,
                )]),
                tool_call_response(vec![json_call("c2", "workflow_start", &script.to_string())]),
                text_response("both running"),
                text_response("noted"),
                text_response("noted"),
                text_response("noted"),
            ],
            // Both the direct worker and the step's worker wait until cancelled.
            vec![
                Step::EventsThenAwaitCancel(vec![]),
                Step::EventsThenAwaitCancel(vec![]),
            ],
            Vec::new(),
        );
        let mut harness = scratch.harness();
        harness.deps.catalog_hook = Some(fakes.hook());
        let port = FakePort::new(Script::PromptCancelDrain);

        let code = run_with_front_end(
            &mut harness.deps,
            &options("parent", scratch.workspace.path(), Some(&scratch.session())),
            CancellationToken::new(),
            Arc::new(PortFrontEnd::new(port.clone())) as Arc<dyn FrontEnd>,
        )
        .await
        .unwrap();
        assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

        assert!(
            matches!(
                *port.prompt_end.lock().unwrap(),
                Some(TurnEnd::Completed { .. })
            ),
            "stderr: {}",
            harness.stderr.text()
        );
        let signals = port.signals();
        for (kind, id) in [
            (BackgroundKind::Worker, "w1"),
            (BackgroundKind::Workflow, "wf1"),
            (BackgroundKind::Worker, "w2"),
        ] {
            let of: Vec<_> = signals
                .iter()
                .filter(|signal| signal.kind == kind && signal.id == id)
                .collect();
            assert_eq!(of.len(), 2, "one start and one end for {id}: {signals:?}");
            assert_eq!(of[0].phase, BackgroundPhase::Started, "{signals:?}");
            assert_eq!(of[1].phase, BackgroundPhase::Ended, "{signals:?}");
            assert_eq!(of[1].turn, of[0].turn, "an end carries its start's turn");
        }
        // The prompt is the session's first turn; the direct worker and the run start in it.
        for id in ["w1", "wf1"] {
            let start = signals.iter().find(|signal| signal.id == id).unwrap();
            assert_eq!(start.turn, Some(1), "{signals:?}");
        }

        let drained = port.drain_end.lock().unwrap().clone();
        assert!(
            matches!(drained, Some(Some(TurnEnd::Completed { .. }))),
            "the drain runs the inbox turn: {drained:?}"
        );
        let requests = fakes.parent.requests();
        assert!(requests.len() >= 4, "an inbox turn reached the model");
        let notices = format!("{:?}", &requests[3].history);
        assert!(
            notices.contains("Workflow wf1 ended (cancelled)")
                || notices.contains("Worker w1 finished"),
            "the inbox turn carries a notice: {notices}"
        );
    })
    .await
    .expect("the port session hung");
}

/// `worker_continue` runs the ended worker again under its id: a second start and end,
/// both in the prompt's turn.
#[cfg(feature = "delegation")]
#[tokio::test]
async fn frontend_port_signals_a_continued_worker_again() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result", "worker_continue"],
        "PARENT",
    );
    write_environment(
        environments.path(),
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "CHILD",
    );
    // `wait` lets each child turn end before the parent goes on: no timing assumption.
    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"b","task":"one","tools":["read"]}"#,
        )]),
        tool_call_response(vec![json_call(
            "c2",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        tool_call_response(vec![json_call(
            "c3",
            "worker_continue",
            r#"{"id":"w1","message":"two"}"#,
        )]),
        tool_call_response(vec![json_call(
            "c4",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        text_response("parent done"),
    ]);
    let child = ScriptedProvider::new(vec![text_response("first"), text_response("second")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));
    let port = FakePort::new(Script::Prompt);

    let code = run_with_front_end(
        &mut harness.deps,
        &options("a", workspace.path(), None),
        CancellationToken::new(),
        Arc::new(PortFrontEnd::new(port.clone())) as Arc<dyn FrontEnd>,
    )
    .await
    .unwrap();
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    use BackgroundPhase::{Ended, Started};
    let phases: Vec<_> = port
        .signals()
        .iter()
        .map(|signal| (signal.phase, signal.kind, signal.id.clone(), signal.turn))
        .collect();
    let worker = |phase| (phase, BackgroundKind::Worker, "w1".to_string(), Some(1));
    assert_eq!(
        phases,
        [
            worker(Started),
            worker(Ended),
            worker(Started),
            worker(Ended)
        ],
        "stderr: {}",
        harness.stderr.text()
    );
}

/// A background shell job is part of the turn's tool calls: no background signal.
#[tokio::test]
async fn frontend_port_signals_nothing_for_a_shell_job() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["shell"],
        "test",
    );
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "shell",
            r#"{"command":"echo job-marker","background":true}"#,
        )]),
        text_response("job started"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let port = FakePort::new(Script::Prompt);

    let code = run_with_front_end(
        &mut harness.deps,
        &options("plain", workspace.path(), None),
        CancellationToken::new(),
        Arc::new(PortFrontEnd::new(port.clone())) as Arc<dyn FrontEnd>,
    )
    .await
    .unwrap();
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    assert_eq!(requests.len(), 2, "the prompt ran its turn");
    let start = requests[1]
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("the start call's result");
    assert!(start.contains("j1"), "the job started: {start}");
    assert!(port.signals().is_empty(), "{:?}", port.signals());
}
