//! Session-local links between background observations and their initiating calls.
//! Start results expose the ids after work can already have reported progress.

use std::collections::HashMap;

use p1_contracts::frontend::WorkflowProgress;
use p1_contracts::{ToolIdentity, ToolResultItem, ToolStatus};

use crate::sink::Update;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CardStatus {
    Running,
    Completed,
    Failed,
}

#[derive(Clone, Copy)]
pub(crate) enum StartKind {
    Workflow,
    Worker,
}

impl StartKind {
    pub(crate) fn for_tool(name: &str, identity: Option<&ToolIdentity>) -> Option<Self> {
        match identity.map(|identity| identity.implementation.as_str()) {
            Some("p1/workflow-start" | "p1-tool-workflow") => Some(Self::Workflow),
            Some(
                "p1/worker-start" | "p1-tool-delegate" | "p1/finder" | "p1/task" | "p1/librarian"
                | "p1/oracle",
            ) => Some(Self::Worker),
            // A bare sink can describe calls without an assembled tool snapshot.
            None if name == "workflow_start" => Some(Self::Workflow),
            None if name == "worker_start" => Some(Self::Worker),
            _ => None,
        }
    }
}

#[derive(Default)]
pub(crate) struct WorkflowCards {
    calls: HashMap<String, Option<StartKind>>,
    runs: HashMap<String, Run>,
    workers: HashMap<String, Card>,
    worker_notes: Vec<(String, String)>,
}

#[derive(Default)]
struct Run {
    card: Option<Card>,
    pending: Vec<WorkflowProgress>,
}

struct Card {
    id: String,
    text: String,
}

impl Card {
    fn append(&mut self, line: &str) {
        if !line.is_empty() {
            self.text.push('\n');
            self.text.push_str(line);
        }
    }

    fn update(&self, status: Option<CardStatus>) -> Update {
        Update::ToolProgress {
            id: self.id.clone(),
            text: self.text.clone(),
            status,
        }
    }
}

impl WorkflowCards {
    pub(crate) fn started(&mut self, id: &str, kind: Option<StartKind>) {
        // Providers can reuse a call id. Never let an old worker's late note
        // rewrite the new call's card.
        self.workers.retain(|_, card| card.id != id);
        for run in self.runs.values_mut() {
            if run.card.as_ref().is_some_and(|card| card.id == id) {
                run.card = None;
            }
        }
        self.calls.insert(id.to_string(), kind);
    }

    /// Whether the call started; a refused or unknown call finishes without starting.
    pub(crate) fn knows(&self, id: &str) -> bool {
        self.calls.contains_key(id)
    }

    pub(crate) fn progress(&mut self, progress: &WorkflowProgress) -> Vec<Update> {
        let run = self.runs.entry(progress.run.clone()).or_default();
        match &mut run.card {
            Some(card) => vec![workflow_update(card, progress)],
            None => {
                run.pending.push(progress.clone());
                Vec::new()
            }
        }
    }

    pub(crate) fn worker_ended(&mut self, worker: &str, note: &str) -> Vec<Update> {
        if let Some(card) = self.workers.get_mut(worker) {
            card.append(note);
            // The start tool already completed. A note changes only its content.
            vec![card.update(None)]
        } else if self.calls.is_empty() {
            vec![Update::Message(note.to_string())]
        } else {
            self.worker_notes
                .push((worker.to_string(), note.to_string()));
            Vec::new()
        }
    }

    pub(crate) fn finished(&mut self, result: ToolResultItem) -> Vec<Update> {
        let kind = self.calls.remove(&result.call_id).flatten();
        let mut updates = Vec::new();
        let workflow = matches!(kind, Some(StartKind::Workflow))
            .then(|| result.content.strip_prefix("Started workflow "))
            .flatten()
            .and_then(|rest| rest.split([' ', ',', '.']).next())
            .filter(|id| !id.is_empty() && result.status == ToolStatus::Ok);
        if let Some(id) = workflow {
            let run = self.runs.entry(id.to_string()).or_default();
            let mut card = Card {
                id: result.call_id,
                text: result.content.clone(),
            };
            updates.push(card.update(Some(CardStatus::Running)));
            for progress in run.pending.drain(..) {
                updates.push(workflow_update(&mut card, &progress));
            }
            run.card = Some(card);
        } else {
            let worker = matches!(kind, Some(StartKind::Worker))
                .then(|| result.content.strip_prefix("Started worker "))
                .flatten()
                .and_then(|rest| rest.split_whitespace().next())
                .filter(|_| result.status == ToolStatus::Ok);
            if let Some(worker) = worker {
                let mut card = Card {
                    id: result.call_id.clone(),
                    text: result.content.clone(),
                };
                updates.push(finished_update(&result));
                for (id, note) in std::mem::take(&mut self.worker_notes) {
                    if id == worker {
                        card.append(&note);
                        updates.push(card.update(None));
                    } else {
                        self.worker_notes.push((id, note));
                    }
                }
                self.workers.insert(worker.to_string(), card);
            } else {
                updates.push(finished_update(&result));
            }
        }
        if self.calls.is_empty() {
            updates.extend(self.unlinked_notes());
        }
        updates
    }

    pub(crate) fn turn_ended(&mut self) -> Vec<Update> {
        self.calls.clear();
        self.unlinked_notes()
    }

    fn unlinked_notes(&mut self) -> Vec<Update> {
        self.worker_notes
            .drain(..)
            .map(|(_, note)| Update::Message(note))
            .collect()
    }
}

fn workflow_update(card: &mut Card, progress: &WorkflowProgress) -> Update {
    card.append(&progress.line);
    let status = match progress.outcome.as_deref() {
        None => CardStatus::Running,
        Some("completed") => CardStatus::Completed,
        Some(_) => CardStatus::Failed,
    };
    card.update(Some(status))
}

fn finished_update(result: &ToolResultItem) -> Update {
    Update::ToolFinished {
        id: result.call_id.clone(),
        succeeded: result.status == ToolStatus::Ok,
        text: result.content.clone(),
    }
}
