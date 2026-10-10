//! The host side of the front-end port (D7, ADR-0152): [`PortFrontEnd`] plugs any
//! [`FrontEndPort`] into the host's own [`FrontEnd`] seam, and [`session`] is the
//! [`p1_contracts::frontend::SessionHandle`] the port drives the session through.
//!
//! The bridge translates the host's worker and workflow callbacks into
//! [`BackgroundSignal`]s: a worker starts at `child_started`, and again at the
//! `TurnStarted` of a turn `worker_continue` gave it after it ended; it ends at
//! `worker_ended` or, for a workflow step (whose end the host does not report as a
//! worker end), at its step's end, at a fallback that replaces it, or at its run's
//! end. Background shell jobs never reach the seam, so they are never signalled.

mod commands;
mod config;
mod mode;
mod session;
#[cfg(feature = "workflows")]
mod workflow;

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use p1_contracts::frontend::{BackgroundKind, BackgroundPhase, BackgroundSignal, FrontEndPort};
use p1_contracts::{AgentEvent, AuthorizationPolicy, BoxFuture, CancellationToken, EventSink};
use p1_core::Agent;

use crate::HostDeps;
use crate::activity::Completion;
use crate::cli::Options;
use crate::frontend::{FrontEnd, WorkerService};
use crate::run::StallGuard;

/// A [`FrontEndPort`] as the host composes it.
pub struct PortFrontEnd {
    port: Arc<dyn FrontEndPort>,
    tracker: Arc<Tracker>,
    /// The session's permission mode (#696), read by every policy this front end
    /// hands out.
    mode: Arc<mode::ModeCell>,
}

impl PortFrontEnd {
    pub fn new(port: Arc<dyn FrontEndPort>) -> Self {
        Self {
            tracker: Arc::new(Tracker::new(port.clone())),
            port,
            mode: Arc::default(),
        }
    }
}

impl FrontEnd for PortFrontEnd {
    fn event_sink(&self) -> Arc<dyn EventSink> {
        self.port.event_sink()
    }

    fn child_event_sink(&self, worker_id: &str, _route: &str, _model: &str) -> Arc<dyn EventSink> {
        Arc::new(ChildSink {
            inner: self.port.child_event_sink(worker_id),
            tracker: self.tracker.clone(),
            worker_id: worker_id.to_string(),
        })
    }

    fn child_started(&self, worker_id: &str) {
        self.tracker.start(BackgroundKind::Worker, worker_id);
    }

    #[cfg(feature = "delegation")]
    fn worker_ended(&self, worker_id: &str, description: &str, report: &p1_workers::WorkerReport) {
        self.port.worker_ended(
            worker_id,
            &crate::render::worker_end_note(worker_id, description, report),
        );
        self.tracker.end(BackgroundKind::Worker, worker_id);
    }

    #[cfg(feature = "workflows")]
    fn workflow_line(&self, line: &str) {
        workflow::line(self.port.as_ref(), line);
    }

    #[cfg(feature = "workflows")]
    fn workflow_run_started(&self, run: &crate::frontend::WorkflowRunStarted) {
        workflow::started(self.port.as_ref(), run);
        self.tracker.start(BackgroundKind::Workflow, &run.id);
    }

    #[cfg(feature = "workflows")]
    fn workflow_phase(&self, run: &str, name: &str) {
        self.port
            .workflow_event(p1_contracts::frontend::WorkflowEvent::Phase {
                run: run.to_string(),
                name: name.to_string(),
            });
    }

    #[cfg(feature = "workflows")]
    fn workflow_log(&self, run: &str, text: &str) {
        self.port
            .workflow_event(p1_contracts::frontend::WorkflowEvent::Log {
                run: run.to_string(),
                text: text.to_string(),
            });
    }

    #[cfg(feature = "workflows")]
    fn workflow_jobs_queued(&self, run: &str, count: usize) {
        workflow::jobs_queued(self.port.as_ref(), run, count);
    }

    #[cfg(feature = "workflows")]
    fn workflow_thunk_failed(&self, run: &str, error: &str) {
        workflow::thunk_failed(self.port.as_ref(), run, error);
    }

