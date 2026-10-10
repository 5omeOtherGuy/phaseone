//! The texts a run's report is read as: the one-line summary `workflow_status` answers for an
//! ended run and the full report `workflow_result` and `p1 workflow run` print. Pure over
//! [`RunReport`], so every reader (the native tools, the CLI, the member components' parity
//! checks) renders the same text from the same report.

use crate::{RunOutcome, RunReport, StepLine, StepStatus};

/// The report's summary line: outcome and step counts.
pub fn report_line(report: &RunReport) -> String {
    let outcome = match report.outcome {
        RunOutcome::Completed => "completed",
        RunOutcome::CompletedWithIssues => "completed with issues",
        RunOutcome::Failed => "failed",
        RunOutcome::Cancelled => "cancelled",
    };
    let c = &report.counts;
    format!(
        "Workflow {}: {outcome} — {} steps ({} replayed): {} done, {} blocked, {} failed, {} cancelled; {} not verified; {} capped; {} invalid output; {} fell back",
        report.id.0,
        c.steps,
        c.replayed,
        c.done,
        c.blocked,
        c.failed,
        c.cancelled,
        c.not_verified,
        c.capped,
        c.invalid_output,
        c.fell_back
    )
}

fn render_step(step: &StepLine) -> String {
    let status = match step.status {
        StepStatus::Done => "done",
        StepStatus::Blocked => "blocked",
        StepStatus::Failed => "failed",
        StepStatus::Cancelled => "cancelled",
    };
    // The model part is the chain the step walked (ADR-0054 item 4).
    let mut line = format!(
        "  {} {} → {}",
        step.label.as_deref().unwrap_or(&step.call.0),
        step.role,
        step.model_chain()
    );
    if let Some(worker) = &step.worker {
        line.push_str(&format!(" [{worker}]"));
    }
    line.push_str(&format!(" {status} — schema {}", step.schema));
    if let Some(evidence) = &step.evidence {
        line.push_str(&format!("; {evidence}"));
    }
    if step.replayed {
        line.push_str("; replayed");
    }
    if step.attempts > 1 {
        line.push_str(&format!("; attempts {}", step.attempts));
    }
    if let Some(error) = &step.error {
        line.push_str(&format!("; {error}"));
    }
    line
}

/// The full report: the summary line, any error, the first 200 steps, the value (cut at
/// 16 KiB) and the run directory.
pub fn render_report(report: &RunReport) -> String {
    let mut text = report_line(report);
    if let Some(error) = &report.error {
        text.push_str(&format!("\nerror: {error}"));
    }
    text.push_str("\nsteps:");
    for step in report.steps.iter().take(200) {
        text.push('\n');
        text.push_str(&render_step(step));
    }
    if report.steps.len() > 200 {
        text.push_str(&format!(
            "\n  … {} more in result.json",
            report.steps.len() - 200
        ));
    }
    let value = serde_json::to_string_pretty(&p1_json_order::canonicalize(report.value.clone()))
        .expect("a serde_json::Value always serializes");
    text.push_str("\nresult:\n");
    if value.len() > 16 * 1024 {
        let mut end = 16 * 1024;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        text.push_str(&value[..end]);
        text.push_str(&format!(
            "… (truncated; full value in {}/result.json)",
            report.run_dir.display()
        ));
    } else {
        text.push_str(&value);
    }
    text.push_str(&format!("\nrun dir: {}", report.run_dir.display()));
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::RunStatus;

    /// The ended status and report text the `p1/workflow-result` member's own test pins
    /// (`modules/p1-module-workflow-start/src/workflow.rs`): the same report renders the same
    /// text here and in the component.
    const ENDED: &str = r#"{"Ended":{"id":"wf1","outcome":"completed_with_issues","value":{"b":1,"a":[true]},
        "counts":{"steps":2,"replayed":0,"done":1,"blocked":0,"failed":1,"cancelled":0,"not_verified":1,
        "capped":0,"invalid_output":0,"fell_back":1},
        "steps":[{"call":"c-1","ordinal":1,"label":"plan","role":"worker","model":"e/p","worker":"w1",
          "status":"done","schema":"ok","evidence":"not verified","attempts":2,"replayed":false,"error":null,
          "models":[{"model":"e/a","moved_on":"route_failed"},{"model":"e/b","moved_on":null}]},
          {"call":"c-2","ordinal":2,"label":null,"role":"judge","model":"e/j","worker":null,
          "status":"failed","schema":"none","evidence":null,"attempts":1,"replayed":true,"error":"route: down",
          "models":[]}],
        "error":null,"run_dir":"/runs/wf1"}}"#;

    #[test]
    fn renders_the_report_the_component_renders() {
        let Ok(RunStatus::Ended(report)) = serde_json::from_str::<RunStatus>(ENDED) else {
            panic!("the ended status parses");
        };
        assert_eq!(
            render_report(&report),
            "Workflow wf1: completed with issues — 2 steps (0 replayed): 1 done, 0 blocked, 1 failed, 0 cancelled; 1 not verified; 0 capped; 0 invalid output; 1 fell back\n\
             steps:\n  plan worker → e/a route failed → e/b [w1] done — schema ok; not verified; attempts 2\n  \
             c-2 judge → e/j failed — schema none; replayed; route: down\n\
             result:\n{\n  \"a\": [\n    true\n  ],\n  \"b\": 1\n}\nrun dir: /runs/wf1"
        );
    }
}
