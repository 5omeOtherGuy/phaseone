//! Background job inspection over the host-owned registry.
#![forbid(unsafe_code)]
use bindings::p1::module::{process_jobs as host, types::DeclarationKind};
use p1_bindings_tool::generated::{self as bindings, *};
use p1_shell_job_guest as guest;
struct ShellJob;
fn input(call: &str) -> Result<guest::Input, String> {
    let value: serde_json::Value = serde_json::from_str(call).map_err(|e| e.to_string())?;
    if value["input"]["kind"] != "json" {
        return Err("expected JSON input".into());
    }
    guest::parse(value["input"]["raw"].as_str().ok_or("missing input")?)
}
impl Guest for ShellJob {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: guest::NAME.into(),
            description: guest::DESCRIPTION.into(),
            kind: DeclarationKind::Function(guest::input_schema().to_string()),
        }
    }
    fn effect(call: ToolCall) -> CallEffect {
        if input(&call).is_ok_and(|i| i.action == guest::Action::Status) {
            CallEffect::ReadOnly
        } else {
            CallEffect::Executes
        }
    }
    fn describe(call: ToolCall) -> CallDescription {
        let input = input(&call).ok();
        serde_json::json!({"verb":if input.as_ref().is_some_and(|i| i.action == guest::Action::Status) {"check"} else {"cancel"},"target":input.map(|i| i.job_id),"destructive":false}).to_string()
    }
    fn describe_result(_: ToolCall, result: HistoryItem) -> ResultDescription {
        let v: serde_json::Value = serde_json::from_str(&result).unwrap_or_default();
        serde_json::json!({"summary":v["content"].as_str().unwrap_or("").lines().next().unwrap_or("")}).to_string()
    }
    fn execute(call: ToolCall) -> ToolOutcome {
        let input = match input(&call) {
            Ok(i) => i,
            Err(e) => return outcome("error", &format!("Invalid input for shell_job: {e}")),
        };
        let result = match input.action {
            guest::Action::Status => host::status(&input.job_id),
            guest::Action::Cancel => host::cancel(&input.job_id),
        };
        match result {
            Err(host::JobError::UnknownJob) => outcome("error", "unknown-job"),
            Err(host::JobError::StartFailed(e)) => outcome("error", &e),
            Ok(host::JobState::Running(progress)) => outcome(
                "ok",
                &format!(
                    "Job {} running: {} ms, {} output bytes",
                    input.job_id, progress.elapsed_ms, progress.output_bytes
                ),
            ),
            Ok(host::JobState::Ended(end)) => outcome(
                "ok",
                &format!(
                    "Job {} ended: {:?}; {} ms; output handle {}; read with read_output",
                    input.job_id, end.status, end.elapsed_ms, end.output
                ),
            ),
        }
    }
}
fn outcome(status: &str, content: &str) -> String {
    serde_json::json!({"status":status,"content":content}).to_string()
}
bindings::export!(ShellJob);
