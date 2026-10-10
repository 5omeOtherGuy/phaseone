//! ACP v1 session config options (schema/v1/schema.json `SessionConfigOption`,
//! `ConfigOptionUpdate`): `select` options only, as p1 offers no boolean option.

use crate::config_options::{category, id, name};
use p1_contracts::frontend::ConfigChoice;
use serde::Serialize;
use serde_json::Value;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct ConfigOption<'a> {
    id: &'static str,
    name: &'static str,
    category: &'static str,
    r#type: &'static str,
    current_value: &'a str,
    options: Vec<OptionValue<'a>>,
}

#[derive(Serialize)]
struct OptionValue<'a> {
    value: &'a str,
    name: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Update {
    session_update: &'static str,
    config_options: Value,
}

pub(crate) fn options(choices: &[ConfigChoice]) -> Value {
    let options: Vec<ConfigOption<'_>> = choices
        .iter()
        .map(|choice| ConfigOption {
            id: id(choice.kind),
            name: name(choice.kind),
            category: category(choice.kind),
            r#type: "select",
            current_value: &choice.current,
            options: choice
                .values
                .iter()
                .map(|value| OptionValue {
                    value: &value.value,
                    name: &value.name,
                    description: value.description.as_deref(),
                })
                .collect(),
        })
        .collect();
    serde_json::to_value(options).expect("wire serialization is infallible")
}

pub(crate) fn update(choices: &[ConfigChoice]) -> Value {
    serde_json::to_value(Update {
        session_update: "config_option_update",
        config_options: options(choices),
    })
    .expect("wire serialization is infallible")
}
