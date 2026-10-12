//! The Codex face of `update_plan` (ADR-0219): the plan tool the Codex CLI ships off by
//! default, over p1's session plan (ADR-0159).
//!
//! Copied from openai/codex at tag `rust-v0.162.1` (commit 092d3acd), Apache-2.0
//! (`docs/licenses/openai-codex/`): `codex-rs/core/src/tools/handlers/plan_spec.rs` and
//! `plan.rs` (`Plan updated`).
//!
//! The model reads `Plan updated`, as in Codex; the session takes the plan from the call's
//! journalled arguments (`p1-todo-session`), since the result carries no snapshot. A Codex
//! step has no priority: every entry is `medium`.
#![forbid(unsafe_code)]

use p1_contracts::plan::{PlanEntry, PlanPriority, PlanStatus};
use serde::Deserialize;
use serde_json::{Value, json};

pub const NAME: &str = "update_plan";
pub const DESCRIPTION: &str = "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n";
pub const PLAN_UPDATED: &str = "Plan updated";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "explanation": { "type": "string", "description": "Optional explanation for this plan update." },
            "plan": {
                "type": "array",
                "description": "The list of steps",
                "items": {
                    "type": "object",
                    "properties": {
                        "status": { "type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Step status." },
                        "step": { "type": "string", "description": "Task step text." }
                    },
                    "required": ["step", "status"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["plan"],
        "additionalProperties": false
    })
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct PlanItem {
    pub step: String,
    pub status: PlanStatus,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Input {
    #[serde(default)]
    pub explanation: Option<String>,
    pub plan: Vec<PlanItem>,
}

/// Codex's `parse_update_plan_arguments`.
pub fn parse(raw: &str) -> Result<Input, String> {
    serde_json::from_str(raw).map_err(|e| format!("failed to parse function arguments: {e}"))
}

/// The session's plan entries for a call: what the front end's plan port shows.
pub fn snapshot(raw: &str) -> Result<Vec<PlanEntry>, String> {
    Ok(parse(raw)?
        .plan
        .into_iter()
        .map(|item| PlanEntry {
            content: item.step,
            status: item.status,
            priority: PlanPriority::Medium,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_face_is_codex_plan_spec_verbatim() {
        assert_eq!(
            DESCRIPTION,
            "Updates the task plan.\nProvide an optional explanation and a list of plan items, each with a step and status.\nAt most one step can be in_progress at a time.\n"
        );
        let schema = input_schema();
        assert_eq!(schema["required"], json!(["plan"]));
        assert_eq!(
            schema["properties"]["plan"]["items"]["required"],
            json!(["step", "status"])
        );
        assert_eq!(
            schema["properties"]["plan"]["items"]["properties"]["status"]["enum"],
            json!(["pending", "in_progress", "completed"])
        );
    }

    #[test]
    fn the_snapshot_is_the_call_s_steps_at_medium_priority() {
        let entries = snapshot(r#"{"explanation":"go","plan":[{"step":"Read","status":"completed"},{"step":"Fix","status":"in_progress"}]}"#).unwrap();
        assert_eq!(
            entries,
            vec![
                PlanEntry {
                    content: "Read".into(),
                    status: PlanStatus::Completed,
                    priority: PlanPriority::Medium
                },
                PlanEntry {
                    content: "Fix".into(),
                    status: PlanStatus::InProgress,
                    priority: PlanPriority::Medium
                },
            ]
        );
        assert_eq!(
            snapshot(r#"{"plan":[{"step":"x","status":"done"}]}"#).unwrap_err(),
            "failed to parse function arguments: unknown variant `done`, expected one of `pending`, `in_progress`, `completed` at line 1 column 34"
        );
    }
}
