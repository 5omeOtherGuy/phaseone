//! Native test adapter; the shipped tool is the p1/ask-user-question component.
#![forbid(unsafe_code)]
use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome, ToolStatus,
};
use p1_module_runtime::questions::{self as host, UserQuestionsService};
use p1_question_guest as guest;
use std::sync::Arc;

pub struct QuestionTool {
    service: Arc<dyn UserQuestionsService>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}
impl QuestionTool {
    pub fn new(service: Arc<dyn UserQuestionsService>) -> Self {
        Self {
            service,
            declaration: ToolDeclaration {
                name: guest::NAME.into(),
                description: guest::DESCRIPTION.into(),
                kind: DeclarationKind::Function {
                    input_schema: guest::input_schema(),
                },
            },
            identity: ToolIdentity {
                implementation: "p1/ask-user-question".into(),
                variant: "claude".into(),
            },
        }
    }
}
impl Tool for QuestionTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _: &ToolCall) -> Effect {
        Effect::ReadOnly
    }
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let ToolInput::Json(raw) = &call.input else {
                return ToolOutcome::error(guest::invalid("expected JSON input"));
            };
            let questions = match guest::parse(raw) {
                Ok(q) => q,
                Err(e) => return ToolOutcome::error(guest::invalid(&e)),
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
            if let Err(e) = host::validate(&request) {
                return ToolOutcome::error(guest::invalid(&e.to_string()));
            }
            let asked = tokio::select! { biased;
                _ = context.cancel.cancelled() => host::Asked::Cancelled,
                asked = self.service.ask(request, context.cancel.clone()) => asked,
            };
            let asked = match asked {
                host::Asked::Cancelled => guest::Asked::Cancelled,
                host::Asked::NoInteractiveUser => guest::Asked::NoInteractiveUser,
                host::Asked::Answered(a) => guest::Asked::Answered(
                    a.into_iter()
                        .map(|a| guest::Answer {
                            chosen: a.chosen,
                            free_text: a.free_text,
                        })
                        .collect(),
                ),
            };
            let (status, text) = guest::format(&questions, asked);
            ToolOutcome {
                status: match status {
                    "ok" => ToolStatus::Ok,
                    "cancelled" => ToolStatus::Cancelled,
                    _ => ToolStatus::Error,
                },
                content: p1_workspace::bound_output(&text, 50_000, 2000),
            }
        })
    }
}
