//! The session's settings for a front end (#675): the model and its effort, listed
//! and changed exactly as the line mode's `/model` and `/effort` do. The models are
//! the host's model table within the run's scope (`--models`, else `enabled_models`);
//! the efforts are the ones the running model's profile lists.

use p1_contracts::Effort;
use p1_contracts::frontend::{ConfigChoice, ConfigKind, ConfigValue};

use crate::HostDeps;
use crate::models::{Model, effort_name, enumerate, in_scope, load_profile, load_settings, scope};
use crate::run::ModelSwitch;

/// The value a session shows while it runs its profile's own effort setting and the
/// profile names none: the request then carries no effort at all.
const DEFAULT_EFFORT: &str = "default";

/// The settings, or none when the session cannot switch (no switch context, or a
/// model the table does not list).
pub(super) fn choices(deps: &HostDeps, switch: &ModelSwitch) -> Result<Vec<ConfigChoice>, String> {
    let (environment, profile, effort) = switch.current_model();
    let models = enumerate(&deps.environment_dirs)?;
    let profile = match profile {
        Some(profile) => profile,
        None => match own_profile(deps, &environment)? {
            Some(profile) => profile,
            None => return Ok(Vec::new()),
        },
    };
    let current = format!("{environment}/{profile}");
    let Some(running) = models.iter().find(|model| model.id() == current) else {
        return Ok(Vec::new());
    };
    let settings = load_settings(&crate::auth::locations(deps))?;
    // A scope that stopped matching (`settings.toml` edited mid-session) offers the
    // running model alone rather than no settings: `/model REF` keeps working too.
    let scope = scope(switch.scope_flag(), &settings, &models).ok();
    let values = models
        .iter()
        .filter(|model| {
            model.id() == current || scope.as_ref().is_some_and(|scope| in_scope(scope, model))
        })
        .map(model_value)
        .collect();
    let mut choices = vec![ConfigChoice {
        kind: ConfigKind::Model,
        current,
        values,
    }];
    if !running.efforts.is_empty() {
        let effort = match effort {
            Some(effort) => Some(effort),
            None => load_profile(&deps.environment_dirs, &profile)?.default_effort,
        };
        choices.push(effort_choice(&running.efforts, effort));
    }
    Ok(choices)
}

/// The profile an environment file names, for a session that selected none.
fn own_profile(deps: &HostDeps, environment: &str) -> Result<Option<String>, String> {
    let file = p1_assembly::load_environment(environment, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    Ok(file.profile.map(|profile| profile.id.clone()))
}

fn model_value(model: &Model) -> ConfigValue {
    ConfigValue {
        value: model.id(),
        name: model.id(),
        description: Some(format!(
            "route {}; efforts {}",
            model.route,
            model.efforts_line()
        )),
    }
}

fn effort_choice(efforts: &[Effort], current: Option<Effort>) -> ConfigChoice {
    let mut values: Vec<ConfigValue> = efforts
        .iter()
        .map(|effort| ConfigValue {
            value: effort_name(*effort).to_string(),
            name: effort_name(*effort).to_string(),
            description: None,
        })
        .collect();
    let current = match current {
        Some(effort) => effort_name(effort).to_string(),
        None => {
            values.insert(
                0,
                ConfigValue {
                    value: DEFAULT_EFFORT.to_string(),
                    name: "Model default".to_string(),
                    description: Some("the request names no effort".to_string()),
                },
            );
            DEFAULT_EFFORT.to_string()
        }
    };
    ConfigChoice {
        kind: ConfigKind::Effort,
        current,
        values,
    }
}

/// Whether `value` is the effort the session already runs without naming one: listed
/// only while it is current, so choosing it changes nothing.
pub(super) fn is_default_effort(kind: ConfigKind, value: &str) -> bool {
    kind == ConfigKind::Effort && value == DEFAULT_EFFORT
}
