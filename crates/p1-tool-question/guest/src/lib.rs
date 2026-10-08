//! Adapted from /home/phaseonebig/projects/iris-agent/src/tools/ask_user_question.rs.
//! Hidden answer fields are deliberately absent: only the host can collect answers.
#![forbid(unsafe_code)]
use serde::Deserialize;
use std::collections::HashSet;

pub const NAME: &str = "ask_user_question";
pub const DESCRIPTION: &str = "Ask the user structured questions when a genuine ambiguity needs their decision. Use it only when the user asked in this session for questions (e.g. \"ask me questions\"); otherwise the host refuses the call, so decide yourself and continue. Supply 1–4 questions, each with 2–4 options; free text is always available. Silence is not an answer. In a headless run decide without asking, or end the turn with the question. Never submit answers in the input.";
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    #[serde(default, deserialize_with = "preview")]
    pub preview: Option<String>,
}
fn preview<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    String::deserialize(d).map(Some)
}
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: bool,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    questions: Vec<Question>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Answer {
    pub chosen: Vec<String>,
    pub free_text: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Asked {
    Answered(Vec<Answer>),
    Cancelled,
    NoInteractiveUser,
    /// The host refused: no user input of this session invited questions (ADR-0135).
    NotInvited,
}
/// The host's refusal text, sent as `question-error::invalid`; equal to
/// `p1_module_runtime::questions::NOT_INVITED` (checked by p1-tool-question's tests).
pub const NOT_INVITED: &str = "ask_user_question is only available after the user asks you to ask questions; decide yourself and continue";

pub fn input_schema() -> serde_json::Value {
    serde_json::json!({"type":"object","additionalProperties":false,"required":["questions"],"properties":{
        "questions":{"type":"array","minItems":1,"maxItems":4,"items":{"type":"object","additionalProperties":false,
        "required":["question","header","options"],"properties":{
            "question":{"type":"string","minLength":1,"maxLength":2000,"description":"Unique question text; at most 2000 UTF-8 bytes."},
            "header":{"type":"string","minLength":1,"maxLength":12},
            "multi_select":{"type":"boolean","default":false},
            "options":{"type":"array","minItems":2,"maxItems":4,"items":{"type":"object","additionalProperties":false,"required":["label","description"],"properties":{
                "label":{"type":"string","minLength":1,"maxLength":2000,"description":"Unique label, not Other; at most 2000 UTF-8 bytes. Put recommendation first."},
                "description":{"type":"string","minLength":1,"maxLength":2000,"description":"Decision-relevant difference; at most 2000 UTF-8 bytes."},
                "preview":{"type":"string","maxLength":8000,"description":"At most 8000 UTF-8 bytes; unavailable with multi_select."}
            }}}
        }}}
    }})
}
pub fn parse(raw: &str) -> Result<Vec<Question>, String> {
    let input: Input = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    validate(&input.questions)?;
    Ok(input.questions)
}
pub fn validate(questions: &[Question]) -> Result<(), String> {
    if !(1..=4).contains(&questions.len()) {
        return Err("questions must contain 1–4 questions".into());
    }
    let mut texts = HashSet::new();
    for q in questions {
        if q.question.is_empty() || q.question.len() > 2000 {
            return Err("question must contain 1–2000 bytes".into());
        }
        if !texts.insert(&q.question) {
            return Err("question text must be unique".into());
        }
        if !(1..=12).contains(&q.header.chars().count()) {
            return Err("header must contain 1–12 characters".into());
        }
        if !(2..=4).contains(&q.options.len()) {
            return Err("options must contain 2–4 options".into());
        }
        let mut labels = HashSet::new();
        for o in &q.options {
            if o.label.is_empty() || o.label.len() > 2000 {
                return Err("label must contain 1–2000 bytes".into());
            }
            if o.label == "Other" {
                return Err("Other is reserved for free text".into());
            }
            if !labels.insert(&o.label) {
                return Err("option labels must be unique".into());
            }
            if o.description.is_empty() || o.description.len() > 2000 {
                return Err("description must contain 1–2000 bytes".into());
            }
            if o.preview.as_ref().is_some_and(|p| p.len() > 8000) {
                return Err("preview must contain at most 8000 bytes".into());
            }
            if q.multi_select && o.preview.is_some() {
                return Err("preview is unavailable on multi-select questions".into());
            }
        }
    }
    Ok(())
}
pub fn format(questions: &[Question], asked: Asked) -> (&'static str, String) {
    match asked {
        Asked::Cancelled => ("cancelled", "cancelled — no answer".into()),
        Asked::NotInvited => ("error", NOT_INVITED.into()),
        Asked::NoInteractiveUser => (
            "error",
            "no interactive user — decide without asking, or end the turn with the question".into(),
        ),
        Asked::Answered(answers) => {
            let text = questions
                .iter()
                .zip(answers)
                .map(|(q, a)| {
                    let mut text = format!("{}: {}", q.header, a.chosen.join(", "));
                    if let Some(free) = a.free_text {
                        if !a.chosen.is_empty() {
                            text.push_str("; ");
                        }
                        text.push_str(&free);
                    }
                    text
                })
                .collect::<Vec<_>>()
                .join("\n");
            ("ok", text)
        }
    }
}
/// Same output contract as p1-workspace; kept guest-only to avoid host filesystem dependencies.
pub fn bound_output(text: &str) -> String {
    let total = text.len();
    let mut end = total;
    for (i, _) in text.match_indices('\n').take(2000).skip(1999) {
        end = (i + 1).min(end);
    }
    end = end.min(50_000);
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    if end == total {
        return text.into();
    }
    let mut out = text[..end].to_owned();
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&format!(
        "[output truncated: showing {end} of {total} bytes]"
    ));
    out
}

