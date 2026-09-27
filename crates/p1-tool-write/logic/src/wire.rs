//! The JSON value families of `docs/design/modules/protocol.md` that the `write` component
//! reads and writes: a tool call and a `tool_result` history item in, a tool outcome and
//! the call and result descriptions out.
//!
//! Only the shapes this tool needs, spelled as the schemas of `p1-module-protocol` spell
//! them (that crate is a host crate, so this one may not depend on it): closed objects,
//! and optional values omitted rather than sent as `null`.

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::{
    Description, Outcome, RawInput, ResultSummary, Status, VERB, describe, describe_result,
    lexically_confined, plain_result,
};

/// Which kind of input a call carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputKind {
    Json,
    Text,
}

/// A wire tool call (`p1:protocol/tool-call/1`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Call {
    pub call_id: String,
    /// The name the model called the tool by.
    pub name: String,
    pub kind: InputKind,
    /// The input, verbatim.
    pub raw: String,
}

impl Call {
    /// The call's input as the tool logic reads it.
    pub fn input(&self) -> RawInput<'_> {
        match self.kind {
            InputKind::Json => RawInput::Json(&self.raw),
            InputKind::Text => RawInput::Text(&self.raw),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WireCall {
    call_id: String,
    name: String,
    input: WireInput,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase", deny_unknown_fields)]
enum WireInput {
    Json { raw: String },
    Text { raw: String },
}

/// Parse a wire tool call; the error says why the text is not one.
pub fn parse_call(json: &str) -> Result<Call, String> {
    let call: WireCall = serde_json::from_str(json).map_err(|error| error.to_string())?;
    let (kind, raw) = match call.input {
        WireInput::Json { raw } => (InputKind::Json, raw),
        WireInput::Text { raw } => (InputKind::Text, raw),
    };
    Ok(Call {
        call_id: call.call_id,
        name: call.name,
        kind,
        raw,
    })
}

/// The part of a `tool_result` history item a result description reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolResult {
    /// One of the closed `ToolStatus` names (`ok`, `error`, …).
    pub status: String,
    /// Exactly what the model was shown.
    pub content: String,
}

/// Read a `tool_result` history item, or `None` when `json` is not one.
pub fn parse_tool_result(json: &str) -> Option<ToolResult> {
    let item: Value = serde_json::from_str(json).ok()?;
    if item.get("item")?.as_str()? != "tool_result" {
        return None;
    }
    Some(ToolResult {
        status: item.get("status")?.as_str()?.to_string(),
        content: item.get("content")?.as_str()?.to_string(),
    })
}

/// A tool outcome (`p1:protocol/tool-outcome/1`).
pub fn outcome_json(outcome: &Outcome) -> String {
    let status = match outcome.status {
        Status::Ok => "ok",
        Status::Error => "error",
        Status::Cancelled => "cancelled",
    };
    json!({ "status": status, "content": outcome.content }).to_string()
}

/// A call description (`p1:protocol/call-description/1`).
pub fn call_description_json(description: &Description) -> String {
    let mut object = Map::new();
    object.insert("verb".into(), VERB.into());
    if let Some(target) = &description.target {
        object.insert("target".into(), target.as_str().into());
    }
    if let Some(edit) = &description.edit {
        object.insert(
            "edit".into(),
            json!({ "path": edit.path, "old": edit.old, "new": edit.new }),
        );
    }
    object.insert("destructive".into(), description.destructive.into());
    Value::Object(object).to_string()
}

/// A result description (`p1:protocol/result-description/1`).
pub fn result_description_json(summary: &ResultSummary) -> String {
    let mut object = Map::new();
    object.insert("summary".into(), summary.summary.as_str().into());
    if let Some(diff) = &summary.diff {
        object.insert(
            "detail".into(),
            json!({
                "kind": "diff",
                "path": diff.path,
                "before": diff.before,
                "after": diff.after,
            }),
        );
    }
    Value::Object(object).to_string()
}

/// `describe`: the call description (`p1:protocol/call-description/1`) of `call_text`, from
/// the call alone (ADR-0057).
///
/// The body of `modules/p1-module-write`'s `describe` export, so the export and the U-desc
/// suite answer from the same function and cannot drift (they are one copy, not two).
/// `describe` runs on the restricted path with no capability, so an escape is decided
/// lexically ([`lexically_confined`]).
pub fn describe_call(call_text: &str) -> String {
    let described = match parse_call(call_text) {
        Ok(call) => describe(call.input(), |path| !lexically_confined(path)),
        // The host sends only calls that match the schema; one it could not read is
        // described as a call whose input did not parse.
        Err(_) => Description {
            target: None,
            edit: None,
            destructive: false,
        },
    };
    call_description_json(&described)
}

/// `describe-result`: the result description (`p1:protocol/result-description/1`) of the
/// `tool_result` item `result_text` for the call `call_text`. The body of
/// `modules/p1-module-write`'s `describe-result` export.
pub fn describe_result_call(call_text: &str, result_text: &str) -> String {
    let result = parse_tool_result(result_text);
    let described = match (parse_call(call_text), result) {
        (Ok(call), Some(result)) => {
            describe_result(call.input(), result.status == "ok", &result.content)
        }
        (Err(_), Some(result)) => plain_result(&result.content),
        (_, None) => plain_result(""),
    };
    result_description_json(&described)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{EditPreview, ResultDiff};

    #[test]
    fn a_wire_call_is_read_with_its_input_kind() {
        let call = parse_call(
            r#"{"call_id":"c1","name":"write","input":{"kind":"json","raw":"{\"a\":1}"}}"#,
        )
        .unwrap();
        assert_eq!(call.name, "write");
        assert_eq!(call.input(), RawInput::Json(r#"{"a":1}"#));
        let text =
            parse_call(r#"{"call_id":"c1","name":"w","input":{"kind":"text","raw":"x"}}"#).unwrap();
        assert_eq!(text.input(), RawInput::Text("x"));
    }

    #[test]
    fn a_call_of_another_shape_is_refused() {
        assert!(parse_call("{}").is_err());
        assert!(
            parse_call(r#"{"call_id":"c","name":"w","input":{"kind":"json","raw":"x","extra":1}}"#)
                .is_err()
        );
        assert!(
            parse_call(r#"{"call_id":"c","name":"w","input":{"kind":"other","raw":"x"}}"#).is_err()
        );
        assert!(
            parse_call(r#"{"call_id":"c","name":"w","input":{"kind":"json","raw":"x"},"x":1}"#)
                .is_err()
        );
    }

    #[test]
    fn a_tool_result_item_is_read_and_other_items_are_not() {
        let item = r#"{"item":"tool_result","call_id":"c1","name":"write","status":"ok","content":"Wrote a (1 bytes)."}"#;
        assert_eq!(
            parse_tool_result(item),
            Some(ToolResult {
                status: "ok".into(),
                content: "Wrote a (1 bytes).".into()
            })
        );
        assert_eq!(parse_tool_result(r#"{"item":"user","text":"hi"}"#), None);
        assert_eq!(parse_tool_result("not json"), None);
    }

    #[test]
    fn outcomes_and_descriptions_have_the_protocol_shapes() {
        let outcome = Outcome {
            status: Status::Cancelled,
            content: String::new(),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&outcome_json(&outcome)).unwrap(),
            json!({"status": "cancelled", "content": ""})
        );

        let full = Description {
            target: Some("a.txt".into()),
            edit: Some(EditPreview {
                path: "a.txt".into(),
                old: String::new(),
                new: "hi".into(),
            }),
            destructive: true,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&call_description_json(&full)).unwrap(),
            json!({
                "verb": "edit",
                "target": "a.txt",
                "edit": {"path": "a.txt", "old": "", "new": "hi"},
                "destructive": true
            })
        );
        let bare = Description {
            target: None,
            edit: None,
            destructive: false,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&call_description_json(&bare)).unwrap(),
            json!({"verb": "edit", "destructive": false})
        );

        let diff = ResultSummary {
            summary: "1 lines".into(),
            diff: Some(ResultDiff {
                path: "a".into(),
                before: String::new(),
                after: "x".into(),
            }),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&result_description_json(&diff)).unwrap(),
            json!({
                "summary": "1 lines",
                "detail": {"kind": "diff", "path": "a", "before": "", "after": "x"}
            })
        );
        let plain = ResultSummary {
            summary: "err".into(),
            diff: None,
        };
        assert_eq!(result_description_json(&plain), r#"{"summary":"err"}"#);
    }

    #[test]
    fn describes_a_call_and_a_result_from_the_wire_text_alone() {
        let call = |raw: &str| {
            format!(
                r#"{{"call_id":"c1","name":"write","input":{{"kind":"json","raw":{}}}}}"#,
                serde_json::to_string(raw).unwrap()
            )
        };
        assert_eq!(
            serde_json::from_str::<Value>(&describe_call(&call(
                r#"{"file_path":"out.txt","content":"hi\n"}"#
            )))
            .unwrap(),
            json!({
                "verb": "edit",
                "target": "out.txt",
                "edit": {"path": "out.txt", "old": "", "new": "hi\n"},
                "destructive": false
            })
        );
        // The restricted path decides an escape lexically: `..` climbs out.
        assert_eq!(
            serde_json::from_str::<Value>(&describe_call(&call(
                r#"{"file_path":"../out.txt","content":"x"}"#
            )))
            .unwrap()["destructive"],
            json!(true)
        );
        // Unparsable text names nothing and is not destructive, as the export answers.
        assert_eq!(
            serde_json::from_str::<Value>(&describe_call("garbage")).unwrap(),
            json!({"verb": "edit", "destructive": false})
        );

        let ok = call(r#"{"file_path":"out.txt","content":"x"}"#);
        let item = r#"{"item":"tool_result","call_id":"c1","name":"write","status":"ok","content":"Wrote out.txt (4000 bytes)."}"#;
        assert_eq!(
            serde_json::from_str::<Value>(&describe_result_call(&ok, item)).unwrap(),
            json!({
                "summary": "1 lines · 4.0 kB",
                "detail": {"kind": "diff", "path": "out.txt", "before": "", "after": "x"}
            })
        );
        assert_eq!(
            describe_result_call(&ok, "not a tool result"),
            r#"{"summary":""}"#
        );
    }
}
