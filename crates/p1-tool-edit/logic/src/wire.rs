//! The JSON values of `docs/design/modules/protocol.md` the `edit` component reads and
//! writes: a tool call and a `tool_result` history item in, a tool outcome, a call
//! description and a result description out.
//!
//! The component's exports are thin: each one hands its argument text to a function here,
//! so every answer it gives is produced, and tested, natively.

use serde::{Deserialize, Serialize};

use crate::exec::{CallInput, Capabilities, Outcome, execute};
use crate::{EditInput, NAME, ResultSummary, VERB, describe_result, parse_json_input};

/// A complete tool call (`p1:protocol/tool-call/1`).
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ToolCall {
    pub call_id: String,
    pub name: String,
    pub input: ToolInput,
}

/// A tool call's input; `raw` is kept verbatim.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
pub enum ToolInput {
    Json { raw: String },
    Text { raw: String },
}

impl ToolCall {
    /// Parse a wire tool call.
    pub fn parse(text: &str) -> Result<Self, String> {
        serde_json::from_str(text).map_err(|error| error.to_string())
    }

    /// The input as [`execute`] takes it.
    pub fn call_input(&self) -> CallInput<'_> {
        match &self.input {
            ToolInput::Json { raw } => CallInput::Json(raw),
            ToolInput::Text { raw } => CallInput::Text(raw),
        }
    }

    /// The name the model called the tool by: messages name it, as the native tool names
    /// the face it was declared under. The default name when the call carries none.
    pub fn tool_name(&self) -> &str {
        if self.name.is_empty() {
            NAME
        } else {
            &self.name
        }
    }

    /// The parsed input, or `None` when it is not a valid `edit` input.
    pub fn edit_input(&self) -> Option<EditInput> {
        match &self.input {
            ToolInput::Json { raw } => parse_json_input(self.tool_name(), raw).ok(),
            ToolInput::Text { .. } => None,
        }
    }
}

/// `execute`: the tool outcome of the call `call_text`.
pub fn execute_call<C: Capabilities>(caps: &C, call_text: &str) -> String {
    let outcome = match ToolCall::parse(call_text) {
        Ok(call) => execute(caps, call.tool_name(), call.call_input()),
        // The host sends a valid tool call; this is only for a malformed one.
        Err(error) => Outcome::Error(format!(
            "Invalid input for {NAME}: the tool call is not readable: {error}"
        )),
    };
    outcome_json(&outcome)
}

/// A tool outcome (`p1:protocol/tool-outcome/1`).
pub fn outcome_json(outcome: &Outcome) -> String {
    #[derive(Serialize)]
    struct Wire<'a> {
        status: &'a str,
        content: &'a str,
    }
    let (status, content) = match outcome {
        Outcome::Ok(content) => ("ok", content.as_str()),
        Outcome::Error(content) => ("error", content.as_str()),
        Outcome::Cancelled => ("cancelled", ""),
    };
    to_json(&Wire { status, content })
}

#[derive(Serialize)]
struct WireCallDescription<'a> {
    verb: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    target: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    edit: Option<WireEditPreview<'a>>,
    destructive: bool,
}

#[derive(Serialize)]
struct WireEditPreview<'a> {
    path: &'a str,
    old: &'a str,
    new: &'a str,
}

/// `describe`: the call description (`p1:protocol/call-description/1`) of `call_text`, from
/// the call alone (ADR-0057). `destructive` is lexical, see [`crate::escapes_lexically`].
pub fn describe_call(call_text: &str) -> String {
    let input = ToolCall::parse(call_text)
        .ok()
        .and_then(|call| call.edit_input());
    let description = WireCallDescription {
        verb: VERB,
        target: input.as_ref().map(|input| input.file_path.as_str()),
        edit: input.as_ref().map(|input| WireEditPreview {
            path: &input.file_path,
            old: &input.old_string,
            new: &input.new_string,
        }),
        destructive: input
            .as_ref()
            .is_some_and(|input| crate::escapes_lexically(&input.file_path)),
    };
    to_json(&description)
}

/// The fields of a `tool_result` history item a result description reads.
#[derive(Deserialize)]
struct WireToolResult {
    item: String,
    status: String,
    content: String,
}

/// `describe-result`: the result description (`p1:protocol/result-description/1`) of the
/// `tool_result` item `result_text` for the call `call_text`.
pub fn describe_result_call(call_text: &str, result_text: &str) -> String {
    let result = serde_json::from_str::<WireToolResult>(result_text)
        .ok()
        .filter(|result| result.item == "tool_result");
    let (ok, content) = match &result {
        Some(result) => (result.status == "ok", result.content.as_str()),
        None => (false, ""),
    };
    let input = ToolCall::parse(call_text)
        .ok()
        .and_then(|call| call.edit_input());
    result_description_json(&describe_result(input, ok, content))
}

