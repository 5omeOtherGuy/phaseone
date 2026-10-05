//! Front-end asking and serialization with authorization (ADR-0116).
use crate::{LineSource, SharedWriter};
use p1_contracts::{BoxFuture, CancellationToken};
use p1_module_runtime::questions::{Answer, Asked, Question, UserQuestionsService, validate};
use std::sync::Arc;

/// Assembly-local identities; weak keys cannot label a later mask at a reused address.
pub(crate) type WorkerLabels = Arc<
    std::sync::Mutex<
        std::collections::HashMap<usize, (std::sync::Weak<p1_redact::MaskCounter>, String)>,
    >,
>;

pub trait QuestionAsker: Send + Sync {
    fn ask<'a>(
        &'a self,
        worker: Option<&'a str>,
        questions: Vec<Question>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Asked>;
}
pub struct QuestionBridge {
    pub(crate) gate: Arc<tokio::sync::Mutex<()>>,
    asker: Option<Arc<dyn QuestionAsker>>,
    worker: Option<String>,
    pub(crate) wake: Arc<tokio::sync::Notify>,
}
impl QuestionBridge {
    pub fn new(asker: Option<Arc<dyn QuestionAsker>>, gate: Arc<tokio::sync::Mutex<()>>) -> Self {
        Self {
            asker,
            gate,
            worker: None,
            wake: Arc::new(tokio::sync::Notify::new()),
        }
    }
    pub fn headless() -> Self {
        Self::new(None, Arc::new(tokio::sync::Mutex::new(())))
    }
    pub fn for_worker(&self, worker: &str) -> Self {
        Self {
            gate: self.gate.clone(),
            asker: self.asker.clone(),
            worker: Some(worker.into()),
            wake: self.wake.clone(),
        }
    }
}
impl UserQuestionsService for QuestionBridge {
    fn ask(&self, questions: Vec<Question>, cancel: CancellationToken) -> BoxFuture<'_, Asked> {
        Box::pin(async move {
            if validate(&questions).is_err() {
                return Asked::Cancelled;
            }
            if cancel.is_cancelled() {
                return Asked::Cancelled;
            }
            let Some(asker) = &self.asker else {
                return Asked::NoInteractiveUser;
            };
            tokio::select! { biased;
                _ = cancel.cancelled() => Asked::Cancelled,
                asked = async {
                    let _guard = self.gate.lock().await;
                    self.wake.notify_one();
                    if cancel.is_cancelled() { return Asked::Cancelled; }
                    asker.ask(self.worker.as_deref(), questions, cancel.clone()).await
                } => asked,
            }
        })
    }
}
pub struct LineQuestionAsker {
    pub lines: Arc<dyn LineSource>,
    pub stderr: SharedWriter,
}
impl QuestionAsker for LineQuestionAsker {
    fn ask<'a>(
        &'a self,
        worker: Option<&'a str>,
        questions: Vec<Question>,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Asked> {
        Box::pin(async move {
            let mut answers = Vec::new();
            for q in &questions {
                {
                    let mut writer = self.stderr.lock().unwrap();
                    let prefix = worker.map(|id| format!("[{id}] ")).unwrap_or_default();
                    let _ = writeln!(writer, "{prefix}{}: {}", q.header, q.question);
                    for (i, o) in q.options.iter().enumerate() {
                        let _ = writeln!(writer, "{}: {} — {}", i + 1, o.label, o.description);
                        if let Some(preview) = &o.preview {
                            let _ = writeln!(writer, "{preview}");
                        }
                    }
                    let _ = writeln!(
                        writer,
                        "Enter option number{} or free text; /text TEXT for numeric text; /cancel dismisses.",
                        if q.multi_select {
                            "s separated by commas"
                        } else {
                            ""
                        }
                    );
                    let _ = writer.flush();
                }
                loop {
                    let Some(line) = self.lines.next_line().await else {
                        return Asked::NoInteractiveUser;
                    };
                    if line.trim() == "/cancel" {
                        return Asked::Cancelled;
                    }
                    if let Some(answer) = parse_line(q, &line) {
                        answers.push(answer);
                        break;
                    }
                }
            }
            Asked::Answered(answers)
        })
    }
}
pub fn parse_line(q: &Question, line: &str) -> Option<Answer> {
    let line = line.trim();
    if line.is_empty() {
        return None;
    }
    if let Some(text) = line.strip_prefix("/text ") {
        return (!text.trim().is_empty()).then(|| Answer {
            chosen: Vec::new(),
            free_text: Some(text.into()),
        });
    }
    let numbers = line
        .split(',')
        .map(|p| p.trim().parse::<usize>())
        .collect::<Result<Vec<_>, _>>();
    if let Ok(numbers) = numbers {
        if (!q.multi_select && numbers.len() != 1)
            || numbers.iter().any(|&n| n == 0 || n > q.options.len())
        {
            return None;
        }
        return Some(Answer {
            chosen: q
                .options
                .iter()
                .enumerate()
                .filter(|(i, _)| numbers.contains(&(i + 1)))
                .map(|(_, o)| o.label.clone())
                .collect(),
            free_text: None,
        });
    }
    Some(Answer {
        chosen: Vec::new(),
        free_text: Some(line.into()),
    })
}

pub struct TuiQuestionAsker {
    sink: Arc<p1_tui::runtime::TuiSink>,
}
impl TuiQuestionAsker {
    pub fn new(sink: Arc<p1_tui::runtime::TuiSink>) -> Self {
        Self { sink }
    }
}
impl QuestionAsker for TuiQuestionAsker {
    fn ask<'a>(
        &'a self,
        worker: Option<&'a str>,
        questions: Vec<Question>,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Asked> {
        Box::pin(async move {
            let (reply, receive) = tokio::sync::oneshot::channel();
            let view = p1_tui::render::permission::QuestionView::new(
                worker.map(str::to_owned),
                questions
                    .iter()
                    .map(|q| p1_tui::render::permission::UserQuestion {
                        question: q.question.clone(),
                        header: q.header.clone(),
                        multi_select: q.multi_select,
                        options: q
                            .options
                            .iter()
                            .map(|o| (o.label.clone(), o.description.clone(), o.preview.clone()))
                            .collect(),
                    })
                    .collect(),
            );
            if !self.sink.questions(p1_tui::runtime::QuestionRequest {
                view,
                reply,
                cancel,
            }) {
                return Asked::NoInteractiveUser;
            }
            match receive.await {
                Ok(Some(answers)) => Asked::Answered(
                    answers
                        .into_iter()
                        .map(|(chosen, free_text)| Answer { chosen, free_text })
                        .collect(),
                ),
                Ok(None) => Asked::Cancelled,
                Err(_) => Asked::NoInteractiveUser,
            }
        })
    }
}

#[cfg(test)]
#[path = "questions_tests.rs"]
mod tests;
