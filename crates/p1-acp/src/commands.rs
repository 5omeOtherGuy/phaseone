//! Slash commands (#676): the list a session publishes with `available_commands_update`
//! and the `/name argument` prompt that runs one, independent of wire version.
//!
//! `/model` and `/effort` belong to the driver: they change the session's config
//! options through `session/set_config_option`'s own path (#675), so a client sees
//! one setting however it changed it. Every other command is the host's
//! ([`SessionHandle::commands`](p1_contracts::frontend::SessionHandle::commands)).

use p1_contracts::frontend::{CommandInfo, ConfigChoice, ConfigKind};

/// The command that changes `kind`, without its slash.
pub fn name(kind: ConfigKind) -> &'static str {
    match kind {
        ConfigKind::Model => "model",
        ConfigKind::Effort => "effort",
    }
}

/// The driver's own command for a setting the session offers.
fn setting(kind: ConfigKind) -> CommandInfo {
    match kind {
        ConfigKind::Model => CommandInfo {
            name: name(kind).to_string(),
            description: "Switch the model; without an argument, list the models".to_string(),
            hint: Some("ENV/PROFILE".to_string()),
        },
        ConfigKind::Effort => CommandInfo {
            name: name(kind).to_string(),
            description: "Set the reasoning effort; without an argument, list the efforts"
                .to_string(),
            hint: Some("level".to_string()),
        },
    }
}

/// The names the driver keeps for itself, whether or not the session offers them.
const SETTINGS: [ConfigKind; 2] = [ConfigKind::Model, ConfigKind::Effort];

/// The published list: one command per setting the session offers, then the host's
/// commands, each name once. A host command named like one of the driver's is left
/// out.
pub fn list(choices: &[ConfigChoice], host: Vec<CommandInfo>) -> Vec<CommandInfo> {
    let mut commands: Vec<CommandInfo> =
        choices.iter().map(|choice| setting(choice.kind)).collect();
    for command in host {
        if !SETTINGS.iter().any(|kind| name(*kind) == command.name)
            && !commands.iter().any(|known| known.name == command.name)
        {
            commands.push(command);
        }
    }
    commands
}

/// A prompt that runs a command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Invocation {
    /// `/model` or `/effort`: change the setting, or list it without an argument.
    Setting { kind: ConfigKind, argument: String },
    /// One of the host's commands.
    Host { name: String, argument: String },
}

/// The command `text` names, when it names one of `commands`: `/name` alone, or
/// followed by whitespace and its argument. Any other text, an unknown `/x` included,
/// is a prompt for the model.
pub fn parse(text: &str, commands: &[CommandInfo]) -> Option<Invocation> {
    let rest = text.trim_start().strip_prefix('/')?;
    let (word, argument) = match rest.find(char::is_whitespace) {
        Some(at) => (&rest[..at], rest[at..].trim()),
        None => (rest.trim_end(), ""),
    };
    let command = commands.iter().find(|command| command.name == word)?;
    let argument = argument.to_string();
    Some(
        match SETTINGS
            .into_iter()
            .find(|kind| name(*kind) == command.name)
        {
            Some(kind) => Invocation::Setting { kind, argument },
            None => Invocation::Host {
                name: command.name.clone(),
                argument,
            },
        },
    )
}

/// A setting's values as text, the current one marked: what `/model` and `/effort`
/// answer without an argument.
pub fn describe(choice: &ConfigChoice) -> String {
    let mut text = String::new();
    for value in &choice.values {
        let mark = if value.value == choice.current {
            "*"
        } else {
            " "
        };
        text.push_str(&format!("{mark} {}", value.value));
        if let Some(description) = &value.description {
            text.push_str(&format!(" — {description}"));
        }
        text.push('\n');
    }
    text
}
