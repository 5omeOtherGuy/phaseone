//! The worker capability boundary (issue #266): what a component can reach through the
//! scoped exports of `workers-start`, `workers-observe` and `workers-control`.
//!
//! Drafted here while `crates/p1-module-tests` does not exist yet; it depends only on the
//! public API of `p1-workers`, `p1-core`, `p1-contracts` and `p1-testkit`, so it moves
//! there unchanged. Every ordering is explicit (a gate, a cancel token, a `wait`); nothing
//! sleeps or asserts on time.

use std::sync::{Arc, Mutex};

use p1_contracts::{
    BoxFuture, CancellationToken, ModelOptions, Provider, ProviderError, ProviderRequest,
    ProviderStream, RouteDescription,
};
use p1_core::{Agent, AgentParts};
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    Step, text_response,
};
use p1_workers::{
    AgentFactory, ChildAgent, ChildId, ChildSpec, ChildStatus, InProcessWorkers, ScopeKey,
    WorkerError, WorkerReport, WorkerScopes, WorkerService, WorkersControl, WorkersObserve,
    WorkersStart,
};
use tokio::sync::{Notify, Semaphore};

/// A provider whose every stream waits for one permit of `gate` before it answers, so a
/// child stays `Running` exactly until the test lets it finish.
struct GatedProvider {
    gate: Arc<Semaphore>,
    inner: ScriptedProvider,
}

impl Provider for GatedProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async move {
            // The permit is consumed: one release lets exactly one response through.
            self.gate
                .acquire()
                .await
                .expect("the test never closes the gate")
                .forget();
            self.inner.stream(request, cancel).await
        })
    }
}

/// A provider that waits for its cancel, and hands the test the token its stream got:
/// the turn's own token, so the test sees directly whether anything cancelled the turn.
struct ProbeProvider {
    inner: ScriptedProvider,
    probe: Probe,
}

#[derive(Clone, Default)]
struct Probe {
    token: Arc<Mutex<Option<CancellationToken>>>,
    streaming: Arc<Notify>,
}

impl Provider for ProbeProvider {
    fn describe(&self) -> RouteDescription {
        self.inner.describe()
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        *self.probe.token.lock().unwrap() = Some(cancel.clone());
        // `notify_one` keeps a permit, so the test may start waiting after this.
        self.probe.streaming.notify_one();
        self.inner.stream(request, cancel)
    }
}

fn agent(provider: Arc<dyn Provider>) -> Agent {
    Agent::new(AgentParts {
        provider,
        tools: Vec::new(),
        system_prompt: "child".into(),
        options: ModelOptions::default(),
        context: Arc::new(PassthroughContext),
        authorization: Arc::new(ScriptedAuthorization::permit_all()),
        journal: Arc::new(RecordingJournal::new()),
        events: Arc::new(RecordingEvents::new()),
    })
    .expect("the test agent builds")
}

fn child(provider: Arc<dyn Provider>) -> ChildAgent {
    ChildAgent {
        agent: agent(provider),
        description: "fake/route".into(),
        report: Arc::new(WorkerReport::default),
        regrant: None,
    }
}

/// The fake agent factory: environment `gated` answers once the gate opens, `hang` waits
/// for its cancel (through `probe`), anything else answers at once. `built` counts the
/// children built.
fn factory(gate: Arc<Semaphore>, probe: Probe, built: Arc<Mutex<usize>>) -> AgentFactory {
    Arc::new(move |spec: &ChildSpec| {
        *built.lock().unwrap() += 1;
        let provider: Arc<dyn Provider> = match spec.environment.as_str() {
            "gated" => Arc::new(GatedProvider {
                gate: Arc::clone(&gate),
                inner: ScriptedProvider::new(vec![text_response("done")]),
            }),
            "hang" => Arc::new(ProbeProvider {
                inner: ScriptedProvider::new(vec![Step::EventsThenAwaitCancel(Vec::new())]),
                probe: probe.clone(),
            }),
            _ => Arc::new(ScriptedProvider::new(vec![text_response("done")])),
        };
        Ok(child(provider))
    })
}

struct Harness {
    service: Arc<InProcessWorkers>,
    scopes: WorkerScopes,
    gate: Arc<Semaphore>,
    probe: Probe,
    built: Arc<Mutex<usize>>,
}

