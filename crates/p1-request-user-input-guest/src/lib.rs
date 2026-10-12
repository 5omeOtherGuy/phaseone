//! The Codex face of `request_user_input` (ADR-0218) over p1's `user-questions` capability,
//! the one `ask_user_question` asks through (ADR-0116).
//!
//! Copied from openai/codex at tag `rust-v0.162.1` (commit 092d3acd), Apache-2.0
//! (`docs/licenses/openai-codex/`): `codex-rs/core/src/tools/handlers/request_user_input_spec.rs`
//! (schema, description, the mode sentence, the empty-options refusal),
//! `core/src/tools/handlers/request_user_input.rs` (the answer JSON and the cancellation text)
//! and `protocol/src/request_user_input.rs` (the answer shape).
//!
//! Codex offers the tool in Plan mode only. p1 has no Plan mode: the host asks only after the
//! user invited questions (ADR-0116), and a call outside that invitation gets the sentence
//! Codex gives a call outside Plan mode.
#![forbid(unsafe_code)]

use p1_question_guest::{Answer, Asked, Question, QuestionOption};
use serde::Deserialize;
use serde_json::{Map, Value, json};

pub const NAME: &str = "request_user_input";
pub const DESCRIPTION: &str = "Request user input for one to three short questions and wait for the response. This tool is only available in Plan mode.";
/// `request_user_input_unavailable_message` for Codex's Default mode: p1's host refused the
/// call because the user has not invited questions.
pub const UNAVAILABLE: &str = "request_user_input is unavailable in Default mode";
pub const CANCELLED: &str = "request_user_input was cancelled before receiving a response";
pub const EMPTY_OPTIONS: &str = "request_user_input requires non-empty options for every question";
/// p1's own case: a run with nobody to ask (ADR-0116); Codex has no headless answer.
pub const NO_INTERACTIVE_USER: &str =
    "no interactive user — decide without asking, or end the turn with the question";

pub fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "questions": {
                "type": "array",
                "description": "Questions to show the user. Prefer 1 and do not exceed 3",
                "items": {
                    "type": "object",
                    "properties": {
                        "header": { "type": "string", "description": "Short header label shown in the UI (12 or fewer chars)." },
                        "id": { "type": "string", "description": "Stable identifier for mapping answers (snake_case)." },
                        "options": {
                            "type": "array",
                            "description": "Provide 2-3 mutually exclusive choices. Put the recommended option first and suffix its label with \"(Recommended)\". Do not include an \"Other\" option in this list; the client will add a free-form \"Other\" option automatically.",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "description": { "type": "string", "description": "One short sentence explaining impact/tradeoff if selected." },
                                    "label": { "type": "string", "description": "User-facing label (1-5 words)." }
                                },
                                "required": ["label", "description"],
                                "additionalProperties": false
                            }
                        },
                        "question": { "type": "string", "description": "Single-sentence prompt shown to the user." }
                    },
                    "required": ["id", "header", "question", "options"],
                    "additionalProperties": false
                }
            }
        },
        "required": ["questions"],
        "additionalProperties": false
    })
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Option_ {
    pub label: String,
    pub description: String,
}

/// One question as the model sends it: Codex's `RequestUserInputQuestion` without the
/// client-side flags.
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct CodexQuestion {
    pub id: String,
    pub header: String,
    pub question: String,
    #[serde(default)]
    pub options: Option<Vec<Option_>>,
}

#[derive(Deserialize)]
struct Input {
    questions: Vec<CodexQuestion>,
}

/// Parse and normalise as Codex does: every question needs options; the free-text answer is
/// always offered (Codex sets `isOther`; p1's host always takes free text).
pub fn parse(raw: &str) -> Result<Vec<CodexQuestion>, String> {
    let input: Input = serde_json::from_str(raw)
        .map_err(|e| format!("failed to parse function arguments: {e}"))?;
    if input
        .questions
        .iter()
        .any(|question| question.options.as_ref().is_none_or(Vec::is_empty))
    {
        return Err(EMPTY_OPTIONS.to_string());
    }
    Ok(input.questions)
}

