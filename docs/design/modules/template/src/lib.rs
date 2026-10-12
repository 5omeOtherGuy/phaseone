//! `echo`: an example p1 tool module, built outside the phaseone repository with the guest
//! SDK. It returns the `text` argument it is given.
//!
//! Every value crossing the boundary is JSON in the form of the value protocol: the SDK's
//! `protocol` feature has the wire types (`docs/design/modules/protocol.md`).
#![forbid(unsafe_code)]

use p1_module_sdk::protocol::{
    WireCallDescription, WireResultDescription, WireToolCall, WireToolInput, WireToolOutcome,
    WireToolStatus,
};
use p1_module_sdk::serde_json::{self, Value, json};
use p1_module_sdk::tool::p1::module::types::DeclarationKind;
use p1_module_sdk::tool::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};

struct Echo;

impl Guest for Echo {
    fn declaration() -> ToolDeclaration {
        let schema = json!({
            "type": "object",
            "properties": { "text": { "type": "string", "description": "What to return." } },
            "required": ["text"],
            "additionalProperties": false,
        });
        ToolDeclaration {
            name: "echo".to_owned(),
            description: "Returns the text it is given, unchanged.".to_owned(),
            kind: DeclarationKind::Function(schema.to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }

    fn describe(call: ToolCall) -> CallDescription {
        let description = WireCallDescription {
            verb: "call".to_owned(),
            target: text(&call).ok(),
            edit: None,
            destructive: false,
            shared: true,
        };
        serde_json::to_string(&description).expect("a protocol value serializes")
    }

    fn describe_result(_call: ToolCall, _result: HistoryItem) -> ResultDescription {
        let description = WireResultDescription {
            summary: "echoed".to_owned(),
            detail: None,
        };
        serde_json::to_string(&description).expect("a protocol value serializes")
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let outcome = match text(&call) {
            Ok(text) => WireToolOutcome {
                status: WireToolStatus::Ok,
                content: text,
            },
            Err(reason) => WireToolOutcome {
                status: WireToolStatus::Error,
                content: reason,
            },
        };
        serde_json::to_string(&outcome).expect("a protocol value serializes")
    }
}

/// The call's `text` argument, or what is wrong with the call.
fn text(call: &str) -> Result<String, String> {
    let call: WireToolCall =
        serde_json::from_str(call).map_err(|error| format!("invalid tool call: {error}"))?;
    let WireToolInput::Json(arguments) = call.input else {
        return Err("echo takes JSON arguments".to_owned());
    };
    let arguments: Value = serde_json::from_str(&arguments)
        .map_err(|error| format!("the arguments are not JSON: {error}"))?;
    arguments["text"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "the `text` argument is missing".to_owned())
}

p1_module_sdk::tool::export!(Echo with_types_in p1_module_sdk::tool);
