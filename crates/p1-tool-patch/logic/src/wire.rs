//! The JSON value families of `docs/design/modules/protocol.md` that the `apply_patch`
//! component reads and writes: a tool call and a `tool_result` history item in, a tool
//! outcome and the call and result descriptions out.
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

    /// Whether the call is read in the freeform declaration form.
    ///
    /// The component declares the freeform form; a host that presents it as a function
    /// tool (a route without freeform tools, as the native `PatchTool::function_face`)
    /// sends JSON input, so the input kind is what tells the two forms apart here.
    pub fn freeform(&self) -> bool {
        self.kind == InputKind::Text
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

/// A call description (`p1:protocol/call-description/1`); a patch carries no edit preview.
pub fn call_description_json(description: &Description) -> String {
    let mut object = Map::new();
    object.insert("verb".into(), VERB.into());
    if let Some(target) = &description.target {
        object.insert("target".into(), target.as_str().into());
    }
    object.insert("destructive".into(), description.destructive.into());
    Value::Object(object).to_string()
}

/// A result description (`p1:protocol/result-description/1`), its detail the `files` kind.
pub fn result_description_json(summary: &ResultSummary) -> String {
    let mut object = Map::new();
    object.insert("summary".into(), summary.summary.as_str().into());
    if let Some(paths) = &summary.files {
        object.insert("detail".into(), json!({ "kind": "files", "paths": paths }));
    }
    Value::Object(object).to_string()
}

/// `describe`: the call description (`p1:protocol/call-description/1`) of `call_text`, from
/// the call alone (ADR-0057).
///
/// The body of `modules/p1-module-patch`'s `describe` export, so the export and the U-desc
/// suite answer from the same function and cannot drift (they are one copy, not two).
/// `describe` runs on the restricted path with no capability, so an escape is decided
/// lexically ([`lexically_confined`]).
pub fn describe_call(call_text: &str) -> String {
    let described = match parse_call(call_text) {
        Ok(call) => describe(call.freeform(), call.input(), |path| {
            !lexically_confined(path)
        }),
        // The host sends only calls it could read; one that does not parse is described as a
        // patch that did not parse.
        Err(_) => Description {
            target: None,
            destructive: false,
        },
    };
    call_description_json(&described)
}

/// `describe-result`: the result description (`p1:protocol/result-description/1`) of the
/// `tool_result` item `result_text` for the call `call_text`. The body of
/// `modules/p1-module-patch`'s `describe-result` export.
pub fn describe_result_call(call_text: &str, result_text: &str) -> String {
    let result = parse_tool_result(result_text);
    let described = match (parse_call(call_text), result) {
        (Ok(call), Some(result)) => describe_result(
            call.freeform(),
            call.input(),
            result.status == "ok",
            &result.content,
        ),
        (Err(_), Some(result)) => plain_result(&result.content),
        (_, None) => plain_result(""),
    };
    result_description_json(&described)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_wire_call_is_read_with_its_input_kind_and_form() {
        let text = parse_call(
            r#"{"call_id":"c1","name":"apply_patch","input":{"kind":"text","raw":"*** Begin Patch"}}"#,
        )
        .unwrap();
        assert_eq!(text.name, "apply_patch");
        assert_eq!(text.input(), RawInput::Text("*** Begin Patch"));
        assert!(text.freeform());
        let json = parse_call(
            r#"{"call_id":"c1","name":"Patch","input":{"kind":"json","raw":"{\"patch\":\"p\"}"}}"#,
        )
        .unwrap();
        assert_eq!(json.input(), RawInput::Json(r#"{"patch":"p"}"#));
        assert!(!json.freeform());
    }

    #[test]
    fn a_call_of_another_shape_is_refused() {
        assert!(parse_call("{}").is_err());
        assert!(
            parse_call(r#"{"call_id":"c","name":"p","input":{"kind":"text","raw":"x","extra":1}}"#)
                .is_err()
        );
        assert!(
            parse_call(r#"{"call_id":"c","name":"p","input":{"kind":"other","raw":"x"}}"#).is_err()
        );
    }

    #[test]
    fn a_tool_result_item_is_read_and_other_items_are_not() {
        let item = r#"{"item":"tool_result","call_id":"c1","name":"apply_patch","status":"ok","content":"A a"}"#;
        assert_eq!(
            parse_tool_result(item),
            Some(ToolResult {
                status: "ok".into(),
                content: "A a".into()
            })
        );
        assert_eq!(parse_tool_result(r#"{"item":"user","text":"hi"}"#), None);
        assert_eq!(parse_tool_result("not json"), None);
    }

    #[test]
    fn outcomes_and_descriptions_have_the_protocol_shapes() {
        let outcome = Outcome {
            status: Status::Error,
            content: "a already exists.".into(),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&outcome_json(&outcome)).unwrap(),
            json!({"status": "error", "content": "a already exists."})
        );

        let described = Description {
            target: Some("2 files".into()),
            destructive: true,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&call_description_json(&described)).unwrap(),
            json!({"verb": "edit", "target": "2 files", "destructive": true})
        );
        let bare = Description {
            target: None,
            destructive: false,
        };
        assert_eq!(
            serde_json::from_str::<Value>(&call_description_json(&bare)).unwrap(),
            json!({"verb": "edit", "destructive": false})
        );

        let files = ResultSummary {
            summary: "+1 · 1 files".into(),
            files: Some(vec!["a\t+1 −0".into()]),
        };
        assert_eq!(
            serde_json::from_str::<Value>(&result_description_json(&files)).unwrap(),
            json!({"summary": "+1 · 1 files", "detail": {"kind": "files", "paths": ["a\t+1 −0"]}})
        );
        let plain = ResultSummary {
            summary: "err".into(),
            files: None,
        };
        assert_eq!(result_description_json(&plain), r#"{"summary":"err"}"#);
    }

    #[test]
    fn describes_a_call_and_a_result_from_the_wire_text_alone() {
        let text = |raw: &str| {
            format!(
                r#"{{"call_id":"c1","name":"apply_patch","input":{{"kind":"text","raw":{}}}}}"#,
                serde_json::to_string(raw).unwrap()
            )
        };
        let patch = "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n";
        assert_eq!(
            serde_json::from_str::<Value>(&describe_call(&text(patch))).unwrap(),
            json!({"verb": "edit", "target": "src/a.rs", "destructive": false})
        );
        // A function-face call (JSON `{"patch": …}`) is read the same way.
        let delete = "*** Begin Patch\n*** Delete File: old.rs\n*** End Patch\n";
        let function = format!(
            r#"{{"call_id":"c1","name":"apply_patch","input":{{"kind":"json","raw":{}}}}}"#,
            serde_json::to_string(&json!({ "patch": delete }).to_string()).unwrap()
        );
        assert_eq!(
            serde_json::from_str::<Value>(&describe_call(&function)).unwrap(),
            json!({"verb": "edit", "target": "old.rs", "destructive": false})
        );
        // Unparsable text names nothing and is not destructive, as the export answers.
        assert_eq!(
            serde_json::from_str::<Value>(&describe_call("garbage")).unwrap(),
            json!({"verb": "edit", "destructive": false})
        );

        let item = r#"{"item":"tool_result","call_id":"c1","name":"apply_patch","status":"ok","content":"M src/a.rs"}"#;
        assert_eq!(
            serde_json::from_str::<Value>(&describe_result_call(&text(patch), item)).unwrap(),
            json!({
                "summary": "+1 −1 · 1 files",
                "detail": {"kind": "files", "paths": ["src/a.rs\t+1 −1"]}
            })
        );
        assert_eq!(
            describe_result_call(&text(patch), "not a tool result"),
            r#"{"summary":""}"#
        );
    }
}
