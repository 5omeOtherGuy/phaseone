//! The frozen public API of the workflow module (ADR-0053). Every seam another crate
//! is written against lives here: the settings the host parses, the model resolver and
//! step runner the host implements, the envelope a script sees, the journal record, and
//! the service the tools call. Additions are allowed; renames and removals are not.

use std::collections::BTreeMap;
use std::path::PathBuf;

use p1_contracts::{BoxFuture, CancellationToken};
use serde::{Deserialize, Serialize};
use serde_json::Value;

// ------------------------------------------------------------------ settings (item 4)

/// The `[workflows]` table of `settings.toml`, as the host parses it. Unknown keys are
/// errors, like every other p1 settings table. A user table is laid OVER the shipped
/// defaults with [`WorkflowSettings::overridden_by`]: a role or cap the user names
/// replaces the shipped one of that name, the others stay.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkflowSettings {
    /// `[workflows.roles.<name>]`: what a script's `role` resolves to.
    #[serde(default)]
    pub roles: BTreeMap<String, RoleSpec>,
    /// `[workflows.caps] "<wire model>" = N`: attempts (starts + repairs) one run may
    /// spend on that resolved wire model. Absent = unlimited. Keyed by the WIRE model,
    /// so two profiles of one model share the counter.
    #[serde(default)]
    pub caps: BTreeMap<String, u32>,
    /// `agent()` calls one run may make (replayed calls count too).
    #[serde(default = "default_max_steps")]
    pub max_steps: u32,
    /// OS threads one run may use for thunks and pipeline items at once, clamped to 64 (the
    /// run's memory ceiling assumes it); a thunk that finds none free runs inline on its
    /// caller's thread (never waits for a thread).
    #[serde(default = "default_max_threads")]
    pub max_threads: usize,
}

fn default_max_steps() -> u32 {
    200
}

fn default_max_threads() -> usize {
    64
}

/// One role: the model reference `environment/profile[:effort]` (ADR-0049), the other
/// models to try when that one's ROUTE fails (ADR-0054), and the tool MODULE names a
/// step in that role is granted (plus `finish`, always). A call's own `tools` replaces
/// the role's grant for that step.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RoleSpec {
    pub model: String,
    /// The fallback chain, in order (ADR-0054 item 2): `E/P[:effort]` references tried
    /// one after another when the route of the model before them fails. Empty means no
    /// fallback — a `[workflows.roles.<r>]` table that names a `model` and no `fallback`
    /// replaces the shipped role WHOLESALE, so its chain is empty too.
    #[serde(default)]
    pub fallback: Vec<String>,
    #[serde(default)]
    pub tools: Vec<String>,
}

impl WorkflowSettings {
    /// The shipped defaults (ADR-0054 items 1 and 2): the everyday worker and the
    /// verifier on the cheap DeepSeek route with a fallback chain behind it, reviewer on
    /// Claude Opus 5.5, judge on Fable — the scarce model, capped at three attempts per
    /// run on its wire model whatever role names it.
    pub fn shipped() -> Self {
        let role = |model: &str, fallback: &[&str], tools: &[&str]| RoleSpec {
            model: model.to_string(),
            fallback: fallback.iter().map(|model| model.to_string()).collect(),
            tools: tools.iter().map(|tool| tool.to_string()).collect(),
        };
        let mut roles = BTreeMap::new();
        roles.insert(
            "worker".to_string(),
            role(
                "deepseek2/deepseek-v4.1-flash",
                &["gpt/gpt-6-sol", "claude/claude-opus-5-5"],
                &["read", "grep", "edit", "shell"],
            ),
        );
        roles.insert(
            "reviewer".to_string(),
            role(
                "claude/claude-opus-5-5:high",
                &[],
                &["read", "grep", "shell"],
            ),
        );
        roles.insert(
            "verifier".to_string(),
            role(
                "deepseek2/deepseek-v4.1-flash",
                &["gpt/gpt-6-sol"],
                &["read", "grep", "shell"],
            ),
        );
        roles.insert(
            "judge".to_string(),
            role("claude/claude-fable-5", &[], &["read", "grep"]),
        );
        let mut caps = BTreeMap::new();
        // The only Fable profile p1 ships today (routes/anthropic-subscription.toml). A
        // `claude-fable-5-1` profile, if one is added, must be capped here too.
        caps.insert("claude-fable-5".to_string(), 3);
        Self {
            roles,
            caps,
            max_steps: default_max_steps(),
            max_threads: default_max_threads(),
        }
    }

