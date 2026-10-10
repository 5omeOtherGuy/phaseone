//! Skill data crosses a host port; the component has no filesystem authority.
#![forbid(unsafe_code)]
use p1_bindings_tool::generated::p1::module::{skills, types::DeclarationKind};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_contracts::skill::LoadedSkill;
use p1_tool_skill as guest;
use serde_json::{Value, json};

struct Skill;
impl Guest for Skill {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: guest::NAME.into(),
            description: guest::DESCRIPTION.into(),
            kind: DeclarationKind::Function(guest::input_schema().to_string()),
        }
    }
    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }
    fn describe(call: ToolCall) -> CallDescription {
        let target = input(&call).ok().map(|input| input.name);
        json!({"verb":"load", "target":target, "destructive":false, "shared":true}).to_string()
    }
    fn describe_result(_call: ToolCall, result: HistoryItem) -> ResultDescription {
        let item: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
        json!({"summary": if item["status"] == "ok" { "skill loaded" } else { "skill load failed" }}).to_string()
    }
    fn execute(call: ToolCall) -> ToolOutcome {
        let result = input(&call).and_then(|input| {
            skills::load(&input.name)
                .map(|skill| {
                    guest::render(&LoadedSkill {
                        body: skill.body,
                        directory: skill.directory.into(),
                        truncated: skill.truncated,
                    })
                })
                .map_err(|error| match error {
                    skills::SkillError::Message(message) => message,
                })
        });
        match result {
            Ok(content) => json!({"status":"ok", "content":content}),
            Err(content) => json!({"status":"error", "content":content}),
        }
        .to_string()
    }
}
fn input(call: &str) -> Result<guest::Input, String> {
    let call: Value =
        serde_json::from_str(call).map_err(|error| format!("invalid skill call: {error}"))?;
    if call["input"]["kind"] != "json" {
        return Err("skill expects JSON {\"name\": string}".into());
    }
    let raw = call["input"]["raw"]
        .as_str()
        .ok_or("skill expects JSON arguments")?;
    guest::Input::parse(raw)
}
p1_bindings_tool::generated::export!(Skill);
