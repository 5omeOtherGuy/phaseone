//! The session's settings as ACP config options (#675): the model and its reasoning
//! effort, each a `select` whose values are what the host's `/model` and `/effort`
//! accept. The ids and categories are ACP's; a later setting (the permission mode,
//! #696) adds one arm to each function.

use p1_contracts::frontend::{ConfigChoice, ConfigKind};

/// The option id a client sends in `session/set_config_option`.
pub fn id(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "model",
        ConfigKind::Effort => "thought_level",
    }
}

pub(crate) fn name(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "Model",
        ConfigKind::Effort => "Thought level",
    }
}

pub(crate) fn category(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "model",
        ConfigKind::Effort => "thought_level",
    }
}

/// Why a change request is refused before it reaches the session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
    UnknownOption(String),
    UnknownValue { option: String, value: String },
}

impl std::fmt::Display for Refusal {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::UnknownOption(option) => write!(f, "unknown config option `{option}`"),
            Self::UnknownValue { option, value } => {
                write!(f, "`{value}` is not a value of config option `{option}`")
            }
        }
    }
}

/// The setting `option` names, if `value` is one the session offers now.
pub fn validate(
    choices: &[ConfigChoice],
    option: &str,
    value: &str,
) -> Result<ConfigKind, Refusal> {
    let choice = choices
        .iter()
        .find(|choice| id(choice.kind) == option)
        .ok_or_else(|| Refusal::UnknownOption(option.to_string()))?;
    if choice.values.iter().any(|offered| offered.value == value) {
        Ok(choice.kind)
    } else {
        Err(Refusal::UnknownValue {
            option: option.to_string(),
            value: value.to_string(),
        })
    }
}

/// `choices` with `kind`'s current value replaced: what the session will run once a
/// change waiting for the running prompt applies.
pub fn with_current(
    mut choices: Vec<ConfigChoice>,
    kind: ConfigKind,
    value: &str,
) -> Vec<ConfigChoice> {
    for choice in &mut choices {
        if choice.kind == kind {
            choice.current = value.to_string();
        }
    }
    choices
}