    /// `user` laid over `self`: named roles and caps replace, the scalars replace.
    pub fn overridden_by(mut self, user: WorkflowSettings) -> Self {
        self.roles.extend(user.roles);
        self.caps.extend(user.caps);
        self.max_steps = user.max_steps;
        self.max_threads = user.max_threads;
        self
    }
}

// ------------------------------------------------------------- model resolution (item 4)

/// A model reference resolved by the host: what the reference names and the WIRE model
/// the route binds it to, which is what caps count.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedModel {
    /// The reference as written, `environment/profile[:effort]`.
    pub reference: String,
    pub environment: String,
    pub profile: String,
    /// The effort name, when the reference carries one.
    pub effort: Option<String>,
    pub wire_model: String,
}

/// Resolves `environment/profile[:effort]` against the host's environments and routes
/// (ADR-0049). Called at preflight for every role, before any worker starts.
pub trait ModelResolver: Send + Sync {
    fn resolve(&self, reference: &str) -> Result<ResolvedModel, String>;
}

// ------------------------------------------------------------------ steps (item 2, 5)

/// Identifier of one run within one service: `wf1`, `wf2`, … never reused.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(pub String);

/// The stable identity of one `agent()` call: a hash of (label, prompt, canonical opts),
/// never a sequence number — `parallel` reaches `agent()` in a different order each run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallId(pub String);

/// Everything the host needs to start ONE worker for a step. `attempt` is 1 for the
/// start; a repair is a second turn in the same worker and never a new request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepRequest {
    pub run: RunId,
    pub call: CallId,
    pub label: Option<String>,
    pub phase: Option<String>,
    pub role: String,
    pub model: ResolvedModel,
    /// The tool MODULE names to grant, never empty, never `finish`, never a worker or
    /// workflow tool (the host refuses those; the engine has already validated them).
    pub tools: Vec<String>,
    /// The task text: the only thing the worker receives.
    pub prompt: String,
    /// The output contract the worker's `finish` gets, when the call passed `schema`.
    pub schema: Option<Value>,
    /// The workspace the worker runs in: the run's own when `None`.
    pub workspace: Option<PathBuf>,
    pub attempt: u32,
}

/// The worker a step ran in: its id in the host's worker service and the route/model
/// description the host shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkerRef {
    pub id: String,
    pub description: String,
}

/// Whether the structured `result` of an accepted `done` met the contract. Mirrors the
/// `finish` tool's own check; carried here so the engine depends on no tool crate.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SchemaCheck {
    NotRequested,
    Passed,
    Failed(Vec<String>),
}

/// How one worker turn ended, as the host read it from the worker's own report and
/// its accepted `finish` outcome — never from its prose.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StepEnd {
    Done {
        summary: String,
        /// ADR-0051's label: `commands passed: …` or `not verified; parent verification
        /// required`.
        evidence: String,
        /// The accepted `result`, when a contract was set.
        result: Option<Value>,
        schema: SchemaCheck,
    },
    Blocked {
        summary: String,
        needs: String,
    },
    /// The turn completed without a `finish` call; `text` is the worker's last message.
    EndedWithoutFinish {
        text: String,
    },
    Failed(String),
    /// The ROUTE failed (ADR-0054 item 3): the worker could not run at all, or its turn
    /// ended on a provider failure — an exhausted account, an unreachable route, one
    /// refusing the model. The ONLY end that walks a role's fallback chain: a wrong
    /// answer (a `Failed` from a turn that ran), a `Blocked`, a cap and a cancellation
    /// never do. `model` is the reference the failed link ran on.
    RouteFailed {
        model: String,
        error: String,
    },
    Cancelled,
}

/// A started step's worker and how its turn ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepOutcome {
    pub worker: WorkerRef,
    pub end: StepEnd,
}