fn harness() -> Harness {
    let gate = Arc::new(Semaphore::new(0));
    let probe = Probe::default();
    let built = Arc::new(Mutex::new(0));
    let service = InProcessWorkers::new(
        factory(Arc::clone(&gate), probe.clone(), Arc::clone(&built)),
        8,
    );
    let scopes = WorkerScopes::new(Arc::clone(&service) as Arc<dyn WorkerService>);
    Harness {
        service,
        scopes,
        gate,
        probe,
        built,
    }
}

fn key(generation: u64, operation: &str, parent: &str) -> ScopeKey {
    ScopeKey {
        generation,
        operation: operation.into(),
        parent: parent.into(),
    }
}

fn spec(environment: &str) -> ChildSpec {
    ChildSpec {
        environment: environment.into(),
        task: "do it".into(),
        tools: Vec::new(),
        workspace: None,
    }
}

/// Every scoped read and control of `id` through `scope` is `unknown-child`.
async fn unknown_everywhere(scope: &p1_workers::WorkerScope, id: &ChildId) {
    let unknown = Err(WorkerError::UnknownChild);
    assert_eq!(scope.status(id).await, unknown);
    assert_eq!(scope.result(id).await, unknown);
    assert_eq!(scope.wait(id, CancellationToken::new()).await, unknown);
    assert_eq!(scope.describe(id).await, Err(WorkerError::UnknownChild));
    assert_eq!(scope.cancel(id).await, Err(WorkerError::UnknownChild));
    assert_eq!(
        scope.continue_child(id, "again".into(), Vec::new()).await,
        Err(WorkerError::UnknownChild)
    );
    assert!(scope.list().await.iter().all(|(listed, _)| listed != id));
}

// ------------------------------------------------------------------------ scope

/// An id started in one scope is `unknown-child` through a scope of another operation,
/// another parent or another generation — the same answer a never-allocated id gets.
#[tokio::test]
async fn another_scopes_or_another_parents_id_is_unknown_child() {
    let h = harness();
    let mine = h.scopes.scope(key(1, "delegate", "main"));
    let id = mine.start(spec("now")).await.unwrap();
    assert!(matches!(
        mine.wait(&id, CancellationToken::new()).await,
        Ok(ChildStatus::Finished(_))
    ));

    for other in [
        key(1, "workflow-run", "main"),
        key(1, "delegate", "helper"),
        key(2, "delegate", "main"),
    ] {
        unknown_everywhere(&h.scopes.scope(other), &id).await;
    }
    unknown_everywhere(&mine, &ChildId("w999".into())).await;

    // The scope that started it reads it, with the same result `status` gives.
    let status = mine.status(&id).await.unwrap();
    assert_eq!(mine.result(&id).await.unwrap(), status);
    assert_eq!(mine.describe(&id).await.unwrap(), "fake/route");
}

// ---------------------------------------------------------------- stale handles

/// A retired scope's ids are `unknown-child`, and the running child is not touched: it
/// keeps running, completes, keeps its result in the service and sends the parent its
/// one notification.
#[tokio::test]
async fn a_retired_scopes_ids_are_unknown_and_the_child_still_completes() {
    let h = harness();
    let parent = agent(Arc::new(ScriptedProvider::new(Vec::new())));
    h.service.set_parent_inbox(parent.inbox());
    let scope = h.scopes.scope(key(1, "delegate", "main"));
    let id = scope.start(spec("gated")).await.unwrap();
    assert_eq!(scope.status(&id).await, Ok(ChildStatus::Running));

    h.scopes.retire_generation(1).await;
    unknown_everywhere(&scope, &id).await;
    unknown_everywhere(&h.scopes.scope(key(1, "delegate", "main")), &id).await;
    assert_eq!(
        h.service.status(&id).await,
        Ok(ChildStatus::Running),
        "retiring cancelled nothing"
    );

    h.gate.add_permits(1);
    match h.service.wait(&id, CancellationToken::new()).await.unwrap() {
        ChildStatus::Finished(result) => assert_eq!(result.final_text, "done"),
        other => panic!("{other:?}"),
    }
    assert!(
        parent.has_pending_inbox(),
        "the parent still hears of it once"
    );
}

