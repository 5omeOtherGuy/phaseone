//! ACP v1 slash commands (schema/v1/schema.json `AvailableCommand`,
//! `AvailableCommandsUpdate`): the name without its slash, free-text input only.

use p1_contracts::frontend::CommandInfo;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
struct AvailableCommand<'a> {
    name: &'a str,
    description: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    input: Option<Input<'a>>,
}

#[derive(Serialize)]
struct Input<'a> {
    hint: &'a str,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Update<'a> {
    session_update: &'static str,
    available_commands: Vec<AvailableCommand<'a>>,
}

pub(crate) fn update(commands: &[CommandInfo]) -> Value {
    serde_json::to_value(Update {
        session_update: "available_commands_update",
        available_commands: commands
            .iter()
            .map(|command| AvailableCommand {
                name: &command.name,
                description: &command.description,
                input: command.hint.as_deref().map(|hint| Input { hint }),
            })
            .collect(),
    })
    .expect("wire serialization is infallible")
}