/// The seam between the engine and the host's worker service (ADR-0053 item 2). The host
/// implements it over `p1-workers`' prepared start: capacity is shared with direct workers,
/// a step never gets the worker or workflow tools.
pub trait StepRunner: Send + Sync {
    /// Wait for capacity, start ONE worker for `request` and run its first turn to the
    /// end. `Err` means no worker was started (unknown environment, service shut down);
    /// the step fails with that reason. `cancel` ends the wait or the turn: the outcome
    /// is then `StepEnd::Cancelled`.
    fn run<'a>(
        &'a self,
        request: &'a StepRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepOutcome, String>>;

    /// The one schema repair (item 5): another turn in the SAME worker with `message`
    /// (the validation errors and the contract), to its end. `Err` means the turn did not
    /// run.
    fn repair<'a>(
        &'a self,
        worker: &'a WorkerRef,
        message: String,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<StepEnd, String>>;
}

// ------------------------------------------------------------------ envelope (item 3)

/// A step's status as the script sees it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepStatus {
    Done,
    Blocked,
    Failed,
    Cancelled,
}

/// One model a step's chain turned to, and why the step moved on from it (ADR-0054
/// item 4). The step line and the envelope name the chain walked with these, in order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelTry {
    /// The reference as the chain named it, `environment/profile[:effort]`.
    pub model: String,
    /// Why the step moved on from this link; `None` on the link the step ended on — and
    /// on the LAST link of a chain whose failure exhausted it, where there was nowhere
    /// left to move to.
    pub moved_on: Option<MovedOn>,
}

/// Why a step left one link of its chain for the next (ADR-0054 items 3 and 4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MovedOn {
    /// The route failed: the one reason fallback exists.
    RouteFailed,
    /// The cap refused the link before anything ran.
    Capped,
}

/// What `agent()` returns to the script — never a naked value, never a null. `value` is
/// the accepted `result` when a schema was given and passed, the `finish` summary text
/// for a `done` without a schema, and `null` otherwise. `error` names a typed failure:
/// `quota_exceeded: …`, `invalid_output: …`, `max_steps: …`, `unknown_role: …`,
/// `route: …`, or the host's reason.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StepEnvelope {
    pub step: CallId,
    pub label: Option<String>,
    pub status: StepStatus,
    pub value: Value,
    pub schema: SchemaCheck,
    pub evidence: Option<String>,
    /// Attempts spent: starts plus repairs; 0 for a step refused before dispatch. Every
    /// link of a fallback chain is one more attempt (ADR-0054 item 4).
    pub attempts: u32,
    /// `<id> (<route/model>)` of the worker that ran it, when one did.
    pub worker: Option<String>,
    pub needs: Option<String>,
    pub error: Option<String>,
    /// The chain the step walked, in order, head first (ADR-0054 item 4): one entry per
    /// model the step turned to, including the link a cap skipped. Empty when the step
    /// was refused before it reached any model.
    pub models: Vec<ModelTry>,
}

// ------------------------------------------------------------------ journal (item 6)

/// One line of a run's `journal.jsonl`, append-only. `Dispatch` is written BEFORE the
/// worker starts (caps are rebuilt from these on resume, counting every dispatch), `Result`
/// after each step, `Ended` last.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum JournalRecord {
    Started {
        run: RunId,
        /// Hex digest of the script source, so a replay can say whether the script changed.
        script_hash: String,
        args: Value,
        resumed_from: Option<RunId>,
    },
    Phase {
        name: String,
    },
    Dispatch {
        call: CallId,
        label: Option<String>,
        role: String,
        model: String,
        wire_model: String,
        attempt: u32,
        prompt: String,
        opts: Value,
    },
    /// A dispatch refused by a cap, before anything ran.
    Capped {
        call: CallId,
        wire_model: String,
        used: u32,
        limit: u32,
    },
    /// A call answered from the journal of `from` without a worker.
    Replayed {
        call: CallId,
        from: RunId,
    },
    /// One hop of a role's fallback chain (ADR-0054 item 4), written BEFORE the next
    /// model is dispatched: the link the step moved on from, the link it moves to, and
    /// why (the route error, or the cap that refused the link).
    Fallback {
        call: CallId,
        from: String,
        to: String,
        error: String,
    },
    Result {
        call: CallId,
        envelope: StepEnvelope,
    },
    Ended {
        outcome: RunOutcome,
        counts: Counts,
        error: Option<String>,
    },
}

