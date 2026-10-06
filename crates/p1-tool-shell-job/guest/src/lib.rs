//! Input and presentation for session-owned shell jobs.
#![forbid(unsafe_code)]
use serde::Deserialize;
pub const NAME: &str = "shell_job";
pub const DESCRIPTION: &str = "Check or cancel a background shell job owned by this session. Completion arrives as a notification without polling. Read stored output with read_output.";
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    Cancel,
}
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub job_id: String,
    pub action: Action,
}
pub fn parse(raw: &str) -> Result<Input, String> {
    let input: Input = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if input.job_id.is_empty() {
        return Err("job_id must not be empty".into());
    }
    Ok(input)
}
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({"type":"object","properties":{"job_id":{"type":"string","minLength":1},"action":{"type":"string","enum":["status","cancel"]}},"required":["job_id","action"],"additionalProperties":false})
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn validates_job_id_and_action() {
        assert!(parse(r#"{"job_id":"","action":"status"}"#).is_err());
        assert!(parse(r#"{"job_id":"j1","action":"list"}"#).is_err());
        assert_eq!(
            parse(r#"{"job_id":"j1","action":"cancel"}"#)
                .unwrap()
                .action,
            Action::Cancel
        );
    }
}
