//! Model policy consumed by compiled adapters. Route selection never changes it.
use p1_contracts::{Effort, ProviderError, ProviderErrorKind};
use serde::Deserialize;

/// The compiled behaviour strategy a profile selects (`profiles/<id>.toml` `thinking`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingPolicy {
    Enabled,
    /// Preserve the model's complete reasoning between turns, including tool use.
    Preserved,
}

/// What a model is, independent of the route that reaches it (ADR-0039). Plain data
/// in `profiles/<id>.toml`; compiled adapters consume it, configuration never invents it.
///
/// `default_effort` is required for now because every shipped model has one: it becomes
/// optional when a model that needs no effort appears.
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
    pub default_effort: Effort,
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
            || !self.efforts.contains(&self.default_effort)
            || self.max_output_tokens == Some(0)
        {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "invalid model profile identity, default effort or output limit",
            ));
        }
        Ok(())
    }

    pub fn resolve_effort(&self, requested: Option<Effort>) -> Result<Effort, ProviderError> {
        let effort = requested.unwrap_or(self.default_effort);
        if !self.efforts.contains(&effort) {
            return Err(ProviderError::new(
                ProviderErrorKind::InvalidRequest,
                "model profile does not support the requested reasoning effort",
            ));
        }
        Ok(effort)
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

    #[test]
    fn a_complete_profile_file_parses_every_field() {
        let profile = ModelProfile::from_toml("deepseek-v4.1-flash", FULL).unwrap();
        assert_eq!(profile.id, "deepseek-v4.1-flash");
        assert_eq!(profile.revision, 3);
        assert_eq!(profile.model_id, "deepseek-v4.1-flash");
        assert_eq!(profile.family, "deepseek");
        assert_eq!(profile.thinking, ThinkingPolicy::Enabled);
        assert_eq!(profile.efforts, vec![Effort::High, Effort::Max]);
        assert_eq!(profile.default_effort, Effort::High);
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
            default_effort: Effort::High,
            context_tokens: None,
            max_output_tokens: None,
        };
        assert!(profile.validate().is_ok());
        assert_eq!(profile.resolve_effort(None).unwrap(), Effort::High);
        assert_eq!(
            profile.resolve_effort(Some(Effort::Max)).unwrap(),
            Effort::Max
        );
        assert!(profile.resolve_effort(Some(Effort::Medium)).is_err());
        let invalid = ModelProfile {
            default_effort: Effort::Low,
            ..profile
        };
        assert!(invalid.validate().is_err());
    }
}
