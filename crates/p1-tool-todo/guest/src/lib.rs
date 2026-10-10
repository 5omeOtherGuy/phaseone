//! Pure computation for the todo component; the session owns durability and state.
use p1_contracts::plan::PlanEntry;
use serde::Deserialize;
use serde_json::{Value, json};

pub const NAME: &str = "todo_write";
pub const DESCRIPTION: &str = "Replace the session's to-do list with the complete current list. Use it for multi-step or complex work so the user can see your plan and progress; skip it for trivial tasks. Keep items concrete and ordered, mark work in_progress when starting and completed only when actually finished. Preserve unfinished items when replacing the list. Priorities are low, medium, or high. Send an empty todos array to clear the list. This tracks work; it does not execute tasks or end the turn.";

pub fn input_schema() -> Value {
    json!({"type":"object", "properties":{"todos":{"type":"array", "maxItems":100,
        "items":{"type":"object", "properties":{
            "content":{"type":"string", "minLength":1, "maxLength":2000},
            "status":{"enum":["pending","in_progress","completed"]},
            "priority":{"enum":["low","medium","high"]}},
            "required":["content","status","priority"], "additionalProperties":false}}},
        "required":["todos"], "additionalProperties":false})
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    todos: Vec<PlanEntry>,
}

fn validate(entries: &[PlanEntry]) -> Result<(), String> {
    if entries.len() > 100 {
        return Err("plan must contain at most 100 items".into());
    }
    if entries
        .iter()
        .any(|entry| entry.content.trim().is_empty() || entry.content.chars().count() > 2000)
    {
        return Err("item content must contain 1–2000 characters of nonblank text".into());
    }
    Ok(())
}

/// Successful content is the complete snapshot, durably carried by ToolFinished.
pub fn execute(raw: &str) -> Value {
    let result = serde_json::from_str::<Input>(raw)
        .map_err(|_| "expected {\"todos\": [{\"content\": string, \"status\": pending|in_progress|completed, \"priority\": low|medium|high}]} with no extra fields".to_string())
        .and_then(|input| validate(&input.todos).map(|()| input.todos));
    match result {
        Ok(entries) => json!({"status":"ok", "content":json!({"todos":entries}).to_string()}),
        Err(error) => {
            json!({"status":"error", "content":format!("invalid todo_write input: {error}")})
        }
    }
}

/// Decode a committed result without reapplying input bounds: redaction can expand
/// valid content into longer markers. The journal is the authoritative snapshot.
pub fn snapshot(content: &str) -> Result<Vec<PlanEntry>, String> {
    let input: Input =
        serde_json::from_str(content).map_err(|_| "invalid todo snapshot".to_string())?;
    Ok(input.todos)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn replaces_in_order_preserving_status_and_priority_and_can_clear() {
        let todos = json!([
            {"content":"Verify patch", "status":"in_progress", "priority":"high"},
            {"content":"Read requirements", "status":"completed", "priority":"low"},
            {"content":"Document", "status":"pending", "priority":"medium"}
        ]);
        let result = execute(&json!({"todos":todos}).to_string());
        assert_eq!(result["status"], "ok");
        assert_eq!(
            snapshot(result["content"].as_str().unwrap()).unwrap(),
            serde_json::from_value::<Vec<PlanEntry>>(todos).unwrap()
        );
        assert_eq!(
            snapshot(execute(r#"{"todos":[]}"#)["content"].as_str().unwrap()).unwrap(),
            vec![]
        );
    }

    #[test]
    fn invalid_input_never_proposes_a_replacement() {
        for raw in [
            "not json",
            "{}",
            r#"{"todos":null}"#,
            r#"{"todos":[],"extra":true}"#,
            r#"{"todos":[{"content":"x","status":"done","priority":"high"}]}"#,
            r#"{"todos":[{"content":"x","status":"pending","priority":"urgent"}]}"#,
            r#"{"todos":[{"content":" ","status":"pending","priority":"low"}]}"#,
            r#"{"todos":[{"content":"x","status":"pending","priority":"low","id":1}]}"#,
        ] {
            let result = execute(raw);
            assert_eq!(result["status"], "error", "{raw}");
            assert!(snapshot(result["content"].as_str().unwrap()).is_err());
        }
        let item = json!({"content":"x","status":"pending","priority":"low"});
        assert_eq!(
            execute(&json!({"todos":vec![item.clone();100]}).to_string())["status"],
            "ok"
        );
        assert_eq!(
            execute(&json!({"todos":vec![item;101]}).to_string())["status"],
            "error"
        );
        for (characters, status) in [(2000, "ok"), (2001, "error")] {
            for letter in ["x", "é"] {
                assert_eq!(execute(&json!({"todos":[{"content":letter.repeat(characters),"status":"pending","priority":"low"}]}).to_string())["status"], status);
            }
        }
    }
}
