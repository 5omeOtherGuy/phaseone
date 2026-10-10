//! Pure plan proposals; the core commits and publishes the resulting snapshot.
#![forbid(unsafe_code)]
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome, p1::module::types::DeclarationKind,
};
use p1_todo_guest as guest;
use serde_json::{Value, json};

struct Todo;
impl Guest for Todo {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: guest::NAME.into(),
            description: guest::DESCRIPTION.into(),
            kind: DeclarationKind::Function(guest::input_schema().to_string()),
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
            return json!({"status":"error", "content":"todo_write expects JSON arguments"})
                .to_string();
        }
        guest::execute(call["input"]["raw"].as_str().unwrap_or("")).to_string()
    }
}
p1_bindings_tool::generated::export!(Todo);
