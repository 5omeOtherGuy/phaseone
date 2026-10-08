//! One tool-world adapter, compiled into each independently grantable component.
#![forbid(unsafe_code)]
use bindings::p1::module::{github_api, types::DeclarationKind};
pub use p1_bindings_tool::generated as bindings;
use p1_github_guest::{Api, Error};
use serde_json::{Value, json};

struct Host;
impl Api for Host {
    fn get(&mut self, path: &str, raw: bool) -> Result<String, Error> {
        github_api::get(path, raw).map_err(|e| match e {
            github_api::Error::Cancelled => Error::Cancelled,
            github_api::Error::Failed(s) => Error::Message(s),
        })
    }
}
pub fn declaration(name: &str) -> bindings::ToolDeclaration {
    bindings::ToolDeclaration {
        name: name.into(),
        description: p1_github_guest::description(name).into(),
        kind: DeclarationKind::Function(p1_github_guest::input_schema(name).to_string()),
    }
}
pub fn describe(call: &str) -> String {
    let input = arguments(call)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok());
    let mut result = json!({"verb":"search","destructive":false});
    if let Some(target) = input.and_then(|i| i["repository"].as_str().map(str::to_owned)) {
        result["target"] = target.into();
    }
    result.to_string()
}
pub fn describe_result(result: &str) -> String {
    let result: Value = serde_json::from_str(result).unwrap_or(Value::Null);
    json!({"summary":if result["status"]=="ok" {"GitHub research completed"}else{"GitHub research failed"}}).to_string()
}
fn arguments(call: &str) -> Result<String, Error> {
    let call: Value = serde_json::from_str(call).map_err(|_| "invalid tool call")?;
    if call["input"]["kind"] != "json" {
        return Err("GitHub tools require JSON arguments".into());
    }
    call["input"]["raw"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| "missing JSON arguments".into())
}
pub fn execute(name: &str, call: &str) -> String {
    let result = arguments(call).and_then(|raw| p1_github_guest::execute(name, &raw, &mut Host));
    let (status, content) = match result {
        Ok(s) => ("ok", s),
        Err(Error::Cancelled) => ("cancelled", String::new()),
        Err(Error::Message(s)) => ("error", s),
    };
    json!({"status":status,"content":content}).to_string()
}

#[macro_export]
macro_rules! export_tool {
    ($component:ident, $name:literal) => {
        struct $component;
        impl $crate::bindings::Guest for $component {
            fn declaration() -> $crate::bindings::ToolDeclaration {
                $crate::declaration($name)
            }
            fn effect(_: $crate::bindings::ToolCall) -> $crate::bindings::CallEffect {
                $crate::bindings::CallEffect::ReadOnly
            }
            fn describe(call: $crate::bindings::ToolCall) -> $crate::bindings::CallDescription {
                $crate::describe(&call)
            }
            fn describe_result(
                _: $crate::bindings::ToolCall,
                result: $crate::bindings::HistoryItem,
            ) -> $crate::bindings::ResultDescription {
                $crate::describe_result(&result)
            }
            fn execute(call: $crate::bindings::ToolCall) -> $crate::bindings::ToolOutcome {
                $crate::execute($name, &call)
            }
        }
        $crate::bindings::export!($component);
    };
}
