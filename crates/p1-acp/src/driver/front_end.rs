//! [`AcpFrontEnd`]: the ACP adapter behind the front-end port (ADR-0152, ADR-0154).
//! The host plugs it in; the session loop in `session.rs` speaks the wire.

use crate::{
    policy::{AcpPolicy, PermissionRequest},
    sink::{AcpSink, Stamped},
};
use p1_contracts::frontend::{
    BackgroundKind, BackgroundPhase, BackgroundSignal, FrontEndPort, SessionHandle,
    WorkflowProgress, WorkflowStep,
};
use p1_contracts::{AgentEvent, AuthorizationPolicy, BoxFuture, CancellationToken, EventSink};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, Weak};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::mpsc;

use super::hold::Hold;

pub type Reader = Box<dyn AsyncRead + Unpin + Send>;
pub type Writer = Box<dyn AsyncWrite + Unpin + Send>;

/// The receivers the session loop takes once, when the host hands the session over.
pub(super) struct Channels {
    pub(super) reader: Reader,
    pub(super) writer: Writer,
    pub(super) updates: mpsc::UnboundedReceiver<Stamped>,
    pub(super) permissions: mpsc::UnboundedReceiver<PermissionRequest>,
}

/// One ACP session over one byte stream pair: the process's stdin and stdout for
/// `p1 acp`, an in-memory pipe in tests.
///
/// The wire keeps the order the session loop writes in, on any runtime (#733): an
/// answer goes onto the transport's one writer queue before anything after it.
pub struct AcpFrontEnd {
    pub(super) workspace: PathBuf,
    pub(super) sink: Arc<AcpSink>,
    pub(super) policy: Arc<AcpPolicy>,
    /// Cancelled when the connection ends: a parked permission request then denies.
    pub(super) connection: CancellationToken,
    pub(super) hold: Hold,
    /// `_p1/agent_state` (#683): also the client's opt-out of the hold.
    pub(super) agent_state: Arc<crate::extensions::agent_state::AgentState>,
    pub(super) history: Mutex<Vec<p1_contracts::JournalRecord>>,
    pub(super) worker_transcripts: Arc<AtomicBool>,
    workers: Mutex<HashMap<String, WorkerState>>,
    pub(super) questions: Arc<crate::questions::Questions>,
    link: Mutex<Link>,
    /// The host took the session request's servers (#695).
    requested: AtomicBool,
}

/// The session's streams: not read yet, read by the running transport, or served.
enum Link {
    Waiting(Channels),
    Connected(super::session::Connected),
    Served,
}

impl Link {
    /// The running transport, started on first use.
    fn connected(&mut self, front: &AcpFrontEnd) -> Option<&mut super::session::Connected> {
        if matches!(self, Link::Waiting(_))
            && let Link::Waiting(channels) = std::mem::replace(self, Link::Served)
        {
            *self = Link::Connected(super::session::connect(front, channels));
        }
        match self {
            Link::Connected(connected) => Some(connected),
            _ => None,
        }
    }

    fn take(&mut self, front: &AcpFrontEnd) -> Option<super::session::Connected> {
        self.connected(front)?;
        match std::mem::replace(self, Link::Served) {
            Link::Connected(connected) => Some(connected),
            _ => None,
        }
    }
}

struct WorkerState {
    // The child owns its sink; observation must not keep dropped agents' state alive.
    sink: Weak<AcpSink>,
    description: Option<String>,
    note: Option<String>,
}

impl AcpFrontEnd {
    /// `workspace` is the host's resolved workspace; `session/new` must name it.
    pub fn new(reader: Reader, writer: Writer, workspace: PathBuf) -> Self {
        let connection = CancellationToken::new();
        let (sink, updates) = AcpSink::new();
        let (policy, permissions) = AcpPolicy::new(connection.clone());
        Self {
            workspace,
            sink: Arc::new(sink),
            policy: Arc::new(policy),
            connection,
            hold: Hold::default(),
            agent_state: Arc::default(),
            history: Mutex::new(Vec::new()),
            worker_transcripts: Arc::new(AtomicBool::new(false)),
            workers: Mutex::new(HashMap::new()),
            questions: Arc::new(crate::questions::Questions::default()),
            requested: AtomicBool::new(false),
            link: Mutex::new(Link::Waiting(Channels {
                reader,
                writer,
                updates,
                permissions,
            })),
        }
    }
}

impl FrontEndPort for AcpFrontEnd {
    fn history_loaded(&self, records: &[p1_contracts::JournalRecord]) {
        *self.history.lock().unwrap() = records.to_vec();
    }

    /// A `session/load` of the open session replays these too.
    fn record_committed(&self, record: &p1_contracts::JournalRecord) {
        self.history.lock().unwrap().push(record.clone());
    }

    fn event_sink(&self) -> Arc<dyn EventSink> {
        self.sink.clone()
    }

    fn child_event_sink(&self, worker_id: &str) -> Arc<dyn EventSink> {
        self.child_event_sink_with_model(worker_id, "", "")
    }

