//! Workflow runs as the WORKERS pane's live tree (ADR-0074): the data contract the host
//! fills — plain structs, statuses as strings, no `p1-workflow` type (§7.7, the pattern of
//! [`crate::transcript::WorkerReport`]) — and the tree model the pane keeps from it: runs in
//! start order, each with its phases in order, each phase with its steps in order.
//!
//! Every event arrives stamped on the TUI's one clock (the sink's milliseconds), so elapsed
//! times are the TUI's own and a paused test runtime drives them.

/// A run started. `resumed_from` names the run whose journal it replays.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunStarted {
    pub id: String,
    pub resumed_from: Option<String>,
}

/// A step's worker exists. Sent again (same `run` + `call`) when the engine reports the
/// start or a fallback link starts another worker: the running row is updated, not doubled.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepStarted {
    pub run: String,
    pub call: String,
    pub label: Option<String>,
    pub phase: Option<String>,
    pub role: String,
    /// `environment/profile[:effort]`.
    pub model: String,
    /// The worker's id (`w3`), when it exists.
    pub worker_id: Option<String>,
    pub attempt: u32,
    /// The step's whole task text.
    pub prompt: String,
}

/// A step ended. A replayed or refused step has no start: its row is made here, which is
/// why the end also names the label and the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepEnded {
    pub run: String,
    pub call: String,
    pub label: Option<String>,
    pub model: String,
    /// `done`, `failed`, `blocked` or `cancelled`.
    pub status: String,
    pub attempts: u32,
    pub replayed: bool,
    pub error: Option<String>,
    pub worker_id: Option<String>,
}

/// A run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunEnded {
    pub id: String,
    /// `completed`, `completed_with_issues`, `failed` or `cancelled`.
    pub outcome: String,
    pub steps_started: u32,
    pub steps_ended: u32,
    pub steps_failed: u32,
    pub error: Option<String>,
}

/// One workflow event as it crosses the host → TUI seam.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum WorkflowEvent {
    RunStarted(RunStarted),
    Phase {
        run: String,
        name: String,
    },
    Log {
        run: String,
        text: String,
    },
    /// A `parallel`/`pipeline` is starting `count` jobs.
    JobsQueued {
        run: String,
        count: usize,
    },
    StepStarted(StepStarted),
    StepEnded(StepEnded),
    /// A `parallel` thunk or `pipeline` item failed: a run-level note.
    ThunkFailed {
        run: String,
        error: String,
    },
    RunEnded(RunEnded),
}

/// Where a step stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Running,
    Done,
    Failed,
    Blocked,
    Cancelled,
}

impl StepState {
    fn from_status(status: &str) -> Self {
        match status {
            "done" => Self::Done,
            "blocked" => Self::Blocked,
            "cancelled" => Self::Cancelled,
            _ => Self::Failed,
        }
    }

