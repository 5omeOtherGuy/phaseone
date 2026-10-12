//! The Codex `update_plan` tool as a component (`p1/update-plan`, ADR-0218). A pure plan
//! proposal: the session commits the snapshot it reads from the journalled call
//! (`p1-todo-session`), and the model reads what Codex shows it.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome, p1::module::types::DeclarationKind,
};
use p1_update_plan_guest as face;
use serde_json::{Value, json};

struct UpdatePlan;

impl Guest for UpdatePlan {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: face::NAME.into(),
            description: face::DESCRIPTION.into(),
            kind: DeclarationKind::Function(face::input_schema().to_string()),
        }
    }

    fn effect(_: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }

    fn describe(_: ToolCall) -> CallDescription {
        json!({"verb":"plan", "target":"session", "destructive":false}).to_string()
    }

    fn describe_result(_: ToolCall, result: HistoryItem) -> ResultDescription {
        let result: Value = serde_json::from_str(&result).unwrap_or_default();
        json!({"summary":result["content"].as_str().unwrap_or("")}).to_string()
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        if p1_bindings_tool::generated::p1::module::control::cancelled() {
            return json!({"status":"cancelled", "content":""}).to_string();
        }
        let call: Value = serde_json::from_str(&call).unwrap_or_default();
        if call["input"]["kind"] != "json" {
            return json!({"status":"error", "content":"update_plan expects JSON arguments"})
                .to_string();
        }
        match face::parse(call["input"]["raw"].as_str().unwrap_or("")) {
            Ok(_) => json!({"status":"ok", "content":face::PLAN_UPDATED}).to_string(),
            Err(error) => json!({"status":"error", "content":error}).to_string(),
        }
    }
}

p1_bindings_tool::generated::export!(UpdatePlan);
