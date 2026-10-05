//! ADR-0116 component. Answers can only return from the host import.
#![forbid(unsafe_code)]
use bindings::p1::module::{types::DeclarationKind, user_questions as host};
use p1_bindings_tool::generated::{
    self as bindings, CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall,
    ToolDeclaration, ToolOutcome,
};
use p1_question_guest as guest;
use serde::Deserialize;
#[derive(Deserialize)]
struct Call {
    input: Input,
}
#[derive(Deserialize)]
#[serde(tag = "kind", content = "raw", rename_all = "snake_case")]
enum Input {
    Json(String),
    Text(String),
}
struct QuestionTool;
impl Guest for QuestionTool {
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
        serde_json::json!({"verb":"ask", "target":"user", "destructive":false}).to_string()
    }
    fn describe_result(_: ToolCall, result: HistoryItem) -> ResultDescription {
        let value: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
        serde_json::json!({"summary":value["content"].as_str().unwrap_or("").lines().next().unwrap_or("")}).to_string()
    }
    fn execute(call: ToolCall) -> ToolOutcome {
        let questions = match serde_json::from_str::<Call>(&call) {
            Ok(Call {
                input: Input::Json(raw),
            }) => guest::parse(&raw),
            Ok(Call {
                input: Input::Text(text),
            }) => {
                let _ = text;
                Err("expected JSON input".into())
            }
            Err(e) => Err(e.to_string()),
        };
        let questions = match questions {
            Ok(q) => q,
            Err(e) => return outcome("error", &guest::invalid(&e)),
        };
        let request = questions
            .iter()
            .map(|q| host::Question {
                question: q.question.clone(),
                header: q.header.clone(),
                multi_select: q.multi_select,
                options: q
                    .options
                    .iter()
                    .map(|o| host::QuestionOption {
                        label: o.label.clone(),
                        description: o.description.clone(),
                        preview: o.preview.clone(),
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let asked = match host::ask(&request) {
            Err(host::QuestionError::Invalid(e)) => return outcome("error", &guest::invalid(&e)),
            Ok(host::Asked::Cancelled) => guest::Asked::Cancelled,
            Ok(host::Asked::NoInteractiveUser) => guest::Asked::NoInteractiveUser,
            Ok(host::Asked::Answered(a)) => guest::Asked::Answered(
                a.into_iter()
                    .map(|a| guest::Answer {
                        chosen: a.chosen,
                        free_text: a.free_text,
                    })
                    .collect(),
            ),
        };
        let (status, text) = guest::format(&questions, asked);
        let content = guest::bound_output(&text);
        outcome(status, &content)
    }
}
fn outcome(status: &str, content: &str) -> String {
    serde_json::json!({"status":status,"content":content}).to_string()
}
bindings::export!(QuestionTool);