    pub fn word(self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Blocked => "blocked",
            Self::Cancelled => "cancelled",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowStep {
    /// The selection key: `<run>/<n>`, `n` counting the run's steps from 1.
    pub key: String,
    pub call: String,
    pub label: Option<String>,
    pub role: String,
    pub model: String,
    pub worker_id: Option<String>,
    pub attempts: u32,
    pub state: StepState,
    pub replayed: bool,
    pub error: Option<String>,
    pub prompt: String,
    pub started_ms: Option<u64>,
    pub ended_ms: Option<u64>,
}

impl WorkflowStep {
    /// The label, else the call id.
    pub fn name(&self) -> &str {
        self.label.as_deref().unwrap_or(&self.call)
    }

    pub fn running(&self) -> bool {
        self.state == StepState::Running
    }

    /// Elapsed in milliseconds: live while running, final after; `None` when never started.
    pub fn elapsed_ms(&self, now_ms: u64) -> Option<u64> {
        let started = self.started_ms?;
        Some(self.ended_ms.unwrap_or(now_ms).saturating_sub(started))
    }

    /// The word a settled step's second line shows: `done`, `replayed · done`, or the
    /// error's first line.
    pub fn outcome(&self) -> String {
        let word = match (&self.error, self.state) {
            (Some(error), StepState::Failed | StepState::Blocked | StepState::Cancelled)
                if !error.trim().is_empty() =>
            {
                error.lines().next().unwrap_or_default().trim().to_string()
            }
            _ => self.state.word().to_string(),
        };
        if self.replayed {
            format!("replayed · {word}")
        } else {
            word
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowPhase {
    /// Empty for the steps a script ran before (or without) any `phase()`.
    pub name: String,
    pub started_ms: u64,
    /// When the next phase began or the run ended.
    pub ended_ms: Option<u64>,
    pub steps: Vec<WorkflowStep>,
}

impl WorkflowPhase {
    pub fn done(&self) -> usize {
        self.steps
            .iter()
            .filter(|step| step.state == StepState::Done)
            .count()
    }

    pub fn has_running(&self) -> bool {
        self.steps.iter().any(WorkflowStep::running)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRun {
    pub id: String,
    pub resumed_from: Option<String>,
    pub started_ms: u64,
    pub ended: Option<RunEnded>,
    pub ended_ms: Option<u64>,
    pub phases: Vec<WorkflowPhase>,
    /// Index into `phases` of the phase the script is in.
    pub current: Option<usize>,
    pub last_log: Option<String>,
    /// The last thunk failure, a run-level note.
    pub note: Option<String>,
    /// Jobs announced by fan-outs that no step has taken yet.
    pub queued: usize,
    /// Steps this run has shown, for the next selection key.
    pub step_count: usize,
}

impl WorkflowRun {
    fn new(id: String, resumed_from: Option<String>, at_ms: u64) -> Self {
        Self {
            id,
            resumed_from,
            started_ms: at_ms,
            ended: None,
            ended_ms: None,
            phases: Vec::new(),
            current: None,
            last_log: None,
            note: None,
            queued: 0,
            step_count: 0,
        }
    }

    pub fn running(&self) -> bool {
        self.ended.is_none()
    }

    pub fn steps(&self) -> impl Iterator<Item = &WorkflowStep> {
        self.phases.iter().flat_map(|phase| phase.steps.iter())
    }

    pub fn current_phase(&self) -> Option<&WorkflowPhase> {
        self.current.map(|index| &self.phases[index])
    }

    /// Whether phase `index` is over: no running step, and not the phase a running run is in.
    pub fn phase_ended(&self, index: usize) -> bool {
        !self.phases[index].has_running() && (!self.running() || self.current != Some(index))
    }

    pub fn elapsed_ms(&self, now_ms: u64) -> u64 {
        self.ended_ms
            .unwrap_or(now_ms)
            .saturating_sub(self.started_ms)
    }

    pub fn count(&self, state: StepState) -> usize {
        self.steps().filter(|step| step.state == state).count()
    }

    /// Every step known: the ones shown plus the jobs still queued.
    pub fn total(&self) -> usize {
        self.steps().count() + self.queued
    }

    /// Phase `index`'s elapsed: from its start to the next phase's or the run's end.
    pub fn phase_elapsed_ms(&self, index: usize, now_ms: u64) -> u64 {
        let phase = &self.phases[index];
        phase
            .ended_ms
            .or(self.ended_ms)
            .unwrap_or(now_ms)
            .saturating_sub(phase.started_ms)
    }

    fn step_mut(&mut self, call: &str, running_only: bool) -> Option<&mut WorkflowStep> {
        self.phases
            .iter_mut()
            .rev()
            .flat_map(|phase| phase.steps.iter_mut().rev())
            .find(|step| step.call == call && (!running_only || step.running()))
    }

    /// The phase a new step belongs to: the latest one named `phase`, else the current one.
    fn phase_for(&mut self, phase: Option<&str>, at_ms: u64) -> usize {
        let wanted = phase
            .map(str::to_string)
            .or_else(|| self.current_phase().map(|phase| phase.name.clone()));
        let wanted = wanted.unwrap_or_default();
        if let Some(index) = self.phases.iter().rposition(|phase| phase.name == wanted) {
            return index;
        }
        self.phases.push(WorkflowPhase {
            name: wanted,
            started_ms: at_ms,
            ended_ms: None,
            steps: Vec::new(),
        });
        self.phases.len() - 1
    }

    fn new_step(&mut self, phase: Option<&str>, step: WorkflowStep, at_ms: u64) {
        let index = self.phase_for(phase, at_ms);
        self.step_count += 1;
        self.queued = self.queued.saturating_sub(1);
        let key = format!("{}/{}", self.id, self.step_count);
        self.phases[index].steps.push(WorkflowStep { key, ..step });
    }
}

/// Every run the session has seen, in start order.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkflowTree {
    pub runs: Vec<WorkflowRun>,
}

impl WorkflowTree {
    pub fn is_empty(&self) -> bool {
        self.runs.is_empty()
    }

    pub fn run(&self, id: &str) -> Option<&WorkflowRun> {
        self.runs.iter().find(|run| run.id == id)
    }

    /// The step with selection key `key`, and its run.
    pub fn step(&self, key: &str) -> Option<(&WorkflowRun, &WorkflowStep)> {
        let (run_id, _) = key.rsplit_once('/')?;
        let run = self.run(run_id)?;
        run.steps()
            .find(|step| step.key == key)
            .map(|step| (run, step))
    }

    /// The phase name of the step with selection key `key`.
    pub fn phase_of(&self, key: &str) -> Option<&str> {
        let (run_id, _) = key.rsplit_once('/')?;
        self.run(run_id)?
            .phases
            .iter()
            .find(|phase| phase.steps.iter().any(|step| step.key == key))
            .map(|phase| phase.name.as_str())
    }

    /// Whether a step (running or ended) names worker `id`.
    pub fn references(&self, worker: &str) -> bool {
        self.runs
            .iter()
            .flat_map(WorkflowRun::steps)
            .any(|step| step.worker_id.as_deref() == Some(worker))
    }

    pub fn any_running(&self) -> bool {
        self.runs.iter().any(WorkflowRun::running)
    }

    fn run_mut(&mut self, id: &str, at_ms: u64) -> &mut WorkflowRun {
        // An event for a run the tree never saw start (it started before the TUI did)
        // still gets its run, so nothing is dropped.
        if let Some(index) = self.runs.iter().position(|run| run.id == id) {
            return &mut self.runs[index];
        }
        self.runs
            .push(WorkflowRun::new(id.to_string(), None, at_ms));
        self.runs.last_mut().expect("just pushed")
    }

    /// Fold one event into the tree, stamped `at_ms` on the TUI's clock.
    pub fn apply(&mut self, event: WorkflowEvent, at_ms: u64) {
        match event {
            WorkflowEvent::RunStarted(started) => {
                let run = self.run_mut(&started.id, at_ms);
                run.resumed_from = started.resumed_from;
                run.started_ms = at_ms;
            }
            WorkflowEvent::Phase { run, name } => {
                let run = self.run_mut(&run, at_ms);
                if run.current_phase().is_some_and(|phase| phase.name == name) {
                    return;
                }
                if let Some(current) = run.current {
                    run.phases[current].ended_ms.get_or_insert(at_ms);
                }
                run.phases.push(WorkflowPhase {
                    name,
                    started_ms: at_ms,
                    ended_ms: None,
                    steps: Vec::new(),
                });
                run.current = Some(run.phases.len() - 1);
            }
            WorkflowEvent::Log { run, text } => {
                self.run_mut(&run, at_ms).last_log = Some(text);
            }
            WorkflowEvent::JobsQueued { run, count } => {
                self.run_mut(&run, at_ms).queued += count;
            }
            WorkflowEvent::ThunkFailed { run, error } => {
                self.run_mut(&run, at_ms).note = Some(error);
            }
            WorkflowEvent::StepStarted(started) => {
                let run = self.run_mut(&started.run, at_ms);
                if let Some(step) = run.step_mut(&started.call, true) {
                    if started.worker_id.is_some() {
                        step.worker_id = started.worker_id;
                    }
                    step.model = started.model;
                    step.attempts = step.attempts.max(started.attempt);
                    return;
                }
                let step = WorkflowStep {
                    key: String::new(),
                    call: started.call,
                    label: started.label,
                    role: started.role,
                    model: started.model,
                    worker_id: started.worker_id,
                    attempts: started.attempt,
                    state: StepState::Running,
                    replayed: false,
                    error: None,
                    prompt: started.prompt,
                    started_ms: Some(at_ms),
                    ended_ms: None,
                };
                run.new_step(started.phase.as_deref(), step, at_ms);
            }
            WorkflowEvent::StepEnded(ended) => {
                let run = self.run_mut(&ended.run, at_ms);
                let state = StepState::from_status(&ended.status);
                if let Some(step) = run.step_mut(&ended.call, true) {
                    step.state = state;
                    step.attempts = ended.attempts;
                    step.replayed = ended.replayed;
                    step.error = ended.error;
                    if ended.worker_id.is_some() {
                        step.worker_id = ended.worker_id;
                    }
                    step.ended_ms = Some(at_ms);
                    return;
                }
                let step = WorkflowStep {
                    key: String::new(),
                    call: ended.call,
                    label: ended.label,
                    role: String::new(),
                    model: ended.model,
                    worker_id: ended.worker_id,
                    attempts: ended.attempts,
                    state,
                    replayed: ended.replayed,
                    error: ended.error,
                    prompt: String::new(),
                    started_ms: None,
                    ended_ms: Some(at_ms),
                };
                run.new_step(None, step, at_ms);
            }
            WorkflowEvent::RunEnded(ended) => {
                let run = self.run_mut(&ended.id, at_ms);
                if let Some(current) = run.current {
                    run.phases[current].ended_ms.get_or_insert(at_ms);
                }
                run.queued = 0;
                run.ended_ms = Some(at_ms);
                run.ended = Some(ended);
            }
        }
    }
}

/// What one worker is doing, from its own event stream (the TUI's own count).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct WorkerActivity {
    /// `<tool> <first argument>`, `streaming`, `thinking`; empty once its turn ended.
    pub now: String,
    /// `ToolStarted` events seen on the worker's sink.
    pub tool_calls: u64,
}

impl WorkerActivity {
    /// Observe one of the worker's events.
    pub fn observe(&mut self, event: &p1_contracts::AgentEvent) {
        use p1_contracts::AgentEvent as E;
        match event {
            E::ToolStarted { call } => {
                self.tool_calls += 1;
                let argument = crate::transcript::summarize_call(&call.name, call.input.raw());
                let argument = argument.lines().next().unwrap_or_default().trim();
                self.now = if argument.is_empty() {
                    call.name.clone()
                } else {
                    format!("{} {argument}", call.name)
                };
            }
            E::TextDelta { .. } => self.now = "streaming".into(),
            E::TurnStarted
            | E::RequestStarted { .. }
            | E::ReasoningDelta { .. }
            | E::ToolFinished { .. }
            | E::ResponseCompleted { .. } => self.now = "thinking".into(),
            E::TurnFinished { .. } => self.now.clear(),
            _ => {}
        }
    }
}

/// A clock as the worker rows show it: `4m12s`.
pub fn clock(ms: u64) -> String {
    let secs = ms / 1_000;
    format!("{}m{:02}s", secs / 60, secs % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn started(call: &str, phase: Option<&str>, worker: Option<&str>) -> WorkflowEvent {
        WorkflowEvent::StepStarted(StepStarted {
            run: "wf1".into(),
            call: call.into(),
            label: Some(format!("label-{call}")),
            phase: phase.map(str::to_string),
            role: "worker".into(),
            model: "claude/opus:high".into(),
            worker_id: worker.map(str::to_string),
            attempt: 1,
            prompt: "do it".into(),
        })
    }

    #[test]
    fn a_repeated_start_updates_the_running_row_and_a_replay_makes_its_own() {
        let mut tree = WorkflowTree::default();
        tree.apply(
            WorkflowEvent::RunStarted(RunStarted {
                id: "wf1".into(),
                resumed_from: Some("wf0".into()),
            }),
            0,
        );
        tree.apply(
            WorkflowEvent::JobsQueued {
                run: "wf1".into(),
                count: 3,
            },
            1,
        );
        tree.apply(
            WorkflowEvent::Phase {
                run: "wf1".into(),
                name: "Review".into(),
            },
            2,
        );
        tree.apply(started("c1", None, Some("w1")), 3);
        tree.apply(started("c1", Some("Review"), Some("w2")), 4);
        tree.apply(
            WorkflowEvent::StepEnded(StepEnded {
                run: "wf1".into(),
                call: "c9".into(),
                label: None,
                model: "claude/opus".into(),
                status: "done".into(),
                attempts: 1,
                replayed: true,
                error: None,
                worker_id: None,
            }),
            5,
        );
        let run = tree.run("wf1").unwrap();
        assert_eq!(run.phases.len(), 1);
        let steps: Vec<_> = run.steps().collect();
        assert_eq!(steps.len(), 2);
        assert_eq!(steps[0].worker_id.as_deref(), Some("w2"));
        assert_eq!(steps[0].key, "wf1/1");
        assert!(steps[1].replayed && steps[1].name() == "c9");
        assert_eq!(run.queued, 1);
        assert_eq!(run.total(), 3);
        assert!(tree.references("w2"));
        assert_eq!(tree.phase_of("wf1/2"), Some("Review"));
    }
}
