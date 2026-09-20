//! Model policy consumed by compiled adapters. Route selection never changes it.
use p1_contracts::{Effort, ProviderError, ProviderErrorKind};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ThinkingPolicy {
    Enabled,
    /// Preserve the model's complete reasoning between turns, including tool use.
    Preserved,
}

/// The policy needed by the first composed chat providers (ADR-0039, migration step 2).
/// Environment profile selection and other model families are separate migrations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelProfile {
    pub model_id: String,
    pub thinking: ThinkingPolicy,
    pub efforts: Vec<Effort>,
    pub default_effort: Effort,
    pub max_output_tokens: Option<u32>,
}

impl ModelProfile {
    pub fn validate(&self) -> Result<(), ProviderError> {
        if self.model_id.is_empty()
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
    #[test]
    fn explicit_unsupported_effort_is_never_coerced_to_the_default() {
        let profile = ModelProfile {
            model_id: "example".into(),
            thinking: ThinkingPolicy::Enabled,
            efforts: vec![Effort::High, Effort::Max],
            default_effort: Effort::High,
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
