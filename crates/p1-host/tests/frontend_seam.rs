//! The front-end seam (issue #12).
//!
//! A recording fake proves that the host installs a custom event sink and
//! authorization policy for the PARENT agent and for every delegated worker,
//! keeps the line front end's worker-usage aggregate, and hands the run loop
//! over. The custom run-loop test also compiles and runs with
//! `--no-default-features`.
//!
//! No network, no real home directory, no sleeps.

mod common;

use std::sync::{Arc, Mutex};

use common::{Harness, provider_hook, write_environment};
use p1_contracts::{
    AgentEvent, AuthorizationPolicy, AuthorizationRequest, BoxFuture, CancellationToken, Decision,
    EventSink,
};
use p1_core::Agent;
use p1_host::HostDeps;
use p1_host::frontend::{FrontEnd, WorkerService};
use p1_host::run::{StallGuard, run_with_front_end};
use p1_testkit::{ScriptedProvider, text_response};
#[cfg(feature = "delegation")]
use p1_testkit::{json_call, tool_call_response};
use tempfile::tempdir;

#[cfg(feature = "delegation")]
use p1_host::frontend::LineFrontEnd;
#[cfg(feature = "delegation")]
use std::collections::HashMap;
#[cfg(feature = "delegation")]
use std::sync::atomic::{AtomicBool, Ordering};

/// Records every event it is given.
struct RecordingSink {
    events: Mutex<Vec<AgentEvent>>,
}

impl RecordingSink {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(Vec::new()),
        })
    }

    fn texts(&self) -> Vec<String> {
        self.events
            .lock()
            .unwrap()
            .iter()
            .filter_map(|event| match event {
                AgentEvent::TextDelta { text } => Some(text.clone()),
                _ => None,
            })
            .collect()
    }

    fn has_text(&self, needle: &str) -> bool {
        self.texts().iter().any(|text| text.contains(needle))
    }
}

impl EventSink for RecordingSink {
    fn emit(&self, event: AgentEvent) {
        self.events.lock().unwrap().push(event);
    }
}

/// Permits everything and records the tool names it was asked about.
struct RecordingPolicy {
    requests: Mutex<Vec<String>>,
}

impl RecordingPolicy {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(Vec::new()),
        })
    }

    #[cfg(feature = "delegation")]
    fn asks(&self) -> Vec<String> {
        self.requests.lock().unwrap().clone()
    }
}

impl AuthorizationPolicy for RecordingPolicy {
    fn authorize<'a>(&'a self, request: AuthorizationRequest<'a>) -> BoxFuture<'a, Decision> {
        Box::pin(async move {
            self.requests
                .lock()
                .unwrap()
                .push(request.call.name.clone());
            Decision::Permit
        })
    }
}

// ------------------------------------------------------- recording line front end

/// Forwards to the recording sink AND to the line renderer, so the line front
/// end still prints the usage lines while the fake observes.
#[cfg(feature = "delegation")]
struct TeeSink {
    recorder: Arc<RecordingSink>,
    inner: Arc<dyn EventSink>,
}

#[cfg(feature = "delegation")]
impl EventSink for TeeSink {
    fn emit(&self, event: AgentEvent) {
        self.recorder.emit(event.clone());
        self.inner.emit(event);
    }
}

/// The recording fake. It observes the parent and each worker's events and the
/// authorization requests, and delegates the run loop, the parent assembly
/// announcement, the child count and the finish to a real [`LineFrontEnd`], so
/// today's rendering and worker-usage behaviour is exercised end to end.
#[cfg(feature = "delegation")]
struct RecordingFrontEnd {
    inner: LineFrontEnd,
    parent: Arc<RecordingSink>,
    children: Mutex<HashMap<String, Arc<RecordingSink>>>,
    policy: Arc<RecordingPolicy>,
    ran: AtomicBool,
}