/// A dropped scope's ids are `unknown-child` through a new handle for the same key, and
/// the child still completes.
#[tokio::test]
async fn a_dropped_scopes_ids_are_unknown_and_the_child_still_completes() {
    let h = harness();
    let scope = h.scopes.scope(key(1, "delegate", "main"));
    let id = scope.start(spec("gated")).await.unwrap();
    drop(scope);

    let again = h.scopes.scope(key(1, "delegate", "main"));
    unknown_everywhere(&again, &id).await;

    h.gate.add_permits(1);
    assert!(matches!(
        h.service.wait(&id, CancellationToken::new()).await,
        Ok(ChildStatus::Finished(_))
    ));
}

/// A start in a retired scope is refused with `shut-down`, builds nothing and takes no
/// id: the next live start gets the id that would have been next.
#[tokio::test]
async fn a_start_in_a_retired_scope_builds_nothing() {
    let h = harness();
    let scope = h.scopes.scope(key(1, "delegate", "main"));
    scope.retire().await;

    assert_eq!(scope.start(spec("now")).await, Err(WorkerError::ShutDown));
    assert_eq!(*h.built.lock().unwrap(), 0, "nothing was built");
    let live = h.scopes.scope(key(2, "delegate", "main"));
    assert_eq!(
        live.start(spec("now")).await,
        Ok(ChildId("w1".into())),
        "no id was taken"
    );
}

// ------------------------------------------------------------------ double start

/// Two starts from one scope with the same spec are two children with distinct ids,
/// both valid in that scope, both listed in start order, neither valid elsewhere.
#[tokio::test]
async fn a_double_start_gives_two_distinct_ids_valid_only_in_scope() {
    let h = harness();
    let scope = h.scopes.scope(key(1, "delegate", "main"));
    let other = h.scopes.scope(key(1, "delegate", "helper"));

    let first = scope.start(spec("now")).await.unwrap();
    let second = scope.start(spec("now")).await.unwrap();
    assert_ne!(first, second);
    assert_eq!(*h.built.lock().unwrap(), 2);

    for id in [&first, &second] {
        assert!(matches!(
            scope.wait(id, CancellationToken::new()).await,
            Ok(ChildStatus::Finished(_))
        ));
        unknown_everywhere(&other, id).await;
    }
    let listed: Vec<ChildId> = scope.list().await.into_iter().map(|(id, _)| id).collect();
    assert_eq!(listed, [first, second]);
    assert!(other.list().await.is_empty());
}

// ------------------------------------------------------------------ cancellation

/// `wait` on a scoped id returns `Running` when the caller's cancel fires first, and the
/// child keeps running.
#[tokio::test]
async fn a_scoped_wait_returns_running_when_cancelled() {
    let h = harness();
    let scope = h.scopes.scope(key(1, "delegate", "main"));
    let id = scope.start(spec("hang")).await.unwrap();

    let cancel = CancellationToken::new();
    cancel.cancel();
    assert_eq!(scope.wait(&id, cancel).await, Ok(ChildStatus::Running));
    assert_eq!(scope.status(&id).await, Ok(ChildStatus::Running));

    scope.cancel(&id).await.unwrap();
    assert_eq!(
        scope.wait(&id, CancellationToken::new()).await,
        Ok(ChildStatus::Cancelled)
    );
}

/// A `cancel` of another scope's id is `unknown-child` and touches no child: the
/// target keeps running until its own scope cancels it.
#[tokio::test]
async fn a_foreign_cancel_is_unknown_child_and_touches_nothing() {
    let h = harness();
    let owner = h.scopes.scope(key(1, "delegate", "main"));
    let intruder = h.scopes.scope(key(1, "delegate", "helper"));
    let id = owner.start(spec("hang")).await.unwrap();

    h.probe.streaming.notified().await;
    let turn = h
        .probe
        .token
        .lock()
        .unwrap()
        .clone()
        .expect("the turn streams");

    assert_eq!(intruder.cancel(&id).await, Err(WorkerError::UnknownChild));
    assert!(!turn.is_cancelled(), "the foreign cancel reached no turn");
    assert_eq!(owner.status(&id).await, Ok(ChildStatus::Running));

    owner.cancel(&id).await.unwrap();
    assert!(turn.is_cancelled(), "the owner's cancel reaches the turn");
    assert_eq!(
        owner.wait(&id, CancellationToken::new()).await,
        Ok(ChildStatus::Cancelled)
    );
}
