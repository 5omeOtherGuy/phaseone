//! The JSON contract of the `workflow-decision` world (S0-R1.1): what the substrate passes to
//! `plan-step` and `accept-step` and what they answer. `p1-workflow` owns these schemas
//! (docs/design/modules/wit.md, "Worlds"); the world itself carries them as opaque `json`.
//!
//! Every top-level value carries [`CONTRACT_VERSION`]. A decision refuses a snapshot, request
//! or outcome of another version, and the substrate refuses a transition of another version:
//! a component built against a different contract fails its steps with a clear error instead
//! of misreading a field.

use serde::{Deserialize, Serialize};

use crate::api::{
    CallId, ModelTry, MovedOn, ResolvedModel, RunId, StepEnd, StepEnvelope, WorkerRef,
};

/// The one contract version this build speaks.
pub const CONTRACT_VERSION: u32 = 1;

/// What the decision sees of the run and of the step it decides for. The substrate builds a
/// fresh snapshot for every call and keeps every piece of state itself: the decision is
/// handed a copy and can change nothing but through the transition it answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Snapshot {
    pub version: u32,
    pub run: RunId,
    /// `agent()` calls the run may make (replayed calls count too).
    pub max_steps: u32,
    /// Whether the run has a base commit a step's worktree could branch from (ADR-0073).
    pub has_base: bool,
    /// The role the call named, as preflight resolved it; `None` when the run has no such role.
    pub role: Option<RoleView>,
    /// The replay prefix of a resumed run, as it stands now.
    pub replay: ReplayView,
    /// The step so far.
    pub step: StepProgress,
}

/// A resolved role: its chain, head first (ADR-0054 item 2), and its tool grant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RoleView {
    pub chain: Vec<ResolvedModel>,
    pub tools: Vec<String>,
}

/// The replay prefix: the recorded `done` results no call has taken yet. Once a call missed
/// it the prefix is latched off and nothing more is replayed (the prefix rule, ADR-0053).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayView {
    pub latched_off: bool,
    pub open: Vec<ReplayEntry>,
}

/// One recorded result not yet taken: its position in the prefix and the call it answered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReplayEntry {
    pub index: u32,
    pub call: CallId,
}

/// What one step's chain spent (ADR-0054 item 4): dispatches (starts plus repairs), the links
/// a cap refused, and the hops between links. Only the substrate changes these.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepCost {
    pub attempts: u32,
    pub capped: u32,
    pub fell_back: u32,
}

/// The substrate's record of one step, in the order its transitions were applied.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct StepProgress {
    /// The step's ordinal in its run, from 1 in `agent()` call order (ADR-0075).
    pub ordinal: u32,
    pub cost: StepCost,
    /// The link of the role's chain the step is on, once one was dispatched.
    pub link: Option<u32>,
    /// Set when the step moved on from `link`: the next plan picks the link after it.
    pub moved_on: bool,
    /// The links the step moved on from, in order.
    pub walked: Vec<ModelTry>,
    /// Why the step last moved on: the route error or the cap refusal.
    pub last_error: String,
    /// The worker of the last link moved on from, as a step line names it.
    pub last_worker: Option<String>,
    /// Whether any link was left for a route failure.
    pub route_failed: bool,
    /// The one repair turn (ADR-0053 item 5, ADR-0072), once asked for.
    pub repair: Option<RepairTurn>,
}

/// The repair turn a step took: the worker it runs in and the envelope that stands if the
/// repair cannot run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RepairTurn {
    pub worker: WorkerRef,
    pub rejected: StepEnvelope,
}

/// What asks `plan-step` for the next move of a step: the `agent()` call as the script made it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanRequest {
    pub version: u32,
    pub call: CallId,
    pub label: Option<String>,
    pub role: String,
    /// The worktree slug the call asked for (ADR-0073).
    pub worktree: Option<String>,
    /// The call's own grant, which replaces the role's for this step.
    pub tools: Option<Vec<String>>,
}

/// What the substrate hands `accept-step`: how the thing it just did for the step ended.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AttemptOutcome {
    pub version: u32,
    pub call: CallId,
    pub label: Option<String>,
    pub attempt: Attempt,
}

/// How one piece of a step's work ended. Whether it was the repair turn is the snapshot's
/// `step.repair`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Attempt {
    /// The step's worktree could not be prepared; nothing was dispatched.
    WorktreeRefused { error: String },
    /// The run was cancelled while the worktree was prepared.
    WorktreeCancelled,
    /// A configured cap refused the dispatch; the substrate journalled `Capped`.
    Capped {
        wire_model: String,
        used: u32,
        limit: u32,
    },
    /// The `Dispatch` line could not be written, so nothing ran.
    JournalFailed { error: String },
    /// The runner could not start the worker or the repair turn.
    RunnerRefused { reason: String },
    /// The run was cancelled while the worker or the repair turn ran.
    Cancelled,
    /// The first turn of the link's worker ended.
    Ended { worker: WorkerRef, end: StepEnd },
    /// The repair turn ended.
    RepairEnded { end: StepEnd },
}

/// What a decision answers.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Transition {
    pub version: u32,
    pub action: Action,
}

impl Transition {
    pub fn new(action: Action) -> Self {
        Self {
            version: CONTRACT_VERSION,
            action,
        }
    }
}

/// The next move of a step.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Action {
    /// Answer the call with the recorded result at `entry` of the replay prefix.
    Replay { entry: u32 },
    /// Dispatch the first attempt on link `link` of the role's chain, whose reference is
    /// `model`, granting `tools`. `latch_replay`: the replay prefix did not answer the call.
    Dispatch {
        link: u32,
        model: String,
        tools: Vec<String>,
        latch_replay: bool,
    },
    /// The one repair turn in the same worker, with `message`; `rejected` stands if the
    /// repair cannot run.
    Repair {
        message: String,
        rejected: StepEnvelope,
    },
    /// Leave the current link for the next one (ADR-0054 item 3): `tried` is the link as the
    /// step line names it, `error` why, `worker` the worker it ran.
    MoveOn {
        reason: MovedOn,
        tried: ModelTry,
        error: String,
        worker: Option<String>,
    },
    /// The step ends with `envelope`. `latch_replay` as for `Dispatch`.
    End {
        envelope: StepEnvelope,
        latch_replay: bool,
    },
    /// The run was cancelled: the step ends with this `cancelled` envelope.
    Cancelled { envelope: StepEnvelope },
}

/// `Ok` for the version this build speaks, otherwise why `what` is refused.
pub(crate) fn check_version(what: &str, version: u32) -> Result<(), String> {
    if version == CONTRACT_VERSION {
        Ok(())
    } else {
        Err(format!(
            "unknown {what} version {version} (this build speaks version {CONTRACT_VERSION})"
        ))
    }
}