#[cfg(feature = "delegation")]
impl RecordingFrontEnd {
    fn new(inner: LineFrontEnd) -> Self {
        Self {
            inner,
            parent: RecordingSink::new(),
            children: Mutex::new(HashMap::new()),
            policy: RecordingPolicy::new(),
            ran: AtomicBool::new(false),
        }
    }

    fn child(&self, worker_id: &str) -> Arc<RecordingSink> {
        self.children
            .lock()
            .unwrap()
            .entry(worker_id.to_string())
            .or_insert_with(RecordingSink::new)
            .clone()
    }
}

#[cfg(feature = "delegation")]
impl FrontEnd for RecordingFrontEnd {
    fn event_sink(&self) -> Arc<dyn EventSink> {
        Arc::new(TeeSink {
            recorder: self.parent.clone(),
            inner: self.inner.event_sink(),
        })
    }

    fn child_event_sink(&self, worker_id: &str, route: &str, model: &str) -> Arc<dyn EventSink> {
        Arc::new(TeeSink {
            recorder: self.child(worker_id),
            inner: self.inner.child_event_sink(worker_id, route, model),
        })
    }

    fn child_started(&self, worker_id: &str) {
        self.inner.child_started(worker_id);
    }

    /// A worker's end goes to the real line front end, exactly as the run loop and
    /// the child count do.
    #[cfg(feature = "delegation")]
    fn worker_ended(&self, worker_id: &str, description: &str, report: &p1_workers::WorkerReport) {
        self.inner.worker_ended(worker_id, description, report);
    }

    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        self.policy.clone()
    }

    fn parent_assembled(
        &self,
        route: &str,
        model: &str,
        completion: Option<p1_host::activity::Completion>,
    ) {
        self.inner.parent_assembled(route, model, completion);
    }

    fn run<'a>(
        &'a self,
        deps: &'a HostDeps,
        agent: &'a mut Agent,
        cancel: &'a CancellationToken,
        workers: Option<Arc<dyn WorkerService>>,
        stall: Option<Arc<StallGuard>>,
    ) -> BoxFuture<'a, i32> {
        self.ran.store(true, Ordering::SeqCst);
        self.inner.run(deps, agent, cancel, workers, stall)
    }

    fn finish(&self) {
        self.inner.finish();
    }
}

// ------------------------------------------------------------- custom run loop

/// The minimal custom front end: it drives one turn itself and returns a
/// sentinel code, proving the host's run-loop hand-off receives a usable agent
/// and propagates its exit code.
struct SentinelFrontEnd {
    sink: Arc<RecordingSink>,
    policy: Arc<RecordingPolicy>,
    code: i32,
}

impl FrontEnd for SentinelFrontEnd {
    fn event_sink(&self) -> Arc<dyn EventSink> {
        self.sink.clone()
    }

    fn child_event_sink(&self, _worker_id: &str, _route: &str, _model: &str) -> Arc<dyn EventSink> {
        RecordingSink::new()
    }

    fn child_started(&self, _worker_id: &str) {}

    /// This front end renders nothing, so a worker's end is a no-op.
    #[cfg(feature = "delegation")]
    fn worker_ended(
        &self,
        _worker_id: &str,
        _description: &str,
        _report: &p1_workers::WorkerReport,
    ) {
    }

    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        self.policy.clone()
    }

    fn parent_assembled(
        &self,
        _route: &str,
        _model: &str,
        _completion: Option<p1_host::activity::Completion>,
    ) {
    }

    fn run<'a>(
        &'a self,
        _deps: &'a HostDeps,
        agent: &'a mut Agent,
        cancel: &'a CancellationToken,
        _workers: Option<Arc<dyn WorkerService>>,
        _stall: Option<Arc<StallGuard>>,
    ) -> BoxFuture<'a, i32> {
        Box::pin(async move {
            let _ = agent.run_turn("go".to_string(), cancel.clone()).await;
            self.code
        })
    }

    fn finish(&self) {}
}

// --------------------------------------------------------------- the tests