    /// A fallback starts another worker for the same step: the one it replaces ended.
    #[cfg(feature = "workflows")]
    fn workflow_step_started(&self, step: &crate::frontend::WorkflowStepStarted) {
        self.port
            .workflow_step(&p1_contracts::frontend::WorkflowStep {
                run: step.run.clone(),
                ordinal: step.ordinal,
                call: step.call.clone(),
                label: step.label.clone(),
                task: Some(step.prompt.clone()),
                status: "running".to_string(),
            });
        workflow::step_started(self.port.as_ref(), step);
        if let Some(worker) = &step.worker_id {
            let replaced = self
                .tracker
                .steps
                .lock()
                .unwrap()
                .insert((step.run.clone(), step.ordinal), worker.clone());
            if let Some(replaced) = replaced.filter(|replaced| replaced != worker) {
                self.tracker.end(BackgroundKind::Worker, &replaced);
            }
        }
    }

    #[cfg(feature = "workflows")]
    fn workflow_step_ended(&self, step: &crate::frontend::WorkflowStepEnded) {
        // Queue the step update before ending workers can release a held prompt.
        workflow::step_ended(self.port.as_ref(), step);
        self.port
            .workflow_step(&p1_contracts::frontend::WorkflowStep {
                run: step.run.clone(),
                ordinal: step.ordinal,
                call: step.call.clone(),
                label: step.label.clone(),
                task: None,
                status: step.status.clone(),
            });
        let tracked = self
            .tracker
            .steps
            .lock()
            .unwrap()
            .remove(&(step.run.clone(), step.ordinal));
        for worker in tracked.iter().chain(step.worker_id.iter()) {
            self.tracker.end(BackgroundKind::Worker, worker);
        }
    }

    #[cfg(feature = "workflows")]
    fn workflow_run_ended(&self, run: &crate::frontend::WorkflowRunEnded) {
        workflow::ended(self.port.as_ref(), run);
        let workers: Vec<String> = {
            let mut steps = self.tracker.steps.lock().unwrap();
            let keys: Vec<_> = steps
                .keys()
                .filter(|key| key.0 == run.id)
                .cloned()
                .collect();
            keys.iter().filter_map(|key| steps.remove(key)).collect()
        };
        for worker in &workers {
            self.tracker.end(BackgroundKind::Worker, worker);
        }
        self.tracker.end(BackgroundKind::Workflow, &run.id);
    }

    /// The port's policy under the session's mode: the parent and every worker.
    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        Arc::new(mode::ModePolicy {
            inner: self.port.authorization(),
            mode: self.mode.clone(),
        })
    }

    fn context_configured(&self, window_tokens: Option<u64>, summarize_at_tokens: Option<u64>) {
        self.port
            .context_configured(window_tokens, summarize_at_tokens);
    }

    fn parent_assembled(&self, _route: &str, _model: &str, _completion: Option<Completion>) {}

    /// The port drives the session itself, so the host's unattended stall guard never
    /// applies.
    fn is_headless(&self, _options: &Options) -> bool {
        false
    }

    fn run<'a>(
        &'a self,
        deps: &'a HostDeps,
        agent: &'a mut Agent,
        _cancel: &'a CancellationToken,
        workers: Option<Arc<dyn WorkerService>>,
        _stall: Option<Arc<StallGuard>>,
    ) -> BoxFuture<'a, i32> {
        Box::pin(async move {
            let session = session::HostSession::new(
                deps,
                agent,
                workers,
                self.tracker.clone(),
                self.mode.clone(),
            );
            self.port.run(&session).await
        })
    }

    fn finish(&self) {}
}

/// A worker's sink: `worker_continue` starts a new turn under the same id and calls
/// no `child_started`, so the turn's `TurnStarted` opens the worker again. A first
/// turn finds it open already and signals nothing.
struct ChildSink {
    inner: Arc<dyn EventSink>,
    tracker: Arc<Tracker>,
    worker_id: String,
}

impl EventSink for ChildSink {
    fn emit(&self, event: AgentEvent) {
        if matches!(event, AgentEvent::TurnStarted) {
            self.tracker.start(BackgroundKind::Worker, &self.worker_id);
        }
        self.inner.emit(event);
    }
}

/// The worker's agent is gone (a failed build after `child_started`, the service's
/// shutdown): a worker still open without a `TurnFinished` ends here, so a front end
/// that holds a prompt on it is never left waiting.
impl Drop for ChildSink {
    fn drop(&mut self) {
        self.tracker.end(BackgroundKind::Worker, &self.worker_id);
    }
}

