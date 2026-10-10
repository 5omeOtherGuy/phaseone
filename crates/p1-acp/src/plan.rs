//! Agent-authored snapshots take precedence over the workflow-only fallback.

use p1_contracts::frontend::WorkflowStep;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlanStatus {
    Pending,
    Active,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanEntry {
    pub content: String,
    pub status: PlanStatus,
    pub priority: p1_contracts::plan::PlanPriority,
}

#[derive(Default)]
pub(crate) struct PlanState {
    authored: Option<Vec<PlanEntry>>,
    // Run start order, then each run's agent() call order. Run ids are opaque:
    // lexicographic order would put wf10 before wf2.
    runs: Vec<Run>,
}

struct Run {
    id: String,
    steps: BTreeMap<u32, Step>,
}

struct Step {
    content: String,
    status: String,
}

impl PlanState {
    pub(crate) fn replace(&mut self, entries: &[p1_contracts::plan::PlanEntry]) -> Vec<PlanEntry> {
        use p1_contracts::plan::PlanStatus as Status;
        let entries: Vec<_> = entries
            .iter()
            .map(|entry| PlanEntry {
                content: entry.content.clone(),
                priority: entry.priority,
                status: match entry.status {
                    Status::Pending => PlanStatus::Pending,
                    Status::InProgress => PlanStatus::Active,
                    Status::Completed => PlanStatus::Done,
                },
            })
            .collect();
        self.authored = Some(entries.clone());
        entries
    }

    pub(crate) fn begin(&mut self, id: &str) {
        self.run(id);
    }

    fn run(&mut self, id: &str) -> &mut Run {
        let index = match self.runs.iter().position(|run| run.id == id) {
            Some(index) => index,
            None => {
                self.runs.push(Run {
                    id: id.to_string(),
                    steps: BTreeMap::new(),
                });
                self.runs.len() - 1
            }
        };
        &mut self.runs[index]
    }

    pub(crate) fn observe(&mut self, event: &WorkflowStep) -> Vec<PlanEntry> {
        if let Some(entries) = &self.authored {
            return entries.clone();
        }
        let content = event.label.as_ref().or(event.task.as_ref());
        let step = self
            .run(&event.run)
            .steps
            .entry(event.ordinal)
            .or_insert_with(|| Step {
                content: content.unwrap_or(&event.call).clone(),
                status: event.status.clone(),
            });
        if let Some(content) = content {
            step.content.clone_from(content);
        }
        step.status.clone_from(&event.status);
        self.runs
            .iter()
            .flat_map(|run| {
                run.steps.iter().map(|(ordinal, step)| {
                    let (status, outcome) = match step.status.as_str() {
                        "running" => (PlanStatus::Active, String::new()),
                        "done" => (PlanStatus::Done, String::new()),
                        // The goal remains unfinished, not successfully completed.
                        // ACP cannot express these outcomes; name the loss in text.
                        other => (PlanStatus::Pending, format!(" ({other})")),
                    };
                    PlanEntry {
                        content: format!("{}/{ordinal}: {}{outcome}", run.id, step.content),
                        status,
                        priority: p1_contracts::plan::PlanPriority::Medium,
                    }
                })
            })
            .collect()
    }
}