#[cfg(feature = "delegation")]
#[tokio::test]
async fn recording_front_end_sees_parent_and_worker_events_and_worker_authorization() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "a",
        "fake-a",
        "model-a",
        &["worker_start", "worker_result"],
        "PARENT PROMPT",
    );
    write_environment(
        environments.path(),
        "b",
        "fake-b",
        "model-b",
        &["read"],
        "CHILD PROMPT",
    );
    std::fs::write(workspace.path().join("input.txt"), "hello").unwrap();

    let parent = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "worker_start",
            r#"{"environment":"b","task":"do it","tools":["read"]}"#,
        )]),
        // `wait` makes the child finish before the parent continues: no gate and
        // no timing assumption (the shape used by `run_worker_write`).
        tool_call_response(vec![json_call(
            "c2",
            "worker_result",
            r#"{"id":"w1","wait":true}"#,
        )]),
        text_response("parent done"),
        text_response("parent notified"),
    ]);
    let child = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "c1",
            "read",
            r#"{"file_path":"input.txt"}"#,
        )]),
        text_response("child done"),
    ]);

    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake-a", parent), ("fake-b", child)]));

    let options = p1_host::cli::parse(&[
        "--yes".to_string(),
        "--env".to_string(),
        "a".to_string(),
        "--workspace".to_string(),
        workspace.path().to_str().unwrap().to_string(),
        "go".to_string(),
    ])
    .unwrap();
    let cancel = CancellationToken::new();
    let line = LineFrontEnd::new(&harness.deps, &options, cancel.clone());
    let front_end = Arc::new(RecordingFrontEnd::new(line));

    let code = run_with_front_end(
        &mut harness.deps,
        &options,
        cancel,
        front_end.clone() as Arc<dyn FrontEnd>,
    )
    .await
    .unwrap();

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        front_end.ran.load(Ordering::SeqCst),
        "the run-loop hand-off must be invoked"
    );

    let parent_texts = front_end.parent.texts();
    assert!(
        front_end.parent.has_text("parent done"),
        "the parent's events must reach the front end's sink: {parent_texts:?}"
    );
    assert!(
        !front_end.parent.has_text("child done"),
        "the worker's events must NOT reach the parent sink: {parent_texts:?}"
    );

    let worker_texts = front_end.child("w1").texts();
    assert!(
        front_end.child("w1").has_text("child done"),
        "the worker's events must reach child_event_sink(\"w1\"): {worker_texts:?}"
    );

    let requests = front_end.policy.asks();
    assert!(
        requests.iter().any(|name| name == "read"),
        "the worker's authorization must reach the front end's policy: {requests:?}"
    );

    assert!(
        harness.stderr.text().contains("workers total (1)"),
        "the line front end must still print the worker-usage totals: {}",
        harness.stderr.text()
    );
}

#[tokio::test]
async fn custom_run_loop_receives_the_agent_and_returns_its_exit_code() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &["read"],
        "PROMPT",
    );
    let provider = ScriptedProvider::new(vec![text_response("custom loop ran")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));

    let options = p1_host::cli::parse(&[
        "--yes".to_string(),
        "--env".to_string(),
        "plain".to_string(),
        "--workspace".to_string(),
        workspace.path().to_str().unwrap().to_string(),
        "go".to_string(),
    ])
    .unwrap();
    let cancel = CancellationToken::new();
    let front_end = Arc::new(SentinelFrontEnd {
        sink: RecordingSink::new(),
        policy: RecordingPolicy::new(),
        code: 42,
    });

    let code = run_with_front_end(
        &mut harness.deps,
        &options,
        cancel,
        front_end.clone() as Arc<dyn FrontEnd>,
    )
    .await
    .unwrap();

    assert_eq!(
        code, 42,
        "the custom run loop's exit code must be returned verbatim"
    );
    assert_eq!(
        handle.requests().len(),
        1,
        "the custom run loop must be handed a usable agent and drive it"
    );
    assert!(
        front_end.sink.has_text("custom loop ran"),
        "the custom loop's events must reach its own sink: {:?}",
        front_end.sink.texts()
    );
}
