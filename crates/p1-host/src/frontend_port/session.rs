//! The host's [`SessionHandle`]: one agent behind an async lock, so a turn holds it
//! while the cancel hooks, which never touch the agent, run beside it.
//!
//! The cancel hooks follow `spawn_worker_stopper` and `spawn_run_canceller` in
//! `tui.rs` (frozen donor): the services' own `cancel`, here for every running id.

use std::sync::Arc;

use p1_contracts::frontend::{CommandInfo, CommandOutput, ConfigChoice, ConfigKind, SessionHandle};
use p1_contracts::{BoxFuture, CancellationToken, TurnEnd};
use p1_core::Agent;
use tokio::sync::Mutex;

use super::{Tracker, TurnGuard};
use crate::HostDeps;
use crate::frontend::WorkerService;
use crate::run::{SwitchRequest, compaction_line, reload_modules, switch_model, write_stderr};

pub(super) struct HostSession<'a> {
    deps: &'a HostDeps,
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
        deps: &'a HostDeps,
        agent: &'a mut Agent,
        workers: Option<Arc<dyn WorkerService>>,
        tracker: Arc<Tracker>,
    ) -> Self {
        Self {
            deps,
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
            self.deps.user_questions.note_user_input(&text);
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

    /// Reads the switch context only, never the agent, so it answers during a turn. A
    /// table that does not load offers nothing, and says why on stderr.
    fn config<'s>(&'s self) -> BoxFuture<'s, Vec<ConfigChoice>> {
        Box::pin(async move {
            let Some(switch) = &self.deps.model_switch else {
                return Vec::new();
            };
            super::config::choices(self.deps, switch).unwrap_or_else(|reason| {
                write_stderr(self.deps, &format!("· no model settings: {reason}\n"));
                Vec::new()
            })
        })
    }

    /// The line mode's `/model REF` and `/effort LEVEL`: the one switch path, between
    /// turns, since the agent lock waits for the running one.
    fn set_config<'s>(
        &'s self,
        kind: ConfigKind,
        value: &'s str,
    ) -> BoxFuture<'s, Result<(), String>> {
        Box::pin(async move {
            let Some(switch) = &self.deps.model_switch else {
                return Err("this session cannot switch its model".to_string());
            };
            if super::config::is_default_effort(kind, value) {
                return Ok(());
            }
            let request = match kind {
                ConfigKind::Model => SwitchRequest::Model(value),
                ConfigKind::Effort => SwitchRequest::Effort(value),
            };
            let mut agent = self.agent.lock().await;
            let model = switch_model(switch, &mut agent, request).await?;
            write_stderr(self.deps, &format!("· model: {model}\n"));
            Ok(())
        })
    }

    fn commands<'s>(&'s self) -> BoxFuture<'s, Vec<CommandInfo>> {
        Box::pin(async move { super::commands::list(self.deps) })
    }

    /// The line mode's own commands. The ones that need the agent take its lock, so
    /// they run between turns, as the line loop runs them.
    fn command<'s>(
        &'s self,
        name: &'s str,
        argument: &'s str,
        cancel: CancellationToken,
    ) -> BoxFuture<'s, Result<CommandOutput, String>> {
        Box::pin(async move {
            if name == "compact" {
                let result = self.agent.lock().await.compact_now(&cancel).await;
                return Ok(CommandOutput::Text(format!(
                    "{}\n",
                    compaction_line(&result)
                )));
            }
            let Some(switch) = &self.deps.model_switch else {
                return Err(format!("/{name} is not a command of this session"));
            };
            match name {
                "status" => Ok(CommandOutput::Text(super::commands::status(
                    self.deps, switch,
                ))),
                "access" => Ok(CommandOutput::Text(super::commands::access(switch))),
                "modules" if argument == "reload" => {
                    let mut agent = self.agent.lock().await;
                    match reload_modules(switch, &mut agent).await {
                        Ok(reloaded) => Ok(CommandOutput::Text(format!(
                            "modules reloaded: {reloaded}\n"
                        ))),
                        Err(reason) => Err(format!("modules not reloaded: {reason}")),
                    }
                }
                "modules" => Err("it takes one argument, `reload`".to_string()),
                skill => super::commands::skill(self.deps, switch, skill, argument),
            }
        })
    }
}