// ------------------------------------------------------------------ runs (item 7)

/// How a run ended. `Completed` only when no step failed, was blocked, capped or
/// cancelled; `CompletedWithIssues` when the script returned but some did; `Failed`
/// when the script itself did not return (parse error, runtime error, limit).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    CompletedWithIssues,
    Failed,
    Cancelled,
}

/// The counts the host's run envelope carries, so a script cannot hide failed workers.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub steps: u32,
    pub replayed: u32,
    pub done: u32,
    pub blocked: u32,
    pub failed: u32,
    pub cancelled: u32,
    /// `done` steps whose evidence is `not verified`.
    pub not_verified: u32,
    pub capped: u32,
    pub invalid_output: u32,
    /// Hops a role's fallback chain walked (ADR-0054 item 4): one per model a step moved
    /// on from, so the cheap model's share of a run and every substitute are visible.
    pub fell_back: u32,
}

/// One line per step for the host's rendering and `workflow_result`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepLine {
    pub call: CallId,
    pub label: Option<String>,
    pub role: String,
    pub model: String,
    pub worker: Option<String>,
    pub status: StepStatus,
    pub schema: String,
    pub evidence: Option<String>,
    pub attempts: u32,
    pub replayed: bool,
    pub error: Option<String>,
    /// The chain the step walked, in order, head first (ADR-0054 item 4).
    pub models: Vec<ModelTry>,
}

impl StepLine {
    /// The chain as a line names it: `a`, or `a route failed → b` when the step moved on
    /// (ADR-0054 item 4). The role's own model when nothing was dispatched.
    pub fn model_chain(&self) -> String {
        if self.models.is_empty() {
            return self.model.clone();
        }
        self.models
            .iter()
            .map(|tried| match tried.moved_on {
                None => tried.model.clone(),
                Some(MovedOn::RouteFailed) => format!("{} route failed", tried.model),
                Some(MovedOn::Capped) => format!("{} capped", tried.model),
            })
            .collect::<Vec<_>>()
            .join(" → ")
    }
}

/// A finished run: the host envelope around the script's return.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RunReport {
    pub id: RunId,
    pub outcome: RunOutcome,
    /// What the script returned (`null` when it did not).
    pub value: Value,
    pub counts: Counts,
    pub steps: Vec<StepLine>,
    /// The script's own failure (with line and column) or the cancellation, if any.
    pub error: Option<String>,
    /// Where `journal.jsonl` and `result.json` are.
    pub run_dir: PathBuf,
}

/// A running run, compactly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunProgress {
    pub phase: Option<String>,
    pub steps_started: u32,
    pub steps_ended: u32,
    pub replayed: u32,
    /// The last `log` lines, newest last.
    pub log: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum RunStatus {
    Running(RunProgress),
    Ended(RunReport),
}

/// What starts a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartRequest {
    pub script: String,
    /// The script's `args` (an object; `null` means `{}`).
    pub args: Value,
    /// Replay the longest unchanged prefix of that run's journal, then run the rest.
    pub resume_from: Option<RunId>,
    /// `--role r=E/P[:effort]` overrides: the role keeps its tool grant.
    pub role_models: BTreeMap<String, String>,
    /// The run's workspace, when the caller has one to name.
    pub workspace: Option<PathBuf>,
}

/// Every way a workflow service call can fail.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WorkflowError {
    #[error("no such workflow run")]
    UnknownRun,
    #[error("script does not parse: {message} [line {line}, column {column}]")]
    Parse {
        message: String,
        line: u32,
        column: u32,
    },
    /// A role that does not resolve, an empty grant, an unknown `resume_from`, …
    #[error("workflow cannot start: {0}")]
    Preflight(String),
    #[error("the workflow service has shut down")]
    ShutDown,
    #[error("workflow i/o: {0}")]
    Io(String),
}