/// The questions as p1's host asks them: single choice, no preview.
pub fn to_host(questions: &[CodexQuestion]) -> Vec<Question> {
    questions
        .iter()
        .map(|question| Question {
            question: question.question.clone(),
            header: question.header.clone(),
            multi_select: false,
            options: question
                .options
                .iter()
                .flatten()
                .map(|option| QuestionOption {
                    label: option.label.clone(),
                    description: option.description.clone(),
                    preview: None,
                })
                .collect(),
        })
        .collect()
}

/// The response as Codex serialises `RequestUserInputResponse`: the chosen labels and the
/// free text under each question's id. `ok` is false for every refusal, whose content is
/// the sentence alone.
pub fn format(questions: &[CodexQuestion], asked: Asked) -> (&'static str, String) {
    match asked {
        Asked::Answered(answers) => ("ok", answers_json(questions, &answers)),
        Asked::Cancelled => ("cancelled", CANCELLED.to_string()),
        Asked::NotInvited => ("error", UNAVAILABLE.to_string()),
        Asked::NoInteractiveUser => ("error", NO_INTERACTIVE_USER.to_string()),
    }
}

fn answers_json(questions: &[CodexQuestion], answers: &[Answer]) -> String {
    let mut by_id = Map::new();
    for (question, answer) in questions.iter().zip(answers) {
        let mut given: Vec<Value> = answer.chosen.iter().map(|label| json!(label)).collect();
        if let Some(free) = &answer.free_text {
            given.push(json!(free));
        }
        by_id.insert(question.id.clone(), json!({ "answers": given }));
    }
    json!({ "answers": by_id }).to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CALL: &str = r#"{"questions":[{"id":"scope","header":"Scope","question":"Which scope?","options":[{"label":"Small (Recommended)","description":"Only the bug."},{"label":"Large","description":"The module."}]}]}"#;

    #[test]
    fn the_face_is_codex_s_verbatim() {
        assert_eq!(
            DESCRIPTION,
            "Request user input for one to three short questions and wait for the response. This tool is only available in Plan mode."
        );
        let schema = input_schema();
        let question = &schema["properties"]["questions"]["items"];
        assert_eq!(
            question["required"],
            json!(["id", "header", "question", "options"])
        );
        assert_eq!(
            question["properties"]["options"]["items"]["required"],
            json!(["label", "description"])
        );
        assert_eq!(
            schema["properties"]["questions"]["description"],
            "Questions to show the user. Prefer 1 and do not exceed 3"
        );
    }

    #[test]
    fn questions_need_options_and_map_to_single_choice_host_questions() {
        let questions = parse(CALL).unwrap();
        let host = to_host(&questions);
        assert_eq!(host.len(), 1);
        assert_eq!(host[0].header, "Scope");
        assert!(!host[0].multi_select);
        assert_eq!(host[0].options[0].label, "Small (Recommended)");
        assert_eq!(
            parse(r#"{"questions":[{"id":"a","header":"A","question":"?","options":[]}]}"#)
                .unwrap_err(),
            EMPTY_OPTIONS
        );
        assert_eq!(
            parse(r#"{"questions":[{"id":"a","header":"A","question":"?"}]}"#).unwrap_err(),
            EMPTY_OPTIONS
        );
    }

    #[test]
    fn answers_are_codex_s_json_and_refusals_its_sentences() {
        let questions = parse(CALL).unwrap();
        let (status, content) = format(
            &questions,
            Asked::Answered(vec![Answer {
                chosen: vec!["Large".into()],
                free_text: Some("but carefully".into()),
            }]),
        );
        assert_eq!(status, "ok");
        assert_eq!(
            content,
            r#"{"answers":{"scope":{"answers":["Large","but carefully"]}}}"#
        );
        assert_eq!(
            format(&questions, Asked::NotInvited),
            ("error", UNAVAILABLE.to_string())
        );
        assert_eq!(
            format(&questions, Asked::Cancelled),
            ("cancelled", CANCELLED.to_string())
        );
    }
}
