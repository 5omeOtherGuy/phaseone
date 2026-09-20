//! The host's front-end seam: event observation, authorization and the run loop.
//!
//! The host composes against this trait with an ordinary value passed down — no
//! registry, no plugin loading. `LineFrontEnd` is the default (today's line
//! renderer and `HostPolicy` plus the host's headless/interactive drivers); a
//! session that owns its own terminal UI (the TUI, issue #12) supplies its own
//! implementation and takes the run loop over.
//!
//! The seam exists because the host must install the sink and the policy into
//! `AgentParts` BEFORE the parent agent is built, while the resolved route/model
//! only exist AFTER `assemble`. [`FrontEnd::parent_assembled`] closes that gap:
//! the host announces the assembled parent once, before the first event.

#[cfg(feature = "delegation")]
use std::io::Write;
use std::sync::{Arc, Mutex, OnceLock};

use p1_contracts::{AuthorizationPolicy, BoxFuture, CancellationToken, EventSink};
use p1_core::Agent;

use crate::activity::Completion;
use crate::cli::Options;
use crate::policy::HostPolicy;
use crate::render::Renderer;
use crate::run::{StallGuard, run_headless, run_interactive};
use crate::{HostDeps, SharedWriter};

/// The optional in-process delegation service as the seam sees it. With the
/// `delegation` feature this is `p1_workers::WorkerService`; without it no
/// service can exist, so this empty placeholder keeps every front-end signature
/// independent of the feature.
#[cfg(feature = "delegation")]
pub use p1_workers::WorkerService;
#[cfg(not(feature = "delegation"))]
pub trait WorkerService: Send + Sync {}

/// What the host composes against. One value, passed down.
pub trait FrontEnd: Send + Sync {
    /// The sink the PARENT agent's events go to. The host still wraps it in its
    /// `ActivityTee` when the environment assembles `finish`.
    fn event_sink(&self) -> Arc<dyn EventSink>;

    /// The sink for one delegated worker, labelled with its id (`w1`…). `route`
    /// and `model` are the CHILD's resolved route/model, which its own usage
    /// lines need (the worker command in the brief omitted them; the line
    /// renderer cannot be built without them).
    fn child_event_sink(&self, worker_id: &str, route: &str, model: &str) -> Arc<dyn EventSink>;

    /// A delegated worker actually started (its agent was built). The line front
    /// end counts it for the exit aggregate; a custom front end may ignore it.
    /// Called at exactly the point the old host called
    /// `WorkerUsage::worker_started`, so a failed start never counts.
    fn child_started(&self, worker_id: &str);

    /// The authorization policy for the parent and, shared, for every worker.
    fn authorization(&self) -> Arc<dyn AuthorizationPolicy>;

    /// The host announces the assembled parent once, before the agent is built
    /// and before any event: its resolved route/model and its `finish` state
    /// (`None` when the environment does not assemble `finish`).
    fn parent_assembled(&self, route: &str, model: &str, completion: Option<Completion>);

    /// Whether this run is unattended and the host's §3c stall guard applies. The
    /// default is the CLI rule (`a prompt means headless`); a front end that owns
    /// a terminal UI overrides it to `false` — a TUI is interactive by definition,
    /// so the guard is never installed for it.
    fn is_headless(&self, options: &Options) -> bool {
        options.is_headless()
    }

    /// Drive the assembled agent to the end of the session and return the
    /// process exit code. The worker service, when present, stays live for the
    /// whole loop and is shut down by the host after this returns. `stall` is the
    /// host's headless §3c guard; a custom front end may ignore it.
    fn run<'a>(
        &'a self,
        deps: &'a HostDeps,
        agent: &'a mut Agent,
        cancel: &'a CancellationToken,
        workers: Option<Arc<dyn WorkerService>>,
        stall: Option<Arc<StallGuard>>,
    ) -> BoxFuture<'a, i32>;

    /// Called once after the run loop returns and the worker service is shut
    /// down. The line front end prints its totals line here; a custom front end
    /// may do nothing.
    fn finish(&self);
}