/// Which turn is running and which background work is open, so every end carries
/// its start's turn and is signalled once.
struct Tracker {
    port: Arc<dyn FrontEndPort>,
    /// The running turn's ordinal and the last ordinal handed out.
    turns: Mutex<(Option<u64>, u64)>,
    /// Started and not yet ended: the turn each one started in.
    open: Mutex<HashMap<(BackgroundKind, String), Option<u64>>>,
    /// The worker currently running each workflow step, keyed by run id and ordinal.
    #[cfg_attr(
        not(feature = "workflows"),
        allow(dead_code, reason = "read only by the workflow callbacks")
    )]
    steps: Mutex<HashMap<(String, u32), String>>,
}

impl Tracker {
    fn new(port: Arc<dyn FrontEndPort>) -> Self {
        Self {
            port,
            turns: Mutex::new((None, 0)),
            open: Mutex::new(HashMap::new()),
            steps: Mutex::new(HashMap::new()),
        }
    }

    fn begin_turn(&self) {
        let mut turns = self.turns.lock().unwrap();
        turns.1 += 1;
        turns.0 = Some(turns.1);
    }

    fn end_turn(&self) {
        self.turns.lock().unwrap().0 = None;
    }

    fn start(&self, kind: BackgroundKind, id: &str) {
        let turn = self.turns.lock().unwrap().0;
        let fresh = self
            .open
            .lock()
            .unwrap()
            .insert((kind, id.to_string()), turn)
            .is_none();
        if fresh {
            self.signal(BackgroundPhase::Started, kind, id, turn);
        }
    }

    /// Ends only what is open, so an end the host reports twice is signalled once.
    fn end(&self, kind: BackgroundKind, id: &str) {
        let started = self.open.lock().unwrap().remove(&(kind, id.to_string()));
        if let Some(turn) = started {
            self.signal(BackgroundPhase::Ended, kind, id, turn);
        }
    }

    fn signal(&self, phase: BackgroundPhase, kind: BackgroundKind, id: &str, turn: Option<u64>) {
        self.port.background(BackgroundSignal {
            phase,
            kind,
            id: id.to_string(),
            turn,
        });
    }
}

/// Holds a turn open for the tracker until the turn's future ends or is dropped.
struct TurnGuard<'a>(&'a Tracker);

impl<'a> TurnGuard<'a> {
    fn begin(tracker: &'a Tracker) -> Self {
        tracker.begin_turn();
        Self(tracker)
    }
}

impl Drop for TurnGuard<'_> {
    fn drop(&mut self) {
        self.0.end_turn();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::frontend::SessionHandle;

    #[derive(Default)]
    struct Recorder(Mutex<Vec<BackgroundSignal>>);

    impl EventSink for Recorder {
        fn emit(&self, _event: AgentEvent) {}
    }

    impl FrontEndPort for Recorder {
        fn event_sink(&self) -> Arc<dyn EventSink> {
            unreachable!("the tracker never asks for the parent sink")
        }
        fn child_event_sink(&self, _worker_id: &str) -> Arc<dyn EventSink> {
            Arc::new(Recorder::default())
        }
        fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
            unreachable!("the tracker never asks for authorization")
        }
        fn background(&self, signal: BackgroundSignal) {
            self.0.lock().unwrap().push(signal);
        }
        fn run<'a>(&'a self, _session: &'a dyn SessionHandle) -> BoxFuture<'a, i32> {
            unreachable!("the tracker never runs the port")
        }
    }

    /// A worker whose agent goes away without a `TurnFinished` still ends, once.
    #[test]
    fn a_dropped_worker_sink_ends_an_open_worker_once() {
        let port = Arc::new(Recorder::default());
        let front_end = PortFrontEnd::new(port.clone());
        let sink = front_end.child_event_sink("w1", "route", "model");
        front_end.child_started("w1");
        drop(sink);
        let phases: Vec<_> = port
            .0
            .lock()
            .unwrap()
            .iter()
            .map(|s| (s.phase, s.id.clone()))
            .collect();
        assert_eq!(
            phases,
            [
                (BackgroundPhase::Started, "w1".to_string()),
                (BackgroundPhase::Ended, "w1".to_string())
            ]
        );
        // A worker that ended normally is not ended again by its sink's drop.
        let sink = front_end.child_event_sink("w2", "route", "model");
        front_end.child_started("w2");
        front_end.tracker.end(BackgroundKind::Worker, "w2");
        drop(sink);
        assert_eq!(port.0.lock().unwrap().len(), 4);
    }
}
