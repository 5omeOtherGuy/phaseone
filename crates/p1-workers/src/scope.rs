//! The scoped worker surface: the native side of the WIT interfaces `workers-start`,
//! `workers-observe` and `workers-control` (decisions S0-R1.2 and S0-R1.3).
//!
//! A [`WorkerService`] names children by ids that are valid across the whole service,
//! which is right for the native delegate tools of ONE parent but wrong for a component:
//! a component that could supply any id could select another agent's child. So every
//! export here goes through a [`WorkerScope`], the (generation, operation, parent)
//! triple the host creates, and the scope remembers the ids it started itself.
//!
//! The rules, in one place:
//!
//! - **Valid ids.** An id is valid only through a live scope that started it. Every other
//!   id — started by another scope, another parent, another operation, a retired
//!   generation, or never allocated — is [`WorkerError::UnknownChild`], checked BEFORE the
//!   service is asked, so the answer never reveals whether the id exists elsewhere.
//! - **Sharing.** [`WorkerScopes::scope`] hands out handles; two handles for one key share
//!   one scope while either lives, which is how `worker_start` and `worker_result` of one
//!   parent see the same children.
//! - **Retirement.** A scope ends when it is retired ([`WorkerScope::retire`],
//!   [`WorkerScopes::retire_generation`]) or when its last handle is dropped. A handle
//!   asked for a retired scope's key is that retired scope while any handle to it lives;
//!   after the last one is dropped a new handle starts empty (or retired, when its
//!   generation is), so every old id is `unknown-child` through it. Retiring only forgets ids: it never
//!   cancels a child, which keeps running, keeps its retained result in the service and
//!   still sends the parent its one completion notification.
//! - **Which reconfiguration retires.** The host retires a generation when a capability
//!   is removed from the module or the assembly is torn down. A model switch or a re-grant
//!   between turns (ADR-0049, ADR-0050 item 6) is NOT a new generation: the host keeps the
//!   generation across it, so the results of running children are never stranded.
//! - **`result` and `list`.** Native-only exports (the WIT has neither). `result` is the
//!   retained status of one child, the read `worker_result` performs: `Finished` carries
//!   the same [`crate::ChildResult`] `status` gives, and a running child is `Ok(Running)`
//!   at once — `result` never waits, `wait` is the blocking read. `list` is the scope's
//!   own children in start order; a retired scope lists nothing.
//! - **Double start.** Each start is its own child with a fresh id from the service, which
//!   never reuses one. A start in a retired scope is [`WorkerError::ShutDown`] and reaches
//!   no service call, so it allocates no id and builds nothing (ADR-0053 item 2).
//! - **Cancellation.** `wait` keeps the service's contract: `Ok(Running)` when the
//!   caller's cancel fires first. `cancel` of an out-of-scope id is `unknown-child` and
//!   touches no child.
//!
//! A call that already passed the scope check when the scope is retired finishes as the
//! service answers it; retirement governs the calls that begin after it. `start` is the
//! one exception that matters: a retire waits for the starts in flight, so no start can
//! add an id to a scope that is already retired.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::sync::{Arc, Mutex, Weak};

use p1_contracts::{BoxFuture, CancellationToken};
use tokio::sync::RwLock;

use crate::{ChildId, ChildSpec, ChildStatus, WorkerError, WorkerService};

/// The WIT `workers-start` interface: starting children in the caller's scope.
///
/// `Send + Sync` with boxed futures (ADR-0015), so a module runtime can link it the way
/// it links any other native capability.
pub trait WorkersStart: Send + Sync {
    /// Start a child now; the id on success, valid only in this scope.
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>>;
}

/// The WIT `workers-observe` interface, plus the native-only `result` and `list`.
pub trait WorkersObserve: Send + Sync {
    /// The route and model a child runs on, as the host shows it.
    fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>>;

    /// The current status; never waits.
    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;

    /// Resolves once the child is no longer running; `Ok(Running)` when `cancel` fires
    /// first.
    fn wait<'a>(
        &'a self,
        id: &'a ChildId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;

    /// The child's retained result: `Finished` with the same result `status` carries,
    /// the terminal `Cancelled` or `Failed`, or `Running` at once while a turn runs.
    fn result<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>>;

    /// This scope's own children, in start order, with their retained statuses.
    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>>;
}