/// The default front end: today's line renderer and `HostPolicy` plus the host's
/// existing headless/interactive drivers.
///
/// It is constructed before the catalog (the delegation child factory captures
/// it) but the parent renderer is built in [`FrontEnd::parent_assembled`], once
/// the route/model exist.
pub struct LineFrontEnd {
    stdout: SharedWriter,
    stderr: SharedWriter,
    tty: bool,
    options: Options,
    policy: Arc<HostPolicy>,
    renderer: OnceLock<Arc<Renderer>>,
    completion: OnceLock<Option<Completion>>,
    /// One owner for the worker usage aggregate: every child renderer this front
    /// end builds feeds it, so the exit line is a plain read in `finish`.
    #[cfg(feature = "delegation")]
    worker_usage: Arc<crate::render::WorkerUsage>,
}

impl LineFrontEnd {
    pub fn new(deps: &HostDeps, options: &Options, cancel: CancellationToken) -> Self {
        let policy = Arc::new(HostPolicy::new(
            options.ask,
            options.is_headless(),
            deps.lines.clone(),
            deps.stderr.clone(),
            cancel,
        ));
        Self {
            stdout: deps.stdout.clone(),
            stderr: deps.stderr.clone(),
            tty: deps.stdout_is_tty,
            options: options.clone(),
            policy,
            renderer: OnceLock::new(),
            completion: OnceLock::new(),
            #[cfg(feature = "delegation")]
            worker_usage: Arc::new(crate::render::WorkerUsage::new()),
        }
    }

    fn renderer(&self) -> &Arc<Renderer> {
        self.renderer
            .get()
            .expect("parent_assembled must run before the first event")
    }

    /// Build the child renderer exactly as `make_child_factory` did: its own
    /// labelled prefix and, under delegation, a feed into the shared usage.
    fn child_renderer(&self, worker_id: &str, route: &str, model: &str) -> Renderer {
        let renderer = Renderer::new(
            self.stdout.clone(),
            self.stderr.clone(),
            self.tty,
            route.to_string(),
            model.to_string(),
            Arc::new(Mutex::new(format!("[{worker_id}] "))),
        );
        #[cfg(feature = "delegation")]
        {
            renderer.with_worker_usage(self.worker_usage.clone())
        }
        #[cfg(not(feature = "delegation"))]
        {
            renderer
        }
    }
}

impl FrontEnd for LineFrontEnd {
    fn event_sink(&self) -> Arc<dyn EventSink> {
        self.renderer().clone()
    }

    fn child_event_sink(&self, worker_id: &str, route: &str, model: &str) -> Arc<dyn EventSink> {
        Arc::new(self.child_renderer(worker_id, route, model))
    }

    fn child_started(&self, _worker_id: &str) {
        #[cfg(feature = "delegation")]
        self.worker_usage.worker_started();
    }

    fn authorization(&self) -> Arc<dyn AuthorizationPolicy> {
        self.policy.clone()
    }

    fn parent_assembled(&self, route: &str, model: &str, completion: Option<Completion>) {
        let renderer = Renderer::new(
            self.stdout.clone(),
            self.stderr.clone(),
            self.tty,
            route.to_string(),
            model.to_string(),
            Arc::new(Mutex::new(String::new())),
        );
        let _ = self.renderer.set(Arc::new(renderer));
        let _ = self.completion.set(completion);
    }

    fn run<'a>(
        &'a self,
        deps: &'a HostDeps,
        agent: &'a mut Agent,
        cancel: &'a CancellationToken,
        _workers: Option<Arc<dyn WorkerService>>,
        stall: Option<Arc<StallGuard>>,
    ) -> BoxFuture<'a, i32> {
        Box::pin(async move {
            let completion = self.completion.get().cloned().unwrap_or(None);
            let renderer = self.renderer().clone();
            if self.is_headless(&self.options) {
                let stall = stall.expect("the host installs the guard for a headless run");
                run_headless(
                    deps,
                    agent,
                    &self.options,
                    cancel,
                    completion,
                    &stall,
                    &renderer,
                )
                .await
            } else {
                run_interactive(deps, agent, cancel).await
            }
        })
    }

    fn finish(&self) {
        if let Some(renderer) = self.renderer.get() {
            renderer.finish();
        }
        // After the parent's own total, and only when a worker actually ran.
        #[cfg(feature = "delegation")]
        if let Some(line) = self.worker_usage.line() {
            let mut writer = self.stderr.lock().unwrap();
            let _ = writeln!(writer, "{line}");
            let _ = writer.flush();
        }
    }
}