/// The typed workflow API a tool depends on (ADR-0053 item 1). Runs are retained for
/// the service's lifetime: a result stays retrievable by id however late anyone asks.
pub trait WorkflowService: Send + Sync {
    /// Compiles, preflights (every role resolves, grants are non-empty, `resume_from`
    /// exists) and starts NOW, in the background. Nothing is started on `Err`.
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>>;

    /// The current status. Never blocks on the run.
    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>>;

    /// Resolves as soon as the run is `Ended` (immediately if so already). If `cancel`
    /// fires first, resolves `Ok(Running(..))`. Cancel-safe and repeatable.
    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>>;

    /// Cancels the run: every in-flight step is cancelled, the script stops, the journal
    /// gets its `Ended` line. Idempotent; an ended run is a no-op `Ok`.
    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>>;

    /// Every run this service has started, with its status.
    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>>;
}

/// What the host renders, one call per event, from the engine's thread. Every method is
/// a no-op by default. `run_ended` is the ONE place the host wakes the parent from.
pub trait WorkflowObserver: Send + Sync {
    fn run_started(&self, _id: &RunId, _resumed_from: Option<&RunId>) {}
    fn phase(&self, _id: &RunId, _name: &str) {}
    fn log(&self, _id: &RunId, _text: &str) {}
    fn step_started(&self, _id: &RunId, _request: &StepRequest, _worker: &WorkerRef) {}
    fn step_ended(&self, _id: &RunId, _line: &StepLine) {}
    fn run_ended(&self, _id: &RunId, _report: &RunReport) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shipped_defaults_name_the_four_roles_and_cap_fable() {
        let settings = WorkflowSettings::shipped();
        assert_eq!(
            settings.roles.keys().cloned().collect::<Vec<_>>(),
            ["judge", "reviewer", "verifier", "worker"]
        );
        assert_eq!(settings.caps.get("claude-fable-5"), Some(&3));
        assert_eq!(settings.max_steps, 200);
        assert_eq!(settings.max_threads, 64);
        for role in settings.roles.values() {
            assert!(!role.tools.is_empty(), "{role:?}");
        }
    }

    // (g)
    #[test]
    fn the_worker_is_deepseek_with_the_shipped_fallback_chains() {
        let settings = WorkflowSettings::shipped();
        let worker = &settings.roles["worker"];
        assert_eq!(worker.model, "deepseek2/deepseek-v4.1-flash");
        assert_eq!(worker.tools, ["read", "grep", "edit", "shell"]);
        assert_eq!(worker.fallback, ["gpt/gpt-6-sol", "claude/claude-opus-5-5"]);
        let verifier = &settings.roles["verifier"];
        assert_eq!(verifier.model, "deepseek2/deepseek-v4.1-flash");
        assert_eq!(verifier.fallback, ["gpt/gpt-6-sol"]);
        assert_eq!(
            settings.roles["reviewer"].model,
            "claude/claude-opus-5-5:high"
        );
        assert!(settings.roles["reviewer"].fallback.is_empty());
        assert_eq!(settings.roles["judge"].model, "claude/claude-fable-5");
        assert!(settings.roles["judge"].fallback.is_empty());
    }

    // (h)
    #[test]
    fn a_user_role_that_names_model_only_has_an_empty_chain() {
        let user: WorkflowSettings = toml::from_str(
            r#"
            [roles.worker]
            model = "deepseek2/deepseek-v4.1-flash"
            tools = ["read"]
            "#,
        )
        .unwrap();
        let merged = WorkflowSettings::shipped().overridden_by(user);
        assert!(
            merged.roles["worker"].fallback.is_empty(),
            "a named role replaces the shipped one wholesale"
        );
        assert_eq!(
            merged.roles["verifier"].fallback,
            ["gpt/gpt-6-sol"],
            "a role the user did not name keeps its shipped chain"
        );

        let named: WorkflowSettings = toml::from_str(
            r#"
            [roles.verifier]
            model = "claude/claude-opus-5-5"
            fallback = ["gpt/gpt-6-sol", "deepseek2/deepseek-v4.1-flash"]
            tools = ["read"]
            "#,
        )
        .unwrap();
        let merged = WorkflowSettings::shipped().overridden_by(named);
        assert_eq!(
            merged.roles["verifier"].fallback,
            ["gpt/gpt-6-sol", "deepseek2/deepseek-v4.1-flash"]
        );
    }

