//! Translation from the module-runtime question service to the neutral front-end port.
use p1_contracts::frontend::{FrontEndPort, Question, QuestionOption, QuestionOutcome};
use p1_contracts::{BoxFuture, CancellationToken};
use p1_module_runtime::questions::{Answer, Asked};
use std::sync::Arc;

pub(super) struct PortQuestionAsker(pub Arc<dyn FrontEndPort>);

impl crate::questions::QuestionAsker for PortQuestionAsker {
    fn is_interactive(&self) -> bool {
        self.0.questions_supported()
    }

    fn ask<'a>(
        &'a self,
        worker: Option<&'a str>,
        questions: Vec<p1_module_runtime::questions::Question>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Asked> {
        Box::pin(async move {
            let questions = questions
                .into_iter()
                .map(|q| Question {
                    question: q.question,
                    header: q.header,
                    multi_select: q.multi_select,
                    options: q
                        .options
                        .into_iter()
                        .map(|o| QuestionOption {
                            label: o.label,
                            description: o.description,
                            preview: o.preview,
                        })
                        .collect(),
                })
                .collect();
            match self.0.ask_questions(worker, questions, cancel).await {
                QuestionOutcome::Answered(answers) => Asked::Answered(
                    answers
                        .into_iter()
                        .map(|a| Answer {
                            chosen: a.chosen,
                            free_text: a.free_text,
                        })
                        .collect(),
                ),
                QuestionOutcome::Cancelled => Asked::Cancelled,
                QuestionOutcome::NoInteractiveUser => Asked::NoInteractiveUser,
            }
        })
    }
}
