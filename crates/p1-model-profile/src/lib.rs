//! Model policy consumed by compiled adapters. Route selection never changes it.
use p1_contracts::{Effort, ProviderError, ProviderErrorKind};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The smallest per-effort thinking budget a budget-style wire protocol accepts.
const MIN_THINKING_BUDGET: u32 = 1_024;

/// The compiled behaviour strategy a profile selects (`profiles/<id>.toml` `thinking`).
/// The kebab-case spellings below are the ones a profile file uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ThinkingPolicy {
    Enabled,
    /// Preserve the model's complete reasoning between turns, including tool use.
    Preserved,
    /// The model takes an effort level; the server decides how much to think.
    EffortLevel,
    /// The model takes a token budget per effort; the profile carries the table.
    Budget,
}

/// What a model is, independent of the route that reaches it (ADR-0039). Plain data
/// in `profiles/<id>.toml`; compiled adapters consume it, configuration never invents it.
///
/// `default_effort` is optional: a model whose effort-less requests carry no thinking
/// fields has none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelProfile {
    /// Must equal the profile file's stem.
    pub id: String,
    pub revision: u32,
    /// Canonical identity of the model, NOT a wire name.
    pub model_id: String,
    pub family: String,
    pub thinking: ThinkingPolicy,
    pub efforts: Vec<Effort>,
    /// When present it must be one of `efforts`; absent means a request that names no
    /// effort carries no thinking/reasoning fields at all.
    pub default_effort: Option<Effort>,
    /// Required, one entry per listed effort, when `thinking = "budget"`; forbidden for
    /// any other variant, where an absent table deserializes to the empty map.
    #[serde(default)]
    pub thinking_budgets: BTreeMap<Effort, u32>,
    /// Context capacity; `None` = unknown, never zero.
    pub context_tokens: Option<u64>,
    /// Output ceiling; `None` = unknown, never zero.
    pub max_output_tokens: Option<u32>,
}

