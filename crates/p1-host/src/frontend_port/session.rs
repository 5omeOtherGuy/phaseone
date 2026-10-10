//! The host's [`SessionHandle`]: one agent behind an async lock, so a turn holds it
//! while the cancel hooks, which never touch the agent, run beside it.
//!
//! The cancel hooks follow `spawn_worker_stopper` and `spawn_run_canceller` in
//! `tui.rs` (frozen donor): the services' own `cancel`, here for every running id.

use std::sync::Arc;

use p1_contracts::frontend::SessionHandle;
use p1_contracts::{BoxFuture, CancellationToken, TurnEnd};
use p1_core::Agent;
use tokio::sync::Mutex;

use super::{Tracker, TurnGuard};
use crate::HostDeps;
use crate::frontend::WorkerService;

pub(super) struct HostSession<'a> {
    agent: Mutex<&'a mut Agent>,
    tracker: Arc<Tracker>,
    #[cfg_attr(
        not(feature = "delegation"),
        allow(dead_code, reason = "cancelled only through the delegation service")
    )]
    workers: Option<Arc<dyn WorkerService>>,
    #[cfg(feature = "workflows")]
    workflows: Option<Arc<dyn p1_workflow::WorkflowService>>,
}

impl<'a> HostSession<'a> {
    pub(super) fn new(
        deps: &HostDeps,
        agent: &'a mut Agent,
        workers: Option<Arc<dyn WorkerService>>,
        tracker: Arc<Tracker>,
    ) -> Self {
        #[cfg(not(feature = "workflows"))]
        let _ = deps;
        Self {
            agent: Mutex::new(agent),
            tracker,
            workers,
            #[cfg(feature = "workflows")]
            workflows: deps.workflow_service.clone(),
        }
    }
}

impl SessionHandle for HostSession<'_> {
    fn prompt<'s>(&'s self, text: String, cancel: CancellationToken) -> BoxFuture<'s, TurnEnd> {
        Box::pin(async move {
            let mut agent = self.agent.lock().await;
            let _turn = TurnGuard::begin(&self.tracker);
            agent.run_turn(text, cancel).await
        })
    }

    fn cancel_runs<'s>(&'s self) -> BoxFuture<'s, ()> {
        Box::pin(async move {
            #[cfg(feature = "workflows")]
            if let Some(service) = &self.workflows {
                for (id, status) in service.list().await {
                    if matches!(status, p1_workflow::RunStatus::Running(_)) {
                        let _ = service.cancel(&id).await;
                    }
                }
            }
        })
    }

    fn stop_workers<'s>(&'s self) -> BoxFuture<'s, ()> {
        Box::pin(async move {
            #[cfg(feature = "delegation")]
            if let Some(service) = &self.workers {
                for (id, status) in service.list().await {
                    if matches!(status, p1_workers::ChildStatus::Running) {
                        let _ = service.cancel(&id).await;
                    }
                }
            }
        })
    }

    fn drain_inbox<'s>(&'s self, cancel: CancellationToken) -> BoxFuture<'s, Option<TurnEnd>> {
        Box::pin(async move {
            let mut agent = self.agent.lock().await;
            let mut last = None;
            while agent.has_pending_inbox() && !cancel.is_cancelled() {
                let _turn = TurnGuard::begin(&self.tracker);
                let Some(end) = agent.run_inbox_turn(cancel.clone()).await else {
                    break;
                };
                let cancelled = matches!(end, TurnEnd::Cancelled);
                last = Some(end);
                if cancelled {
                    break;
                }
            }
            last
        })
    }

    fn inbox_ready<'s>(&'s self) -> BoxFuture<'s, ()> {
        Box::pin(async move { self.agent.lock().await.inbox_ready().await })
    }
}
