//! The value types of the workflow API that the decision contract (`crate::decision`)
//! carries: the step envelope and everything it is made of. Re-exported by `api`, where
//! they belong to the frozen surface.
//!
//! This file depends on serde, serde_json and std only, because the workflow-decision
//! component (`modules/p1-module-workflow-decision`) compiles it by path together with
//! `decision/`: a guest crate may use serde and serde_json and nothing else (D-XO-8), and
//! the decisions keep one source.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};
use serde_json::Value;

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

/// Identifier of one run within one service: `wf1`, `wf2`, … never reused.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
pub struct RunId(pub String);

/// The stable identity of one `agent()` call: a hash of (label, prompt, canonical opts),
/// never a sequence number — `parallel` reaches `agent()` in a different order each run.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct CallId(pub String);

/// A step's own git worktree (ADR-0073), as the host prepared it and the step left it.
/// The script reads `r.worktree.path`, `.branch` and `.head`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    /// `task/<slug>`.
    pub branch: String,
    /// The worktree's `HEAD` commit after the step ended.
    pub head: String,
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
/// its accepted `finish` outcome — never from its prose. Serde because the decision
/// contract carries it to `accept-step` ([`crate::decision::Attempt`]).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
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
    /// The step's own git worktree (ADR-0073), when it asked for one and got it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<WorktreeInfo>,
}