impl ModelProfile {
    /// Parse the text of `profiles/<stem>.toml`. Unknown keys are rejected, `id`
    /// must equal `stem` (a profile cannot be renamed silently) and the result is
    /// validated before it is used.
    pub fn from_toml(stem: &str, text: &str) -> Result<Self, String> {
        let profile: Self = toml::from_str(text).map_err(|error| error.to_string())?;
        if profile.id != stem {
            return Err(format!(
                "profile id `{}` does not match the file name `{stem}.toml`",
                profile.id
            ));
        }
        profile.validate().map_err(|error| error.to_string())?;
        Ok(profile)
    }

    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.id.is_empty()
            || self.model_id.is_empty()
            || self.family.is_empty()
            || self
                .default_effort
                .is_some_and(|effort| !self.efforts.contains(&effort))
            || self.max_output_tokens == Some(0)
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "invalid model profile identity, default effort or output limit",
            ));
        }
        match self.thinking {
            ThinkingPolicy::Budget => {
                if self
                    .efforts
                    .iter()
                    .any(|effort| !self.thinking_budgets.contains_key(effort))
                    || self
                        .thinking_budgets
                        .keys()
                        .any(|effort| !self.efforts.contains(effort))
                {
                    return Err(ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        "thinking = \"budget\" requires one thinking budget per listed effort",
                    ));
                }
                if self
                    .thinking_budgets
                    .values()
                    .any(|&budget| budget < MIN_THINKING_BUDGET)
                {
                    return Err(ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        "a thinking budget must be at least 1024 tokens",
                    ));
                }
            }
            _ if !self.thinking_budgets.is_empty() => {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    "[thinking_budgets] is only valid for thinking = \"budget\"",
                ));
            }
            _ => {}
        }
        Ok(())
    }

    /// The profile's thinking budget for `effort`, if the profile carries a table.
    pub fn budget_for(&self, effort: Effort) -> Option<u32> {
        self.thinking_budgets.get(&effort).copied()
    }

    /// Resolve a request's effort against the profile: an explicit effort must be
    /// supported; an absent one becomes the profile's default, which may be `None`
    /// (the request then carries no thinking fields).
    pub fn resolve_effort(
        &self,
        requested: Option<Effort>,
    ) -> Result<Option<Effort>, ProviderError> {
        match requested {
            Some(effort) => {
                if !self.efforts.contains(&effort) {
                    return Err(ProviderError::new(
                        ProviderErrorKind::InvalidRequest,
                        "model profile does not support the requested reasoning effort",
                    ));
                }
                Ok(Some(effort))
            }
            None => Ok(self.default_effort),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FULL: &str = r#"
id             = "deepseek-v4.1-flash"
revision       = 3
model_id       = "deepseek-v4.1-flash"
family         = "deepseek"
thinking       = "enabled"
efforts        = ["high", "max"]
default_effort = "high"
context_tokens    = 128000
max_output_tokens = 32000
"#;

    const BUDGET: &str = r#"
id       = "claude-opus-4-6"
revision = 1
model_id = "claude-opus-4-6"
family   = "claude"
thinking = "budget"
efforts  = ["low", "medium", "high", "extra_high", "max"]

[thinking_budgets]
low = 4096
medium = 10240
high = 20480
extra_high = 32768
max = 32768
"#;

    const EFFORT_LEVEL: &str = r#"
id       = "claude-opus-5"
revision = 1
model_id = "claude-opus-5"
family   = "claude"
thinking = "effort-level"
efforts  = ["low", "medium", "high", "extra_high", "max"]
"#;

    #[test]
    fn a_complete_profile_file_parses_every_field() {
        let profile = ModelProfile::from_toml("deepseek-v4.1-flash", FULL).unwrap();
        assert_eq!(profile.id, "deepseek-v4.1-flash");
        assert_eq!(profile.revision, 3);
        assert_eq!(profile.model_id, "deepseek-v4.1-flash");
        assert_eq!(profile.family, "deepseek");
        assert_eq!(profile.thinking, ThinkingPolicy::Enabled);
        assert_eq!(profile.efforts, vec![Effort::High, Effort::Max]);
        assert_eq!(profile.default_effort, Some(Effort::High));
        assert!(profile.thinking_budgets.is_empty());
        assert_eq!(profile.context_tokens, Some(128_000));
        assert_eq!(profile.max_output_tokens, Some(32_000));
    }

    #[test]
    fn preserved_thinking_and_the_other_effort_spellings_parse() {
        let text = r#"
id             = "glm-5.3"
revision       = 1
model_id       = "glm-5.3"
family         = "glm"
thinking       = "preserved"
efforts        = ["low", "medium", "high", "extra_high", "max"]
default_effort = "low"
"#;
        let profile = ModelProfile::from_toml("glm-5.3", text).unwrap();
        assert_eq!(profile.thinking, ThinkingPolicy::Preserved);
        assert_eq!(
            profile.efforts,
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max
            ]
        );
        // Unknown capacity is absent, never zero.
        assert_eq!(profile.context_tokens, None);
        assert_eq!(profile.max_output_tokens, None);
    }

    #[test]
    fn an_effort_level_profile_parses_without_a_budget_table() {
        let profile = ModelProfile::from_toml("claude-opus-5", EFFORT_LEVEL).unwrap();
        assert_eq!(profile.thinking, ThinkingPolicy::EffortLevel);
        assert_eq!(profile.default_effort, None);
        assert!(profile.thinking_budgets.is_empty());
        assert_eq!(
            profile.efforts,
            vec![
                Effort::Low,
                Effort::Medium,
                Effort::High,
                Effort::ExtraHigh,
                Effort::Max
            ]
        );
    }

    #[test]
    fn a_budget_profile_carries_one_budget_per_effort() {
        let profile = ModelProfile::from_toml("claude-opus-4-6", BUDGET).unwrap();
        assert_eq!(profile.thinking, ThinkingPolicy::Budget);
        assert_eq!(profile.default_effort, None);
        assert_eq!(profile.budget_for(Effort::Low), Some(4_096));
        assert_eq!(profile.budget_for(Effort::Medium), Some(10_240));
        assert_eq!(profile.budget_for(Effort::High), Some(20_480));
        assert_eq!(profile.budget_for(Effort::ExtraHigh), Some(32_768));
        assert_eq!(profile.budget_for(Effort::Max), Some(32_768));
    }

    #[test]
    fn a_budget_without_an_entry_for_a_listed_effort_is_rejected() {
        // Drop the `max` budget while `max` stays a listed effort.
        let text = BUDGET.replace("max = 32768\n", "");
        assert!(ModelProfile::from_toml("claude-opus-4-6", &text).is_err());
    }

    #[test]
    fn a_budget_for_an_effort_that_is_not_listed_is_rejected() {
        // Keep the `max` budget but remove `max` from `efforts`.
        let text = BUDGET.replace(
            "efforts  = [\"low\", \"medium\", \"high\", \"extra_high\", \"max\"]",
            "efforts  = [\"low\", \"medium\", \"high\", \"extra_high\"]",
        );
        assert!(ModelProfile::from_toml("claude-opus-4-6", &text).is_err());
    }

    #[test]
    fn a_budget_below_the_minimum_is_rejected() {
        let text = BUDGET.replace("low = 4096", "low = 1023");
        assert!(ModelProfile::from_toml("claude-opus-4-6", &text).is_err());
        let at_minimum = BUDGET.replace("low = 4096", "low = 1024");
        assert!(ModelProfile::from_toml("claude-opus-4-6", &at_minimum).is_ok());
    }

    #[test]
    fn a_budget_table_on_a_non_budget_profile_is_rejected() {
        let text = format!("{FULL}\n[thinking_budgets]\nhigh = 4096\n");
        assert!(ModelProfile::from_toml("deepseek-v4.1-flash", &text).is_err());
    }

    #[test]
    fn an_absent_default_effort_resolves_to_no_effort() {
        let profile = ModelProfile::from_toml("claude-opus-5", EFFORT_LEVEL).unwrap();
        assert_eq!(profile.resolve_effort(None).unwrap(), None);
        assert_eq!(
            profile.resolve_effort(Some(Effort::ExtraHigh)).unwrap(),
            Some(Effort::ExtraHigh)
        );
    }

    #[test]
    fn a_requested_effort_is_still_checked_without_a_default() {
        let text = FULL.replace("default_effort = \"high\"\n", "");
        let profile = ModelProfile::from_toml("deepseek-v4.1-flash", &text).unwrap();
        assert_eq!(profile.default_effort, None);
        assert_eq!(profile.resolve_effort(None).unwrap(), None);
        assert_eq!(
            profile.resolve_effort(Some(Effort::Max)).unwrap(),
            Some(Effort::Max)
        );
        assert!(profile.resolve_effort(Some(Effort::Medium)).is_err());
    }

    #[test]
    fn an_unknown_key_is_rejected_by_name() {
        let text = format!("{FULL}\nfamliy = \"typo\"\n");
        let error = ModelProfile::from_toml("deepseek-v4.1-flash", &text).unwrap_err();
        assert!(error.contains("famliy"), "{error}");
    }

    #[test]
    fn the_file_stem_must_equal_the_profile_id() {
        let error = ModelProfile::from_toml("deepseek-v4.2-flash", FULL).unwrap_err();
        assert!(error.contains("deepseek-v4.1-flash"), "{error}");
        assert!(error.contains("deepseek-v4.2-flash"), "{error}");
    }

    #[test]
    fn a_default_effort_outside_the_efforts_is_rejected() {
        let text = FULL.replace("default_effort = \"high\"", "default_effort = \"low\"");
        assert!(ModelProfile::from_toml("deepseek-v4.1-flash", &text).is_err());
    }

    #[test]
    fn a_zero_output_limit_is_rejected() {
        let text = FULL.replace("max_output_tokens = 32000", "max_output_tokens = 0");
        assert!(ModelProfile::from_toml("deepseek-v4.1-flash", &text).is_err());
    }

    #[test]
    fn explicit_unsupported_effort_is_never_coerced_to_the_default() {
        let profile = ModelProfile {
            id: "example".into(),
            revision: 1,
            model_id: "example".into(),
            family: "example".into(),
            thinking: ThinkingPolicy::Enabled,
            efforts: vec![Effort::High, Effort::Max],
            default_effort: Some(Effort::High),
            thinking_budgets: BTreeMap::new(),
            context_tokens: None,
            max_output_tokens: None,
        };
        assert!(profile.validate().is_ok());
        assert_eq!(profile.resolve_effort(None).unwrap(), Some(Effort::High));
        assert_eq!(
            profile.resolve_effort(Some(Effort::Max)).unwrap(),
            Some(Effort::Max)
        );
        assert!(profile.resolve_effort(Some(Effort::Medium)).is_err());
        let invalid = ModelProfile {
            default_effort: Some(Effort::Low),
            ..profile
        };
        assert!(invalid.validate().is_err());
    }

    #[test]
    fn every_shipped_profile_file_parses() {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
        let mut seen = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().and_then(|extension| extension.to_str()) != Some("toml") {
                continue;
            }
            let stem = path.file_stem().unwrap().to_str().unwrap();
            let text = std::fs::read_to_string(&path).unwrap();
            ModelProfile::from_toml(stem, &text).unwrap_or_else(|error| panic!("{stem}: {error}"));
            seen += 1;
        }
        assert!(seen >= 2, "no profile files found in {}", dir.display());
    }
}
