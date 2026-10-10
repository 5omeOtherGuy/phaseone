//! Configured subagents and per-call overrides. The host loads prompts and resolves models;
//! this module resolves permissions before a worker can be assembled.

use std::collections::BTreeMap;

use p1_contracts::Effort;
use serde::{Deserialize, Serialize};

use crate::ChildSpec;

/// One entry in the operator's subagent configuration, not a compiled variant.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentDefinition {
    pub environment: String,
    pub description: String,
    /// Prompt file relative to the configuration directory. Read by the host's config reader.
    pub prompt_file: String,
    /// Prepend the parent's tool-conditional prompt template to this worker role.
    #[serde(default)]
    pub inherit_prompt: bool,
    pub tools: Vec<String>,
    pub models: Vec<String>,
    #[serde(default)]
    pub allowed_children: Vec<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentDefinitions {
    #[serde(default)]
    pub subagents: BTreeMap<String, SubagentDefinition>,
}

impl SubagentDefinitions {
    pub fn validate(&self) -> Result<(), String> {
        for (name, definition) in &self.subagents {
            if name.is_empty()
                || definition.environment.is_empty()
                || definition.description.trim().is_empty()
                || definition.description.contains(['\n', '\r'])
                || definition.prompt_file.is_empty()
                || definition.models.is_empty()
                || definition
                    .models
                    .iter()
                    .any(|model| model.trim().is_empty())
            {
                return Err(format!(
                    "subagent `{name}` needs an environment, one-line description, prompt file and model chain"
                ));
            }
            for child in &definition.allowed_children {
                if !self.subagents.contains_key(child) {
                    return Err(format!("subagent `{name}` allows unknown child `{child}`"));
                }
            }
        }
        Ok(())
    }