/// The WIT `workers-control` interface.
pub trait WorkersControl: Send + Sync {
    /// Cancel the child's running turn; a finished child is a no-op `Ok`.
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>>;

    /// Another turn in the same child session, granting `add_tools` for it and every
    /// later turn (ADR-0050 item 6); `Busy` while a turn runs.
    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>>;
}

/// The host-chosen triple a scope is keyed by. All three parts are opaque here: the host
/// (S6.7) decides what an operation is and when a generation changes.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ScopeKey {
    /// The assembly generation; retired as a whole by [`WorkerScopes::retire_generation`].
    pub generation: u64,
    /// The key the members of one tool family share for one parent.
    pub operation: String,
    /// The parent agent the children belong to.
    pub parent: String,
}

/// The host's registry of scopes over one [`WorkerService`]. It adds scoping on top of
/// the service and changes nothing in it: the native delegate tools keep using the
/// service directly.
pub struct WorkerScopes {
    service: Arc<dyn WorkerService>,
    registry: Mutex<Registry>,
}

struct Registry {
    /// Weak, so a scope whose last handle is dropped ends by itself: nothing here keeps
    /// a stale scope's ids alive for a later handle to inherit.
    live: BTreeMap<ScopeKey, Weak<ScopeState>>,
    /// Generations retired so far. A handle asked for one of them is born retired, so a
    /// torn-down assembly cannot come back through a fresh handle.
    retired_generations: BTreeSet<u64>,
}

impl WorkerScopes {
    pub fn new(service: Arc<dyn WorkerService>) -> Self {
        Self {
            service,
            registry: Mutex::new(Registry {
                live: BTreeMap::new(),
                retired_generations: BTreeSet::new(),
            }),
        }
    }

    /// A handle to the scope `key`: the live one if any handle to it still exists,
    /// otherwise a new, empty one (retired at once when its generation is).
    pub fn scope(&self, key: ScopeKey) -> WorkerScope {
        let mut registry = self.registry.lock().unwrap();
        // Dropped scopes leave dead weak entries behind; clearing them here keeps the
        // map as small as the set of live scopes without a background task.
        registry.live.retain(|_, state| state.strong_count() > 0);
        if let Some(state) = registry.live.get(&key).and_then(Weak::upgrade) {
            return WorkerScope { state };
        }
        let retired = registry.retired_generations.contains(&key.generation);
        let state = Arc::new(ScopeState {
            key: key.clone(),
            service: Arc::clone(&self.service),
            gate: RwLock::new(()),
            members: Mutex::new(Members {
                retired,
                ids: Vec::new(),
            }),
        });
        registry.live.insert(key, Arc::downgrade(&state));
        WorkerScope { state }
    }

    /// Retire every scope of `generation`, now and for every later handle. Waits for the
    /// starts in flight in those scopes; never cancels a child.
    pub async fn retire_generation(&self, generation: u64) {
        // The generation is marked under the same lock `scope` takes, so a handle created
        // after this point is born retired and the collection below misses none.
        let states: Vec<Arc<ScopeState>> = {
            let mut registry = self.registry.lock().unwrap();
            registry.retired_generations.insert(generation);
            registry
                .live
                .iter()
                .filter(|(key, _)| key.generation == generation)
                .filter_map(|(_, state)| state.upgrade())
                .collect()
        };
        for state in states {
            state.retire().await;
        }
    }
}

/// One scope, as a component's capabilities see it. Clones share the scope.
#[derive(Clone)]
pub struct WorkerScope {
    state: Arc<ScopeState>,
}

struct ScopeState {
    key: ScopeKey,
    service: Arc<dyn WorkerService>,
    /// Held shared by every `start` across its service call and exclusively by `retire`,
    /// so an id is either recorded before the retirement or never allocated. An async
    /// lock because the service call awaits; the membership below never does.
    gate: RwLock<()>,
    members: Mutex<Members>,
}

struct Members {
    retired: bool,
    /// The ids this scope started, in start order.
    ids: Vec<ChildId>,
}

impl ScopeState {
    async fn retire(&self) {
        let _exclusive = self.gate.write().await;
        let mut members = self.members.lock().unwrap();
        members.retired = true;
        // The flag alone makes every id unknown; dropping them frees what nobody can
        // name through this scope any more.
        members.ids.clear();
    }

