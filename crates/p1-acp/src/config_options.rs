//! The session's settings as ACP config options (#675): the model and its reasoning
//! effort, each a `select` whose values are what the host's `/model` and `/effort`
//! accept, and the permission mode (#696). The ids and categories are ACP's.

use p1_contracts::frontend::{ConfigChoice, ConfigKind};

/// The option id a client sends in `session/set_config_option`.
pub fn id(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "model",
        ConfigKind::Effort => "thought_level",
        ConfigKind::Mode => "mode",
    }
}

pub(crate) fn name(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "Model",
        ConfigKind::Effort => "Thought level",
        ConfigKind::Mode => "Mode",
    }
}

pub(crate) fn category(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "model",
        ConfigKind::Effort => "thought_level",
        ConfigKind::Mode => "mode",
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

/// `choices` as the session will run them once the changes waiting for the running
/// prompt apply. A waiting model change drops the effort option: the new model's
/// profile decides its efforts, and the update after the switch lists them.
pub fn waiting(
    mut choices: Vec<ConfigChoice>,
    pending: &[(ConfigKind, String)],
) -> Vec<ConfigChoice> {
    for (kind, value) in pending {
        for choice in &mut choices {
            if choice.kind == *kind {
                choice.current = value.clone();
            }
        }
    }
    if pending.iter().any(|(kind, _)| *kind == ConfigKind::Model) {
        choices.retain(|choice| choice.kind != ConfigKind::Effort);
    }
    choices
}

/// Queue `kind = value` behind the running prompt. A model switch resets the effort to
/// the new model's own (as `/model` does), so it replaces a waiting effort change.
pub fn queue(pending: &mut Vec<(ConfigKind, String)>, kind: ConfigKind, value: &str) {
    pending.retain(|(waiting, _)| {
        *waiting != kind && !(kind == ConfigKind::Model && *waiting == ConfigKind::Effort)
    });
    pending.push((kind, value.to_string()));
}