    fn child_event_sink_with_model(
        &self,
        worker_id: &str,
        route: &str,
        model: &str,
    ) -> Arc<dyn EventSink> {
        if self.worker_transcripts.load(Ordering::Relaxed) {
            let sink = Arc::new(self.sink.child(crate::extensions::workers::Worker {
                id: worker_id.to_string(),
                route: route.to_string(),
                model: model.to_string(),
            }));
            let mut workers = self.workers.lock().unwrap();
            workers.retain(|_, worker| worker.sink.strong_count() != 0);
            workers.insert(
                worker_id.to_string(),
                WorkerState {
                    sink: Arc::downgrade(&sink),
                    description: None,
                    note: None,
                },
            );
            return sink;
        }
        Arc::new(OperatorLog {
            prefix: format!("[{worker_id}]"),
        })
    }

    fn worker_context_configured(&self, worker_id: &str, window_tokens: Option<u64>) {
        if let Some(sink) = self
            .workers
            .lock()
            .unwrap()
            .get(worker_id)
            .and_then(|worker| worker.sink.upgrade())
        {
            sink.context_configured(window_tokens);
        }
    }

    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        self.policy.clone()
    }

    fn background(&self, signal: BackgroundSignal) {
        if signal.kind == BackgroundKind::Worker {
            let mut workers = self.workers.lock().unwrap();
            let worker = workers.get_mut(&signal.id);
            let (description, note) = if signal.phase == BackgroundPhase::Ended {
                worker
                    .map(|worker| (worker.description.take(), worker.note.take()))
                    .unwrap_or_default()
            } else {
                (None, None)
            };
            self.sink
                .worker_update(crate::extensions::workers::WorkerUpdate {
                    worker: signal.id.clone(),
                    state: if signal.phase == BackgroundPhase::Started {
                        "started"
                    } else {
                        "ended"
                    },
                    description,
                    note,
                });
        }
        if signal.kind == BackgroundKind::Workflow && signal.phase == BackgroundPhase::Started {
            self.sink.workflow_started(&signal.id);
        }
        self.hold.signal(&signal);
    }

    fn context_configured(&self, window_tokens: Option<u64>, _summarize_at_tokens: Option<u64>) {
        // ACP's size is capacity, not p1's earlier summarization threshold.
        self.sink.context_configured(window_tokens);
    }

    fn workflow_step(&self, step: &WorkflowStep) {
        self.sink.workflow_step(step);
    }

    fn plan_updated(&self, entries: &[p1_contracts::plan::PlanEntry]) {
        self.sink.plan_updated(entries);
    }

    fn workflow_progress(&self, progress: &WorkflowProgress) {
        self.sink.workflow_progress(progress);
    }

    fn workflow_event(&self, event: p1_contracts::frontend::WorkflowEvent) {
        self.sink.workflow_event(event);
    }

    fn worker_ended(&self, worker: &str, note: &str) {
        self.sink.worker_ended(worker, note);
    }

    fn worker_finished(&self, worker: &str, description: &str, note: &str) {
        if let Some(state) = self.workers.lock().unwrap().get_mut(worker) {
            state.description = Some(description.to_string());
            state.note = Some(note.to_string());
        }
        self.worker_ended(worker, note);
    }

    fn questions_supported(&self) -> bool {
        self.questions.supported()
    }

    fn ask_questions<'a>(
        &'a self,
        worker: Option<&'a str>,
        questions: Vec<p1_contracts::frontend::Question>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, p1_contracts::frontend::QuestionOutcome> {
        Box::pin(async move {
            // A question the client can answer waits for it.
            let _waiting = self.questions.ready().then(|| self.agent_state.waiting());
            self.questions
                .ask(worker, questions, cancel, &self.connection)
                .await
        })
    }

    /// The transport starts here, before the agent exists: `initialize` is answered at
    /// once, and the session request's servers reach the host while its answer waits for
    /// the agent.
    fn session_servers<'a>(&'a self) -> BoxFuture<'a, Vec<p1_contracts::mcp::ClientServer>> {
        Box::pin(async move {
            let opening = self
                .link
                .lock()
                .unwrap()
                .connected(self)
                .and_then(|connected| connected.opening.take());
            match opening {
                Some(opening) => match opening.await {
                    Ok(servers) => {
                        self.requested.store(true, Ordering::Relaxed);
                        servers
                    }
                    Err(_) => Vec::new(),
                },
                None => Vec::new(),
            }
        })
    }

    fn start_failed<'a>(&'a self, reason: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let connected = self.link.lock().unwrap().take(self);
            if let Some(connected) = connected {
                let requested = self.requested.load(Ordering::Relaxed);
                super::session::refuse_start(connected, reason, requested).await;
            }
        })
    }

    fn run<'a>(&'a self, session: &'a dyn SessionHandle) -> BoxFuture<'a, i32> {
        Box::pin(async move {
            let Some(connected) = self.link.lock().unwrap().take(self) else {
                eprintln!("p1 acp: the session was already served");
                return 1;
            };
            super::session::serve(self, session, connected).await
        })
    }
}

/// One stderr line per worker tool call and turn end.
struct OperatorLog {
    prefix: String,
}

impl EventSink for OperatorLog {
    fn emit(&self, event: AgentEvent) {
        match event {
            AgentEvent::ToolStarted { call } => eprintln!("{} {}", self.prefix, call.name),
            AgentEvent::TurnFinished { end } => eprintln!("{} turn ended: {end:?}", self.prefix),
            _ => {}
        }
    }
}
