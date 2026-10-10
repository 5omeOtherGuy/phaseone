//! Session-scoped form elicitation, behind the neutral question hook.
use crate::driver::io::Peer;
use p1_contracts::CancellationToken;
use p1_contracts::frontend::{Question, QuestionAnswer, QuestionOutcome};
use serde_json::{Value, json};
use std::sync::{
    Mutex,
    atomic::{AtomicBool, Ordering},
};

#[derive(Default)]
pub(crate) struct Questions {
    form: AtomicBool,
    session: Mutex<Option<(Peer, String)>>,
}

impl Questions {
    pub(crate) fn supported(&self) -> bool {
        self.form.load(Ordering::Relaxed)
    }

    pub(crate) fn initialize(&self, params: &Value) {
        self.form.store(
            params
                .pointer("/clientCapabilities/elicitation/form")
                .is_some_and(Value::is_object),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn opened(&self, peer: Peer, session: String) {
        *self.session.lock().unwrap() = Some((peer, session));
    }

    pub(crate) async fn ask(
        &self,
        worker: Option<&str>,
        questions: Vec<Question>,
        cancel: CancellationToken,
        connection: &CancellationToken,
    ) -> QuestionOutcome {
        if !self.form.load(Ordering::Relaxed) {
            return QuestionOutcome::NoInteractiveUser;
        }
        let Some((peer, session)) = self.session.lock().unwrap().clone() else {
            return QuestionOutcome::NoInteractiveUser;
        };
        if cancel.is_cancelled() || connection.is_cancelled() {
            return QuestionOutcome::Cancelled;
        }
        let asked = peer.request("elicitation/create", form(&session, worker, &questions));
        tokio::select! { biased;
            _ = cancel.cancelled() => QuestionOutcome::Cancelled,
            _ = connection.cancelled() => QuestionOutcome::Cancelled,
            result = asked => result.ok().and_then(|v| answers(&questions, &v)).map(QuestionOutcome::Answered).unwrap_or(QuestionOutcome::Cancelled),
        }
    }
}

fn form(session: &str, worker: Option<&str>, questions: &[Question]) -> Value {
    let mut properties = serde_json::Map::new();
    for (i, q) in questions.iter().enumerate() {
        let title = worker
            .map(|w| format!("[{w}] {}", q.header))
            .unwrap_or_else(|| q.header.clone());
        let options: Vec<_> = q
            .options
            .iter()
            .map(|o| {
                let description = o
                    .preview
                    .as_ref()
                    .map(|p| format!("{}\n\n{p}", o.description))
                    .unwrap_or_else(|| o.description.clone());
                json!({"const":o.label,"title":o.label,"description":description})
            })
            .collect();
        let mut choice = if q.multi_select {
            json!({"type":"array","items":{"anyOf":options},"minItems":1,"maxItems":q.options.len()})
        } else {
            json!({"type":"string","oneOf":options})
        };
        choice["title"] = json!(title);
        choice["description"] = json!(q.question);
        properties.insert(format!("q{i}"), choice);
        properties.insert(format!("q{i}_text"), json!({"type":"string","title":format!("{title} — Other"),"description":"Free text instead of, or in addition to, the choices","minLength":1}));
    }
    json!({"sessionId":session,"mode":"form","message":questions.iter().map(|q| q.question.as_str()).collect::<Vec<_>>().join("\n"),"requestedSchema":{"type":"object","title":worker.map(|w| format!("[{w}] Questions")).unwrap_or_else(|| "Questions".into()),"properties":properties}})
}

fn answers(questions: &[Question], response: &Value) -> Option<Vec<QuestionAnswer>> {
    if response["action"] != "accept" {
        return None;
    }
    let content = response["content"].as_object()?;
    questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let chosen = match content.get(&format!("q{i}")) {
                None => Vec::new(),
                Some(Value::String(s)) if !q.multi_select => vec![s.clone()],
                Some(Value::Array(items)) if q.multi_select => items
                    .iter()
                    .map(|v| v.as_str().map(str::to_owned))
                    .collect::<Option<Vec<_>>>()?,
                _ => return None,
            };
            if chosen
                .iter()
                .any(|c| !q.options.iter().any(|o| &o.label == c))
                || chosen
                    .iter()
                    .enumerate()
                    .any(|(i, c)| chosen[..i].contains(c))
            {
                return None;
            }
            let free_text = match content.get(&format!("q{i}_text")) {
                None => None,
                Some(Value::String(s)) if !s.trim().is_empty() => Some(s.clone()),
                _ => return None,
            };
            if chosen.is_empty() && free_text.is_none() {
                return None;
            }
            // Return choices in the source order, just like the host's line asker.
            Some(QuestionAnswer {
                chosen: q
                    .options
                    .iter()
                    .filter(|o| chosen.contains(&o.label))
                    .map(|o| o.label.clone())
                    .collect(),
                free_text,
            })
        })
        .collect()
}