/// A result description (`p1:protocol/result-description/1`).
pub fn result_description_json(summary: &ResultSummary) -> String {
    #[derive(Serialize)]
    struct Wire<'a> {
        summary: &'a str,
        #[serde(skip_serializing_if = "Option::is_none")]
        detail: Option<Detail<'a>>,
    }
    #[derive(Serialize)]
    struct Detail<'a> {
        kind: &'a str,
        path: &'a str,
        before: &'a str,
        after: &'a str,
    }
    to_json(&Wire {
        summary: &summary.summary,
        detail: summary.diff.as_ref().map(|diff| Detail {
            kind: "diff",
            path: &diff.path,
            before: &diff.before,
            after: &diff.after,
        }),
    })
}

/// The declaration's input schema as JSON text.
pub fn input_schema_json() -> String {
    crate::input_schema().to_string()
}

fn to_json<T: Serialize>(value: &T) -> String {
    // Plain structs of strings and bools always serialize; an empty object would only ever
    // be the host's invalid output, never a panic in the guest.
    serde_json::to_string(value).unwrap_or_else(|_| String::from("{}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(input: serde_json::Value) -> String {
        serde_json::json!({"call_id": "c1", "name": "edit", "input": input}).to_string()
    }

    fn json_call(raw: &str) -> String {
        call(serde_json::json!({"kind": "json", "raw": raw}))
    }

    #[test]
    fn reads_a_wire_tool_call() {
        let parsed = ToolCall::parse(&json_call(r#"{"a":1}"#)).unwrap();
        assert_eq!(parsed.call_id, "c1");
        assert_eq!(parsed.call_input(), CallInput::Json(r#"{"a":1}"#));
        let text = ToolCall::parse(&call(serde_json::json!({"kind": "text", "raw": "x"})));
        assert_eq!(text.unwrap().call_input(), CallInput::Text("x"));
        assert!(
            ToolCall::parse(r#"{"call_id":"c","name":"e","input":{"kind":"json","raw":"","x":1}}"#)
                .is_err()
        );
    }

    #[test]
    fn writes_outcomes() {
        assert_eq!(
            outcome_json(&Outcome::Ok("Edited a (1 replacement).".into())),
            r#"{"status":"ok","content":"Edited a (1 replacement)."}"#
        );
        assert_eq!(
            outcome_json(&Outcome::Error("a \"b\"\nc".into())),
            r#"{"status":"error","content":"a \"b\"\nc"}"#
        );
        assert_eq!(
            outcome_json(&Outcome::Cancelled),
            r#"{"status":"cancelled","content":""}"#
        );
    }

    #[test]
    fn describes_a_call_from_its_input_alone() {
        let described = describe_call(&json_call(
            r#"{"file_path": "src/a.rs", "old_string": "a", "new_string": "b"}"#,
        ));
        assert_eq!(
            described,
            r#"{"verb":"edit","target":"src/a.rs","edit":{"path":"src/a.rs","old":"a","new":"b"},"destructive":false}"#
        );
        let escaping = describe_call(&json_call(
            r#"{"file_path": "../a.rs", "old_string": "a", "new_string": "b"}"#,
        ));
        assert!(escaping.ends_with(r#""destructive":true}"#), "{escaping}");
        assert_eq!(
            describe_call(&json_call("not json")),
            r#"{"verb":"edit","destructive":false}"#
        );
        assert_eq!(
            describe_call("garbage"),
            r#"{"verb":"edit","destructive":false}"#
        );
    }

    #[test]
    fn describes_results() {
        let call = json_call(r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#);
        let ok = serde_json::json!({
            "item": "tool_result", "call_id": "c1", "name": "edit",
            "status": "ok", "content": "Edited d.txt (1 replacement)."
        })
        .to_string();
        assert_eq!(
            describe_result_call(&call, &ok),
            r#"{"summary":"+1 −1","detail":{"kind":"diff","path":"d.txt","before":"two","after":"TWO"}}"#
        );
        let failed = serde_json::json!({
            "item": "tool_result", "call_id": "c1", "name": "edit",
            "status": "error", "content": "You must read d.txt before changing it.\nmore"
        })
        .to_string();
        assert_eq!(
            describe_result_call(&call, &failed),
            r#"{"summary":"You must read d.txt before changing it."}"#
        );
    }

    #[test]
    fn the_schema_crosses_as_json_text() {
        let schema: serde_json::Value = serde_json::from_str(&input_schema_json()).unwrap();
        assert_eq!(schema, crate::input_schema());
    }
}