pub fn invalid(why: &str) -> String {
    format!("Invalid input for {NAME}: {why}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};
    fn valid() -> Value {
        json!({"questions":[{"question":"Choose storage", "header":"Storage", "options":[{"label":"SSD","description":"Working set"},{"label":"HDD","description":"Bulk data"}]}]})
    }
    #[test]
    fn nested_bounds_unknown_fields_and_injected_answers_are_rejected() {
        assert!(parse(&valid().to_string()).is_ok());
        let mut cases = vec![json!({"questions":[]})];
        for n in 0..20 {
            let mut v = valid();
            match n {
                0 => v["answers"] = json!(["SSD"]),
                1 => v["questions"][0]["answer"] = json!("SSD"),
                2 => v["questions"][0]["options"][0]["chosen"] = json!(true),
                3 => v["questions"][0]["question"] = json!(""),
                4 => v["questions"][0]["question"] = json!("é".repeat(1001)),
                5 => v["questions"][0]["header"] = json!(""),
                6 => v["questions"][0]["header"] = json!("h".repeat(13)),
                7 => v["questions"][0]["options"] = json!([]),
                8 => v["questions"][0]["options"][0]["label"] = json!(""),
                9 => v["questions"][0]["options"][0]["label"] = json!("Other"),
                10 => v["questions"][0]["options"][0]["label"] = json!("HDD"),
                11 => v["questions"][0]["options"][0]["description"] = json!(""),
                12 => v["questions"][0]["options"][0]["description"] = json!("d".repeat(2001)),
                13 => v["questions"][0]["options"][0]["preview"] = json!("p".repeat(8001)),
                14 => {
                    v["questions"][0]["multi_select"] = json!(true);
                    v["questions"][0]["options"][0]["preview"] = json!("");
                }
                15 => v["questions"] = json!(vec![v["questions"][0].clone(); 2]),
                16 => v["questions"] = json!(vec![v["questions"][0].clone(); 5]),
                17 => v["questions"][0]["multi_select"] = Value::Null,
                18 => v["questions"][0]["options"][0]["preview"] = Value::Null,
                19 => v["questions"][0]["options"][0]["label"] = json!("l".repeat(2001)),
                _ => unreachable!(),
            }
            cases.push(v);
        }
        for v in cases {
            assert!(parse(&v.to_string()).is_err(), "{v}");
        }
    }
}
