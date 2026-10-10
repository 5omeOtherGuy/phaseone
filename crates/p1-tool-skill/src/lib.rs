//! Pure guest computation and prompt listing for environment-selected skill data.

use p1_contracts::skill::{LoadedSkill, SkillSummary};
use serde::Deserialize;
use serde_json::json;

pub const NAME: &str = "skill";
pub const DESCRIPTION: &str = "Load a listed skill's instructions by name. Returns its body and skill directory; use shell to reach companion files. Skills are prompt data, never executed by this tool.";

pub fn input_schema() -> serde_json::Value {
    json!({
        "type": "object", "properties": {"name": {"type": "string"}},
        "required": ["name"], "additionalProperties": false
    })
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub name: String,
}

impl Input {
    pub fn parse(raw: &str) -> Result<Self, String> {
        serde_json::from_str(raw).map_err(|error| format!("invalid skill input: {error}"))
    }
}

pub fn render(skill: &LoadedSkill) -> String {
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
    content
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
