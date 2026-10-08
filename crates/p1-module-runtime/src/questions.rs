//! Host-owned user questions (ADR-0116). Guests can ask, never supply an answer.
use crate::capabilities::{CallState, check_arity};
use crate::loader::interface_import;
use p1_contracts::{BoxFuture, CancellationToken};
use std::collections::HashSet;
use wasmtime::bail;
use wasmtime::component::{Linker, Val};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
    pub preview: Option<String>,
}
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Question {
    pub question: String,
    pub header: String,
    pub options: Vec<QuestionOption>,
    pub multi_select: bool,
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
    /// No user input of this session invited questions (ADR-0135); nothing was shown.
    NotInvited,
}
/// The refusal the guest receives as `question-error::invalid` for [`Asked::NotInvited`]
/// (ADR-0135); the guest shows it verbatim. The interface keeps its shape.
pub const NOT_INVITED: &str = "ask_user_question is only available after the user asks you to ask questions; decide yourself and continue";
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum QuestionError {
    #[error("{0}")]
    Invalid(String),
}
pub trait UserQuestionsService: Send + Sync {
    fn ask(&self, questions: Vec<Question>, cancel: CancellationToken) -> BoxFuture<'_, Asked>;
}

pub fn validate(questions: &[Question]) -> Result<(), QuestionError> {
    let invalid = |why: &str| QuestionError::Invalid(why.into());
    if !(1..=4).contains(&questions.len()) {
        return Err(invalid("questions must contain 1–4 questions"));
    }
    let mut texts = HashSet::new();
    for q in questions {
        if q.question.is_empty() || q.question.len() > 2000 {
            return Err(invalid("question must contain 1–2000 bytes"));
        }
        if !texts.insert(&q.question) {
            return Err(invalid("question text must be unique"));
        }
        if !(1..=12).contains(&q.header.chars().count()) {
            return Err(invalid("header must contain 1–12 characters"));
        }
        if !(2..=4).contains(&q.options.len()) {
            return Err(invalid("options must contain 2–4 options"));
        }
        let mut labels = HashSet::new();
        for o in &q.options {
            if o.label.is_empty() || o.label.len() > 2000 {
                return Err(invalid("label must contain 1–2000 bytes"));
            }
            if o.label == "Other" {
                return Err(invalid("Other is reserved for free text"));
            }
            if !labels.insert(&o.label) {
                return Err(invalid("option labels must be unique"));
            }
            if o.description.is_empty() || o.description.len() > 2000 {
                return Err(invalid("description must contain 1–2000 bytes"));
            }
            if o.preview.as_ref().is_some_and(|p| p.len() > 8000) {
                return Err(invalid("preview must contain at most 8000 bytes"));
            }
            if q.multi_select && o.preview.is_some() {
                return Err(invalid("preview is unavailable on multi-select questions"));
            }
        }
    }
    Ok(())
}

pub(crate) fn link_user_questions(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut interface = linker.instance(&interface_import("user-questions"))?;
    interface.func_new_async("ask", |store, _ty, params, results| {
        let request = check_arity("user-questions.ask", params, results, 1, 1)
            .and_then(|()| decode(&params[0]));
        let service = store.data().user_questions.clone();
        let cancel = store.data().cancel.clone();
        let deadline = store.data().question_deadline.clone();
        Box::new(async move {
            let questions = request?;
            results[0] = match validate(&questions) {
                Err(QuestionError::Invalid(why)) => invalid_val(why),
                Ok(()) => {
                    let Some(service) = service else {
                        bail!("user-questions.ask called without a service");
                    };
                    let _waiting = deadline.as_ref().map(|deadline| deadline.pause());
                    let asked = tokio::select! { biased;
                        _ = cancel.cancelled() => Asked::Cancelled,
                        asked = service.ask(questions, cancel.clone()) => asked,
                    };
                    asked_result(asked)
                }
            };
            Ok(())
        })
    })
}
fn field<'a>(value: &'a Val, name: &str) -> wasmtime::Result<&'a Val> {
    let Val::Record(fields) = value else {
        bail!("user-questions: expected record");
    };
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
        .ok_or_else(|| wasmtime::format_err!("user-questions: missing {name}"))
}
fn text(value: &Val) -> wasmtime::Result<String> {
    let Val::String(s) = value else {
        bail!("user-questions: expected string");
    };
    Ok(s.clone())
}
fn list(value: &Val) -> wasmtime::Result<&[Val]> {
    let Val::List(items) = value else {
        bail!("user-questions: expected list");
    };
    Ok(items)
}
fn decode(value: &Val) -> wasmtime::Result<Vec<Question>> {
    list(value)?
        .iter()
        .map(|q| {
            let Val::Bool(multi_select) = field(q, "multi-select")? else {
                bail!("user-questions: expected boolean");
            };
            let options = list(field(q, "options")?)?
                .iter()
                .map(|o| {
                    let Val::Option(preview) = field(o, "preview")? else {
                        bail!("user-questions: expected option");
                    };
                    Ok(QuestionOption {
                        label: text(field(o, "label")?)?,
                        description: text(field(o, "description")?)?,
                        preview: preview.as_deref().map(text).transpose()?,
                    })
                })
                .collect::<wasmtime::Result<Vec<_>>>()?;
            Ok(Question {
                question: text(field(q, "question")?)?,
                header: text(field(q, "header")?)?,
                options,
                multi_select: *multi_select,
            })
        })
        .collect()
}
fn invalid_val(why: String) -> Val {
    Val::Result(Err(Some(Box::new(Val::Variant(
        "invalid".into(),
        Some(Box::new(Val::String(why))),
    )))))
}
/// The interface's `result<asked, question-error>`; [`Asked::NotInvited`] travels as
/// `invalid` so the interface and every component's type stay unchanged (ADR-0135).
fn asked_result(asked: Asked) -> Val {
    let asked = match asked {
        Asked::NotInvited => return invalid_val(NOT_INVITED.into()),
        Asked::Cancelled => Val::Variant("cancelled".into(), None),
        Asked::NoInteractiveUser => Val::Variant("no-interactive-user".into(), None),
        Asked::Answered(answers) => Val::Variant(
            "answered".into(),
            Some(Box::new(Val::List(
                answers
                    .into_iter()
                    .map(|answer| {
                        Val::Record(vec![
                            (
                                "chosen".into(),
                                Val::List(answer.chosen.into_iter().map(Val::String).collect()),
                            ),
                            (
                                "free-text".into(),
                                Val::Option(answer.free_text.map(|t| Box::new(Val::String(t)))),
                            ),
                        ])
                    })
                    .collect(),
            ))),
        ),
    };
    Val::Result(Ok(Some(Box::new(asked))))
}
#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;
