//! The Codex `request_user_input` tool as a component (`p1/request-user-input`, ADR-0219):
//! Codex's questions and answers over the host import `ask_user_question` asks through
//! (ADR-0116). Answers can only return from that import.
#![forbid(unsafe_code)]

use bindings::p1::module::{types::DeclarationKind, user_questions as host};
use p1_bindings_tool::generated::{
    self as bindings, CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall,
    ToolDeclaration, ToolOutcome,
};
use p1_question_guest::{Answer, Asked};
use p1_request_user_input_guest as face;
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

struct RequestUserInput;

impl Guest for RequestUserInput {
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
        serde_json::json!({"verb":"ask", "target":"user", "destructive":false}).to_string()
    }

    fn describe_result(_: ToolCall, result: HistoryItem) -> ResultDescription {
        let value: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
        serde_json::json!({"summary":value["content"].as_str().unwrap_or("").lines().next().unwrap_or("")}).to_string()
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let questions = match serde_json::from_str::<Call>(&call) {
            Ok(Call {
                input: Input::Json(raw) | Input::Text(raw),
            }) => face::parse(&raw),
            Err(e) => Err(e.to_string()),
        };
        let questions = match questions {
            Ok(questions) => questions,
            Err(e) => return outcome("error", &e),
        };
        let request = face::to_host(&questions)
            .into_iter()
            .map(|q| host::Question {
                question: q.question,
                header: q.header,
                multi_select: q.multi_select,
                options: q
                    .options
                    .into_iter()
                    .map(|o| host::QuestionOption {
                        label: o.label,
                        description: o.description,
                        preview: o.preview,
                    })
                    .collect(),
            })
            .collect::<Vec<_>>();
        let asked = match host::ask(&request) {
            Err(host::QuestionError::Invalid(e)) if e == p1_question_guest::NOT_INVITED => {
                Asked::NotInvited
            }
            Err(host::QuestionError::Invalid(e)) => return outcome("error", &e),
            Ok(host::Asked::Cancelled) => Asked::Cancelled,
            Ok(host::Asked::NoInteractiveUser) => Asked::NoInteractiveUser,
            Ok(host::Asked::Answered(answers)) => Asked::Answered(
                answers
                    .into_iter()
                    .map(|a| Answer {
                        chosen: a.chosen,
                        free_text: a.free_text,
                    })
                    .collect(),
            ),
        };
        let (status, text) = face::format(&questions, asked);
        outcome(status, &p1_question_guest::bound_output(&text))
    }
}

fn outcome(status: &str, content: &str) -> String {
    serde_json::json!({"status":status,"content":content}).to_string()
}

bindings::export!(RequestUserInput);