    #[test]
    fn a_user_table_replaces_named_roles_and_caps_and_keeps_the_rest() {
        let user: WorkflowSettings = toml::from_str(
            r#"
            max_steps = 10
            [roles.worker]
            model = "deepseek2/deepseek-v4.1-flash"
            tools = ["read"]
            [caps]
            "claude-opus-5-5" = 5
            "#,
        )
        .unwrap();
        let merged = WorkflowSettings::shipped().overridden_by(user);
        assert_eq!(
            merged.roles["worker"].model,
            "deepseek2/deepseek-v4.1-flash"
        );
        assert_eq!(merged.roles["worker"].tools, ["read"]);
        assert_eq!(merged.roles["judge"].model, "claude/claude-fable-5");
        assert_eq!(merged.caps.get("claude-fable-5"), Some(&3));
        assert_eq!(merged.caps.get("claude-opus-5-5"), Some(&5));
        assert_eq!(merged.max_steps, 10);
        assert_eq!(
            merged.max_threads, 64,
            "a scalar the user left out keeps its default"
        );
    }

    #[test]
    fn an_unknown_settings_key_is_an_error() {
        let error = toml::from_str::<WorkflowSettings>("max_stepz = 1").unwrap_err();
        assert!(error.to_string().contains("max_stepz"), "{error}");
    }

    #[test]
    fn the_journal_record_round_trips_as_tagged_json() {
        let record = JournalRecord::Capped {
            call: CallId("abc".into()),
            wire_model: "claude-fable-5".into(),
            used: 3,
            limit: 3,
        };
        let json = serde_json::to_string(&record).unwrap();
        assert!(json.starts_with(r#"{"kind":"capped""#), "{json}");
        assert_eq!(
            serde_json::from_str::<JournalRecord>(&json).unwrap(),
            record
        );

        let hop = JournalRecord::Fallback {
            call: CallId("abc".into()),
            from: "deepseek2/deepseek-v4.1-flash".into(),
            to: "gpt/gpt-6-sol".into(),
            error: "InsufficientBalance: the account has no balance".into(),
        };
        let json = serde_json::to_string(&hop).unwrap();
        assert!(json.starts_with(r#"{"kind":"fallback""#), "{json}");
        assert_eq!(serde_json::from_str::<JournalRecord>(&json).unwrap(), hop);
    }

    #[test]
    fn a_line_names_the_chain_it_walked() {
        let line = |models: Vec<ModelTry>| StepLine {
            call: CallId("abc".into()),
            label: None,
            role: "worker".into(),
            model: "deepseek2/deepseek-v4.1-flash".into(),
            worker: Some("w7 (deepseek2/deepseek-v4.1-flash)".into()),
            status: StepStatus::Done,
            schema: "not_requested".into(),
            evidence: None,
            attempts: 1,
            replayed: false,
            error: None,
            models,
        };
        let tried = |model: &str, moved_on: Option<MovedOn>| ModelTry {
            model: model.to_string(),
            moved_on,
        };
        assert_eq!(
            line(vec![tried("deepseek2/deepseek-v4.1-flash", None)]).model_chain(),
            "deepseek2/deepseek-v4.1-flash"
        );
        assert_eq!(
            line(Vec::new()).model_chain(),
            "deepseek2/deepseek-v4.1-flash",
            "a step refused before dispatch still names the role's model"
        );
        assert_eq!(
            line(vec![
                tried("deepseek2/deepseek-v4.1-flash", Some(MovedOn::RouteFailed)),
                tried("gpt/gpt-6-sol", None),
            ])
            .model_chain(),
            "deepseek2/deepseek-v4.1-flash route failed → gpt/gpt-6-sol"
        );
        assert_eq!(
            line(vec![
                tried("fable-a", Some(MovedOn::Capped)),
                tried("gpt/gpt-6-sol", None),
            ])
            .model_chain(),
            "fable-a capped → gpt/gpt-6-sol"
        );
    }
}
