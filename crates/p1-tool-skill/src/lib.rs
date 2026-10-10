//! One host tool over this agent's frozen, environment-selected skill data.

use std::sync::Arc;

use p1_contracts::skill::{SkillSource, SkillSummary};
use p1_contracts::{
    BoxFuture, Concurrency, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration,
    ToolIdentity, ToolInput, ToolOutcome,
};
use serde::Deserialize;
use serde_json::json;

pub struct SkillTool {
    source: Arc<dyn SkillSource>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl SkillTool {
    pub fn new(source: Arc<dyn SkillSource>) -> Self {
        Self {
            source,
            declaration: ToolDeclaration {
                name: "skill".into(),
                description: "Load a listed skill's instructions by name. Returns its body and skill directory; use shell to reach companion files. Skills are prompt data, never executed by this tool.".into(),
                kind: DeclarationKind::Function { input_schema: json!({
                    "type": "object", "properties": {"name": {"type": "string"}},
                    "required": ["name"], "additionalProperties": false
                }) },
            },
            identity: ToolIdentity { implementation: "p1-tool-skill".into(), variant: "default".into() },
        }
    }

    pub fn with_face(mut self, face: p1_contracts::tool::ToolFace, variant: &str) -> Self {
        self.declaration.name = face.name;
        self.declaration.description = face.description;
        self.identity.variant = variant.into();
        self
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    name: String,
}

impl Tool for SkillTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }
    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }
    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }
    fn concurrency(&self, _call: &ToolCall) -> Concurrency {
        Concurrency::Shared
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let ToolInput::Json(raw) = &call.input else {
                return ToolOutcome::error("skill expects JSON {\"name\": string}");
            };
            let input: Input = match serde_json::from_str(raw) {
                Ok(input) => input,
                Err(error) => return ToolOutcome::error(format!("invalid skill input: {error}")),
            };
            let skill = match self.source.load(&input.name) {
                Ok(skill) => skill,
                Err(error) => return ToolOutcome::error(error.to_string()),
            };
            let mut end = skill.body.len().min(100 * 1024);
            while !skill.body.is_char_boundary(end) {
                end -= 1;
            }
            let mut content = format!(
                "Skill directory: {}\n\n{}",
                skill.directory.display(),
                &skill.body[..end]
            );
            if end < skill.body.len() || skill.truncated {
                content.push_str("\n[Skill body truncated; at most 100 KiB returned.]\n");
            }
            ToolOutcome::ok(content)
        })
    }
}

/// The model's listing and its tool live together; sources supply only data.
pub fn listing(skills: &[SkillSummary], max_chars: usize, tool_name: &str) -> String {
    let detailed: String = skills
        .iter()
        .map(|skill| {
            format!(
                "<skill><name>{}</name><description>{}</description></skill>\n",
                xml(&skill.name),
                xml(&skill.description)
            )
        })
        .collect();
    let mut text = format!(
        "\n\n<skills>\nUse the {} tool to load a skill when the task matches its description.\n",
        xml(tool_name)
    );
    if text.chars().count() + detailed.chars().count() + "</skills>".len() > max_chars {
        text.push_str("Skill listing budget exceeded; names only.\n");
        for skill in skills {
            text.push_str(&format!(
                "<skill><name>{}</name></skill>\n",
                xml(&skill.name)
            ));
        }
    } else {
        text.push_str(&detailed);
    }
    text.push_str("</skills>");
    text
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}