    /// `Ok` only for an id this live scope started; `UnknownChild` for everything else,
    /// whatever the service knows about it.
    fn check(&self, id: &ChildId) -> Result<(), WorkerError> {
        let members = self.members.lock().unwrap();
        if !members.retired && members.ids.contains(id) {
            Ok(())
        } else {
            Err(WorkerError::UnknownChild)
        }
    }
}

impl WorkerScope {
    pub fn key(&self) -> &ScopeKey {
        &self.state.key
    }

    /// Retire this scope: every id it holds becomes `unknown-child` through it and every
    /// later start is refused. The children keep running and still complete.
    pub async fn retire(&self) {
        self.state.retire().await;
    }

    pub fn is_retired(&self) -> bool {
        self.state.members.lock().unwrap().retired
    }
}

impl WorkersStart for WorkerScope {
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        Box::pin(async move {
            let _shared = self.state.gate.read().await;
            if self.state.members.lock().unwrap().retired {
                return Err(WorkerError::ShutDown);
            }
            let id = self.state.service.start(spec).await?;
            // Still under the shared gate: no retire has run since the check above.
            self.state.members.lock().unwrap().ids.push(id.clone());
            Ok(id)
        })
    }
}

impl WorkersObserve for WorkerScope {
    fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        Box::pin(async move {
            self.state.check(id)?;
            self.state.service.describe(id).await
        })
    }

    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move {
            self.state.check(id)?;
            self.state.service.status(id).await
        })
    }

    fn wait<'a>(
        &'a self,
        id: &'a ChildId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        Box::pin(async move {
            self.state.check(id)?;
            self.state.service.wait(id, cancel).await
        })
    }

    fn result<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        // The retained status is the result: the service keeps it for its lifetime, and
        // a running child answers at once so a reader never blocks by accident.
        Box::pin(async move {
            self.state.check(id)?;
            self.state.service.status(id).await
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        Box::pin(async move {
            let ids = {
                let members = self.state.members.lock().unwrap();
                if members.retired {
                    return Vec::new();
                }
                members.ids.clone()
            };
            // The service lists every child it has; this scope shows only its own, in
            // the order it started them.
            let mut all: HashMap<ChildId, ChildStatus> =
                self.state.service.list().await.into_iter().collect();
            ids.into_iter()
                .filter_map(|id| all.remove(&id).map(|status| (id, status)))
                .collect()
        })
    }
}

impl WorkersControl for WorkerScope {
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            self.state.check(id)?;
            self.state.service.cancel(id).await
        })
    }

    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        Box::pin(async move {
            self.state.check(id)?;
            self.state
                .service
                .continue_child(id, message, add_tools)
                .await
        })
    }
}

/// The three worker traits over a whole [`WorkerService`], with NO scope: every call is
/// the service's own, so any id the service knows is reachable.
///
/// This is today's native behaviour kept as it is: a parent's delegate tools reach every
/// child of the service they were built over. The delegate tool members take the narrow
/// traits, and this adapter lets the host keep building them from its one service until
/// it switches to [`WorkerScope`]s (S6.7). Nothing is checked or recorded here.
#[derive(Clone)]
pub struct UnscopedWorkers {
    service: Arc<dyn WorkerService>,
}

impl UnscopedWorkers {
    pub fn new(service: Arc<dyn WorkerService>) -> Self {
        Self { service }
    }
}

impl WorkersStart for UnscopedWorkers {
    fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        self.service.start(spec)
    }
}

impl WorkersObserve for UnscopedWorkers {
    fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
        self.service.describe(id)
    }

    fn status<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        self.service.status(id)
    }

    fn wait<'a>(
        &'a self,
        id: &'a ChildId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        self.service.wait(id, cancel)
    }

    fn result<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
        // The same reading a scope gives: the retained status, never a wait.
        self.service.status(id)
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
        self.service.list()
    }
}