    /// Resolve defaults and replacements, intersecting tools with the parent's real grant.
    /// `allowed = None` is the operator's top-level agent; an empty list is a leaf worker.
    pub fn resolve(
        &self,
        request: SubagentRequest,
        parent_grant: &[String],
        allowed: Option<&[String]>,
    ) -> Result<ChildSpec, String> {
        let name = &request.subagent_type;
        if allowed.is_some_and(|children| !children.contains(name)) {
            return Err(format!("this agent may not start subagent `{name}`"));
        }
        let definition = self
            .subagents
            .get(name)
            .ok_or_else(|| format!("unknown subagent `{name}`"))?;
        let requested = request.tools.as_ref().unwrap_or(&definition.tools);
        let mut tools = Vec::new();
        for tool in requested {
            if parent_grant.contains(tool) && !tools.contains(tool) {
                tools.push(tool.clone());
            }
        }
        let models = match request.model {
            Some(model) if model.trim().is_empty() => return Err("model must not be empty".into()),
            Some(model) => vec![model],
            None => definition.models.clone(),
        };
        Ok(ChildSpec {
            environment: definition.environment.clone(),
            task: request.task,
            tools,
            workspace: None,
            options: ChildOptions {
                subagent_type: Some(request.subagent_type),
                models,
                effort: request.effort,
                system_prompt: request.system_prompt,
                prompt_file: Some(definition.prompt_file.clone()),
                allowed_children: definition.allowed_children.clone(),
                background: request.background,
                isolation: request.isolation,
                parent: None,
            },
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SubagentRequest {
    pub subagent_type: String,
    pub task: String,
    #[serde(default)]
    pub tools: Option<Vec<String>>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub effort: Option<Effort>,
    #[serde(default)]
    pub system_prompt: Option<String>,
    #[serde(default = "background_default")]
    pub background: bool,
    #[serde(default)]
    pub isolation: Isolation,
}

fn background_default() -> bool {
    true
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Isolation {
    #[default]
    Shared,
    Worktree,
}

/// Host-resolved options. Empty defaults preserve environment-based worker starts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildOptions {
    pub subagent_type: Option<String>,
    pub models: Vec<String>,
    pub effort: Option<Effort>,
    pub system_prompt: Option<String>,
    pub prompt_file: Option<String>,
    pub allowed_children: Vec<String>,
    pub background: bool,
    pub isolation: Isolation,
    /// Stamped by the host scope, never accepted from the guest request.
    pub parent: Option<String>,
}

impl Default for ChildOptions {
    fn default() -> Self {
        Self {
            subagent_type: None,
            models: Vec::new(),
            effort: None,
            system_prompt: None,
            prompt_file: None,
            allowed_children: Vec::new(),
            background: true,
            isolation: Isolation::Shared,
            parent: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn definitions() -> SubagentDefinitions {
        serde_json::from_value(json!({"subagents": {
            "task": {"environment":"coder", "description":"Implement code", "prompt_file":"task/prompt.md",
                "tools":["read", "write"], "models":["primary", "backup"], "allowed_children":["review"]},
            "review": {"environment":"reader", "description":"Review a diff", "prompt_file":"review/prompt.md",
                "tools":["read", "shell"], "models":["reviewer"]}
        }})).unwrap()
    }

    fn request(value: serde_json::Value) -> SubagentRequest {
        serde_json::from_value(value).unwrap()
    }

    #[test]
    fn defaults_are_clamped_and_leaf_is_not_top_level() {
        let definitions = definitions();
        definitions.validate().unwrap();
        let resolved = definitions
            .resolve(
                request(json!({"subagent_type":"task","task":"fix"})),
                &["read".into(), "shell".into()],
                None,
            )
            .unwrap();
        assert_eq!(resolved.tools, ["read"]);
        assert_eq!(resolved.options.models, ["primary", "backup"]);
        assert_eq!(resolved.options.allowed_children, ["review"]);
        assert!(resolved.options.background);
        assert!(
            definitions
                .resolve(
                    request(json!({"subagent_type":"review","task":"check"})),
                    &["read".into()],
                    Some(&[])
                )
                .is_err()
        );
        assert!(
            definitions
                .resolve(
                    request(json!({"subagent_type":"task","task":"fix"})),
                    &["read".into()],
                    Some(&["review".into()])
                )
                .is_err()
        );
    }

    #[test]
    fn overrides_replace_defaults_without_expanding_parent_access() {
        let resolved = definitions()
            .resolve(
                request(json!({"subagent_type":"task","task":"fix",
            "tools":["shell", "write", "shell"], "model":"chosen", "effort":"high",
            "system_prompt":"replacement", "background":false, "isolation":"worktree"})),
                &["read".into(), "shell".into()],
                Some(&["task".into()]),
            )
            .unwrap();
        assert_eq!(resolved.tools, ["shell"]);
        assert_eq!(resolved.options.models, ["chosen"]);
        assert_eq!(resolved.options.effort, Some(Effort::High));
        assert_eq!(
            resolved.options.system_prompt.as_deref(),
            Some("replacement")
        );
        assert!(!resolved.options.background);
        assert_eq!(resolved.options.isolation, Isolation::Worktree);
    }

    #[test]
    fn explicit_empty_tools_does_not_restore_defaults() {
        let resolved = definitions()
            .resolve(
                request(json!({"subagent_type":"task","task":"think","tools":[]})),
                &["read".into()],
                None,
            )
            .unwrap();
        assert!(resolved.tools.is_empty());
    }

    #[test]
    fn unknown_children_and_multiline_descriptions_are_rejected() {
        let mut definitions = definitions();
        definitions
            .subagents
            .get_mut("review")
            .unwrap()
            .allowed_children
            .push("missing".into());
        assert!(
            definitions
                .validate()
                .unwrap_err()
                .contains("unknown child")
        );
        definitions
            .subagents
            .get_mut("review")
            .unwrap()
            .allowed_children
            .clear();
        definitions.subagents.get_mut("review").unwrap().description = "one\ntwo".into();
        assert!(definitions.validate().is_err());
    }
}
