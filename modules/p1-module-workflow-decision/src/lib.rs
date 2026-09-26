//! A workflow's step decisions as a guest component (`p1/workflow-decision`, world
//! `p1:module/workflow-decision@1.0.0`): `plan-step` and `accept-step` over JSON, the
//! decisions of `crates/p1-workflow/src/decision/` with the run's state kept native.
//!
//! One source, no copy: the decision code and the value types its contract carries are
//! compiled here from their files in `crates/p1-workflow` by path — `decision/` (the
//! decisions and their JSON contract) and `api/values.rs` (the step envelope and what it is
//! made of), mounted as this crate's `api` so the decision code's `crate::api::…` paths
//! resolve unchanged. Those files need serde, serde_json and std only, the guest
//! dependencies D-XO-8 allows. The exports are thin shells over `plan_step_json` and
//! `accept_step_json`, the same functions the native crate tests.
//!
//! Its capabilities are `control` and `clock`, the class allocation; the decisions are pure
//! and call neither, so the component imports nothing it could act through.
#![forbid(unsafe_code)]

#[path = "../../../crates/p1-workflow/src/api/values.rs"]
pub mod api;
#[path = "../../../crates/p1-workflow/src/decision/mod.rs"]
pub mod decision;

use p1_bindings_workflow_decision::generated::Guest;

struct WorkflowDecision;

impl Guest for WorkflowDecision {
    fn plan_step(snapshot: String, request: String) -> Result<String, String> {
        decision::plan_step_json(&snapshot, &request)
    }

    fn accept_step(snapshot: String, outcome: String) -> Result<String, String> {
        decision::accept_step_json(&snapshot, &outcome)
    }
}

p1_bindings_workflow_decision::generated::export!(WorkflowDecision);