impl WorkersControl for UnscopedWorkers {
    fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
        self.service.cancel(id)
    }

    fn continue_child<'a>(
        &'a self,
        id: &'a ChildId,
        message: String,
        add_tools: Vec<String>,
    ) -> BoxFuture<'a, Result<(), WorkerError>> {
        self.service.continue_child(id, message, add_tools)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::Notify;

    /// A service with no agents behind it: every start is recorded, so a test sees
    /// exactly which calls reached the service. `hold_starts` parks each start until
    /// the test releases it, to put a retire in the middle of one.
    #[derive(Default)]
    struct FakeService {
        children: Mutex<Vec<(ChildId, ChildStatus)>>,
        starts: Mutex<usize>,
        cancels: Mutex<Vec<ChildId>>,
        hold_starts: bool,
        entered: Notify,
        release: Notify,
    }

    impl FakeService {
        fn known(&self, id: &ChildId) -> Result<ChildStatus, WorkerError> {
            let children = self.children.lock().unwrap();
            children
                .iter()
                .find(|(known, _)| known == id)
                .map(|(_, status)| status.clone())
                .ok_or(WorkerError::UnknownChild)
        }
    }

    impl WorkerService for FakeService {
        fn start<'a>(&'a self, _spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
            Box::pin(async move {
                if self.hold_starts {
                    self.entered.notify_one();
                    self.release.notified().await;
                }
                let mut starts = self.starts.lock().unwrap();
                *starts += 1;
                let id = ChildId(format!("w{starts}"));
                self.children
                    .lock()
                    .unwrap()
                    .push((id.clone(), ChildStatus::Running));
                Ok(id)
            })
        }

        fn status<'a>(
            &'a self,
            id: &'a ChildId,
        ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
            Box::pin(async move { self.known(id) })
        }

        fn wait<'a>(
            &'a self,
            id: &'a ChildId,
            _cancel: CancellationToken,
        ) -> BoxFuture<'a, Result<ChildStatus, WorkerError>> {
            Box::pin(async move { self.known(id) })
        }

        fn cancel<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<(), WorkerError>> {
            Box::pin(async move {
                self.known(id)?;
                self.cancels.lock().unwrap().push(id.clone());
                Ok(())
            })
        }

        fn continue_child<'a>(
            &'a self,
            id: &'a ChildId,
            _message: String,
            _add_tools: Vec<String>,
        ) -> BoxFuture<'a, Result<(), WorkerError>> {
            Box::pin(async move { self.known(id).map(|_| ()) })
        }

        fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(ChildId, ChildStatus)>> {
            Box::pin(async move { self.children.lock().unwrap().clone() })
        }

        fn describe<'a>(&'a self, id: &'a ChildId) -> BoxFuture<'a, Result<String, WorkerError>> {
            Box::pin(async move { self.known(id).map(|_| "route/model".to_string()) })
        }
    }

    fn key(generation: u64, operation: &str, parent: &str) -> ScopeKey {
        ScopeKey {
            generation,
            operation: operation.into(),
            parent: parent.into(),
        }
    }

    fn spec() -> ChildSpec {
        ChildSpec {
            environment: "child".into(),
            task: "do it".into(),
            tools: vec!["read".into()],
            workspace: None,
        }
    }

    fn setup() -> (Arc<FakeService>, WorkerScopes) {
        let service = Arc::new(FakeService::default());
        let scopes = WorkerScopes::new(Arc::clone(&service) as Arc<dyn WorkerService>);
        (service, scopes)
    }

    /// Two handles for one key are one scope — `worker_start` and `worker_result` of a
    /// parent share it — while a handle for another key sees none of its children.
    #[tokio::test]
    async fn handles_for_one_key_share_the_scope() {
        let (_service, scopes) = setup();
        let starter = scopes.scope(key(1, "delegate", "main"));
        let reader = scopes.scope(key(1, "delegate", "main"));
        let other = scopes.scope(key(1, "workflow", "main"));

        let id = starter.start(spec()).await.unwrap();
        assert_eq!(reader.status(&id).await, Ok(ChildStatus::Running));
        assert_eq!(other.status(&id).await, Err(WorkerError::UnknownChild));
    }

    /// Once the last handle is dropped the scope is gone: a new handle for the same key
    /// starts empty, so the old ids are unknown through it.
    #[tokio::test]
    async fn a_dropped_scope_leaves_nothing_for_a_new_handle() {
        let (service, scopes) = setup();
        let first = scopes.scope(key(1, "delegate", "main"));
        let id = first.start(spec()).await.unwrap();
        drop(first);

        let again = scopes.scope(key(1, "delegate", "main"));
        assert!(!again.is_retired(), "a dropped scope is not a retired one");
        assert_eq!(again.status(&id).await, Err(WorkerError::UnknownChild));
        assert!(again.list().await.is_empty());
        // The child itself is untouched in the service.
        assert_eq!(service.status(&id).await, Ok(ChildStatus::Running));
    }

    /// Retiring a generation retires its live scopes and every later handle for it;
    /// another generation is untouched, and a refused start never reaches the service.
    #[tokio::test]
    async fn a_retired_generation_stays_retired_for_new_handles() {
        let (service, scopes) = setup();
        let old = scopes.scope(key(1, "delegate", "main"));
        let next = scopes.scope(key(2, "delegate", "main"));
        let id = old.start(spec()).await.unwrap();
        let kept = next.start(spec()).await.unwrap();

        scopes.retire_generation(1).await;
        assert!(old.is_retired());
        assert_eq!(old.status(&id).await, Err(WorkerError::UnknownChild));
        assert_eq!(next.status(&kept).await, Ok(ChildStatus::Running));

        let born_retired = scopes.scope(key(1, "other", "main"));
        assert!(born_retired.is_retired());
        let starts_before = *service.starts.lock().unwrap();
        assert_eq!(old.start(spec()).await, Err(WorkerError::ShutDown));
        assert_eq!(born_retired.start(spec()).await, Err(WorkerError::ShutDown));
        assert_eq!(
            *service.starts.lock().unwrap(),
            starts_before,
            "a start in a retired scope reaches no service call"
        );
    }

    /// `list` shows the scope's own children in start order and nothing once retired;
    /// `result` is the retained status, `Running` at once while a turn runs.
    #[tokio::test]
    async fn list_and_result_see_only_the_scope() {
        let (_service, scopes) = setup();
        let mine = scopes.scope(key(1, "delegate", "main"));
        let theirs = scopes.scope(key(1, "delegate", "helper"));
        let a = mine.start(spec()).await.unwrap();
        let foreign = theirs.start(spec()).await.unwrap();
        let b = mine.start(spec()).await.unwrap();

        let listed: Vec<ChildId> = mine.list().await.into_iter().map(|(id, _)| id).collect();
        assert_eq!(listed, [a.clone(), b]);
        assert_eq!(mine.result(&a).await, Ok(ChildStatus::Running));
        assert_eq!(mine.result(&foreign).await, Err(WorkerError::UnknownChild));

        mine.retire().await;
        assert!(mine.list().await.is_empty());
        assert_eq!(mine.result(&a).await, Err(WorkerError::UnknownChild));
    }

    /// A retire that meets a start in flight waits for it, so the id that start gets is
    /// recorded before the retirement and forgotten with it — never added afterwards.
    #[tokio::test]
    async fn retire_waits_for_a_start_in_flight() {
        let service = Arc::new(FakeService {
            hold_starts: true,
            ..FakeService::default()
        });
        let scopes = WorkerScopes::new(Arc::clone(&service) as Arc<dyn WorkerService>);
        let scope = scopes.scope(key(1, "delegate", "main"));

        let starting = tokio::spawn({
            let scope = scope.clone();
            async move { scope.start(spec()).await }
        });
        service.entered.notified().await;
        let retiring = tokio::spawn({
            let scope = scope.clone();
            async move { scope.retire().await }
        });
        // One scheduler pass: the retire task has run and is parked on the gate.
        tokio::task::yield_now().await;
        assert!(!retiring.is_finished(), "the retire waits for the start");
        assert!(!scope.is_retired());

        service.release.notify_one();
        let id = starting.await.unwrap().unwrap();
        retiring.await.unwrap();
        assert!(scope.is_retired());
        assert_eq!(scope.status(&id).await, Err(WorkerError::UnknownChild));
    }

    /// An out-of-scope cancel is `unknown-child` and never reaches the service.
    #[tokio::test]
    async fn a_foreign_cancel_reaches_no_child() {
        let (service, scopes) = setup();
        let mine = scopes.scope(key(1, "delegate", "main"));
        let theirs = scopes.scope(key(1, "delegate", "helper"));
        let id = theirs.start(spec()).await.unwrap();

        assert_eq!(mine.cancel(&id).await, Err(WorkerError::UnknownChild));
        assert_eq!(
            mine.cancel(&ChildId("w99".into())).await,
            Err(WorkerError::UnknownChild),
            "a never-allocated id reads the same"
        );
        assert!(service.cancels.lock().unwrap().is_empty());
        theirs.cancel(&id).await.unwrap();
        assert_eq!(*service.cancels.lock().unwrap(), [id]);
    }
}
