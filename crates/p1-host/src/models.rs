//! Model selection at start (ADR-0049 stage 1, `docs/design/model-selection.md` §1–§2).
//!
//! A *model* is `<environment>/<profile>`: the environment fixes the prompt family,
//! the tools and `[context]`; its route × the profile fix the provider (ADR-0039).
//! The candidates are exactly the profiles bound in an environment's route file
//! (`[models."P"]`) — there is no free-text model id, and nothing here invents one.
//!
//! The module is data plus a little string work: the glob is hand-written (`*` and
//! `?` only), `settings.toml` is parsed with the same TOML engine the route and
//! profile files use, and no credential is ever read — the credential column comes
//! from `catalog::credential_line_for_route`, the same probe `p1 env show` prints.

use std::ffi::OsStr;
use std::path::PathBuf;
use std::sync::Arc;

use p1_assembly::{EnvironmentFile, load_environment};
use p1_auth::Locations;
use p1_contracts::Effort;
use p1_model_profile::ModelProfile;
use serde::Deserialize;

use crate::cli::DEFAULT_ENV;

/// The effort levels, spelled exactly as `Effort` serialises them — the spelling a
/// profile file lists and `--effort`/`:effort` accepts.
pub const EFFORT_LEVELS: [&str; 5] = ["low", "medium", "high", "extra_high", "max"];

/// Parse one effort level. An unknown spelling is an error naming every level.
pub fn parse_effort(text: &str) -> Result<Effort, String> {
    match text {
        "low" => Ok(Effort::Low),
        "medium" => Ok(Effort::Medium),
        "high" => Ok(Effort::High),
        "extra_high" => Ok(Effort::ExtraHigh),
        "max" => Ok(Effort::Max),
        other => Err(format!(
            "unknown effort `{other}`; the levels are {}",
            EFFORT_LEVELS.join(", ")
        )),
    }
}

/// The spelling of one effort level, for `p1 models`, `p1 env show` and the errors.
pub fn effort_name(effort: Effort) -> &'static str {
    match effort {
        Effort::Low => "low",
        Effort::Medium => "medium",
        Effort::High => "high",
        Effort::ExtraHigh => "extra_high",
        Effort::Max => "max",
    }
}

/// One selectable model: an environment, the profile bound in its route file, that
/// route's id and the efforts the profile lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub environment: String,
    pub profile: String,
    pub route: String,
    pub efforts: Vec<Effort>,
    /// The definitions (`environment`, `route`, `profile`, `account`) whose file
    /// shadows a later one of the same id (ADR-0139 §4), marked in the table.
    pub shadows: Vec<String>,
}

impl Model {
    /// The reference the operator writes: `E/P`.
    pub fn id(&self) -> String {
        format!("{}/{}", self.environment, self.profile)
    }

    /// The efforts as one column: joined with `,`, or `-` when the profile lists none.
    pub fn efforts_line(&self) -> String {
        if self.efforts.is_empty() {
            "-".to_string()
        } else {
            self.efforts
                .iter()
                .map(|effort| effort_name(*effort))
                .collect::<Vec<_>>()
                .join(",")
        }
    }
}

/// A reference resolved to a pair and, when it carried one, an effort.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resolved {
    pub environment: String,
    pub profile: String,
    pub effort: Option<Effort>,
    /// The account a trailing `@account` named (ADR-0139 §3 rule 2).
    pub account: Option<String>,
}

/// What a command line selects: the environment to load, the profile to apply on
/// top of it (`None` keeps the environment's own) and the effort to apply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Choice {
    pub environment: String,
    pub profile: Option<String>,
    pub effort: Option<Effort>,
    /// The account to run the environment's route with (ADR-0139 §3 rules 1–2); `None`
    /// keeps the environment's own.
    pub account: Option<String>,
}

/// `settings.toml` (spec §2): `default_model` replaces the default environment and
/// `enabled_models` is the scope. An unknown key is rejected by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Settings {
    #[serde(default)]
    pub default_model: Option<String>,
    #[serde(default)]
    pub enabled_models: Vec<String>,
    /// Global standing instructions; absent means `~/.agents/AGENTS.md`.
    #[serde(default)]
    pub instructions_global: Option<String>,
    /// `[workflows]` (ADR-0053 item 4): laid over the shipped roles and caps.
    #[cfg(feature = "workflows")]
    #[serde(default)]
    pub workflows: Option<p1_workflow::WorkflowSettings>,
    #[serde(default)]
    pub shadow: Option<ShadowSettings>,
    /// `[capabilities]` (ADR-0085 item 6): the worker and workflow families' run-time
    /// switch, both enabled when absent. S6's type, read by `catalog/delegation.rs`.
    ///
    /// notice: S1's file; S6.8 adds this one `Settings` field and nothing else here
    /// (D065/D068 pattern, ADR-0085 item 6).
    #[serde(default)]
    pub capabilities: crate::catalog::delegation::Capabilities,
}

/// The optional brain shadow binary in `[shadow]`.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShadowSettings {
    pub brain_packet_shadow: Option<PathBuf>,
}

// ------------------------------------------------------------------ settings

/// `settings.toml` lives next to the p1 store (`auth.json`): `$XDG_CONFIG_HOME/p1`
/// else `~/.config/p1`. A blank variable counts as unset, exactly as the credential
/// locations read it. `None` when the host has neither.
pub fn settings_path(locations: &Locations) -> Option<PathBuf> {
    let config = dir(locations.env("XDG_CONFIG_HOME"))
        .or_else(|| dir(locations.env("HOME")).map(|home| home.join(".config")))?;
    Some(config.join("p1").join("settings.toml"))
}

/// Parse `settings.toml`. An absent file is the empty settings; every other failure
/// names the file, and an unknown key names the key.
pub fn load_settings(locations: &Locations) -> Result<Settings, String> {
    let Some(path) = settings_path(locations) else {
        return Ok(Settings::default());
    };
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Settings::default());
        }
        Err(error) => return Err(format!("{}: {error}", path.display())),
    };
    toml::from_str(&text).map_err(|error| format!("{}: {error}", path.display()))
}

/// The effective workflow settings: the user's `[workflows]` table over the shipped
/// defaults, the shipped defaults alone when the table is absent.
#[cfg(feature = "workflows")]
pub fn workflow_settings(locations: &Locations) -> Result<p1_workflow::WorkflowSettings, String> {
    let shipped = p1_workflow::WorkflowSettings::shipped();
    Ok(match load_settings(locations)?.workflows {
        Some(user) => shipped.overridden_by(user),
        None => shipped,
    })
}

/// Where workflow runs keep their directories when no session file places them
/// (ADR-0053 item 7): `$XDG_STATE_HOME/p1/workflows` else
/// `~/.local/state/p1/workflows`, read like `settings_path`. `None` when the host has
/// neither.
#[cfg(feature = "workflows")]
pub fn workflow_state_root(locations: &Locations) -> Option<PathBuf> {
    let state = dir(locations.env("XDG_STATE_HOME"))
        .or_else(|| dir(locations.env("HOME")).map(|home| home.join(".local").join("state")))?;
    Some(state.join("p1").join("workflows"))
}

fn dir(value: Option<String>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}

// ------------------------------------------------------------------ the catalog

/// Every model: every environment directory entry × every profile bound in that
/// environment's route file, sorted by environment then profile.
///
/// An environment that names no route (a whole-provider file) contributes nothing:
/// it has no route and therefore no profiles. A broken environment, route or profile
/// file is an error — a partial list would silently hide a model.
pub fn enumerate(environment_dirs: &[PathBuf]) -> Result<Vec<Model>, String> {
    let mut models = Vec::new();
    for environment in environment_names(environment_dirs)? {
        // These companion environments belong to fixed-model subagent tools, not
        // the interactive model catalog. Do not make existing bare names ambiguous.
        if crate::catalog::delegation::SUBAGENT_MODULES
            .iter()
            .any(|module| crate::catalog::delegation::lock_key(module) == environment)
        {
            continue;
        }
        let loaded = load_environment(&environment, environment_dirs).map_err(|e| e.to_string())?;
        if loaded.profile.is_none() {
            continue;
        }
        let route = crate::routes::load_route_by_id(environment_dirs, &loaded.provider)?;
        for profile_id in route.models.keys() {
            let profile = load_profile(environment_dirs, profile_id)?;
            let shadows =
                crate::routes::definitions(environment_dirs, &environment, &route, profile_id)
                    .into_iter()
                    .filter(|(_, files)| files.len() > 1)
                    .map(|(kind, _)| kind.to_string())
                    .collect();
            models.push(Model {
                environment: environment.clone(),
                profile: profile_id.clone(),
                // ADR-0139 §5: the account column rides on the route's, as
                // `<route>@<account>` whenever the account is not the route's own.
                route: if route.implicit {
                    route.id.clone()
                } else {
                    format!("{}@{}", route.route, route.account)
                },
                efforts: profile.efforts.clone(),
                shadows,
            });
        }
    }
    models.sort_by(|left, right| {
        left.environment
            .cmp(&right.environment)
            .then_with(|| left.profile.cmp(&right.profile))
    });
    Ok(models)
}

/// The environment names the directories hold, highest priority first, sorted. A
/// name found in more than one directory resolves to the first one, exactly like
/// `load_environment` and `load_all_routes`. Also the `worker_start` schema's
/// environment enum (`catalog.rs`).
pub(crate) fn environment_names(environment_dirs: &[PathBuf]) -> Result<Vec<String>, String> {
    let mut names: Vec<String> = Vec::new();
    for dir in environment_dirs {
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => {
                return Err(format!(
                    "cannot read the environment directory {}: {error}",
                    dir.display()
                ));
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.join("environment.toml").is_file() {
                continue;
            }
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !names.iter().any(|seen| seen == name) {
                names.push(name.to_string());
            }
        }
    }
    names.sort();
    Ok(names)
}

/// `<dir>/../profiles`, for each environments directory, highest priority first —
/// the same rule `p1-assembly` uses for the profile an environment names.
fn profiles_dirs(environment_dirs: &[PathBuf]) -> Vec<PathBuf> {
    environment_dirs
        .iter()
        .map(|dir| dir.join("../profiles"))
        .collect()
}

/// Load one `profiles/<id>.toml`, in the same search order the environments use. A
/// missing file names the profiles that exist, like `p1-assembly`'s lookup does.
pub fn load_profile(environment_dirs: &[PathBuf], id: &str) -> Result<Arc<ModelProfile>, String> {
    load_profile_with_text(environment_dirs, id).map(|(profile, _)| profile)
}

fn load_profile_with_text(
    environment_dirs: &[PathBuf],
    id: &str,
) -> Result<(Arc<ModelProfile>, String), String> {
    let dirs = profiles_dirs(environment_dirs);
    for dir in &dirs {
        let path = dir.join(format!("{id}.toml"));
        if !path.is_file() {
            continue;
        }
        let text = p1_assembly::read_configuration(&path)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        let profile = ModelProfile::from_toml(id, &text)
            .map_err(|error| format!("{}: {error}", path.display()))?;
        return Ok((Arc::new(profile), text));
    }
    let searched = dirs
        .iter()
        .map(|dir| dir.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "profile `{id}` was not found in {searched}; available: {:?}",
        available_profiles(&dirs)
    ))
}

/// The profile ids the directories hold, sorted and deduplicated. Unreadable
/// directories contribute nothing: this only decorates an error that already fired.
fn available_profiles(dirs: &[PathBuf]) -> Vec<String> {
    let mut ids: Vec<String> = dirs
        .iter()
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension() == Some(OsStr::new("toml")))
        .filter_map(|path| path.file_stem().and_then(OsStr::to_str).map(str::to_string))
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

// ------------------------------------------------------------------ resolution

/// Resolve a model reference (spec §1): `E/P`, or a bare `P` preferring
/// `current_environment`, optionally `:effort`, then optionally `@account` (ADR-0139
/// §3). Every failure is one sentence that lists the candidates — never a guess.
pub fn resolve(
    reference: &str,
    current_environment: &str,
    models: &[Model],
) -> Result<Resolved, String> {
    let (model, account) = split_account(reference)?;
    let (pair, effort) = match model.split_once(':') {
        Some((pair, effort)) => (pair, Some(parse_effort(effort)?)),
        None => (model, None),
    };
    let (environment, profile) = match pair.split_once('/') {
        Some((environment, profile)) if !environment.is_empty() && !profile.is_empty() => {
            match models
                .iter()
                .find(|model| model.environment == environment && model.profile == profile)
            {
                Some(model) => (model.environment.clone(), model.profile.clone()),
                None => {
                    let near: Vec<&Model> = models
                        .iter()
                        .filter(|model| {
                            model.environment == environment || model.profile == profile
                        })
                        .collect();
                    let list = if near.is_empty() {
                        list(models.iter())
                    } else {
                        list(near.into_iter())
                    };
                    return Err(format!("unknown model `{pair}`; the models are: {list}"));
                }
            }
        }
        Some(_) => {
            return Err(format!(
                "`{reference}` is not a model reference; write `environment/profile`, with an \
                 optional `:effort`"
            ));
        }
        None => {
            let candidates: Vec<&Model> = models
                .iter()
                .filter(|model| model.profile == pair)
                .collect();
            if candidates.is_empty() {
                return Err(format!(
                    "unknown model `{pair}`; the models are: {}",
                    list(models.iter())
                ));
            }
            let chosen = candidates
                .iter()
                .copied()
                .find(|model| model.environment == current_environment)
                .or_else(|| {
                    // Exactly one candidate: no preference is needed, and there is
                    // nothing to guess.
                    (candidates.len() == 1).then(|| candidates[0])
                });
            match chosen {
                Some(model) => (model.environment.clone(), model.profile.clone()),
                None => {
                    return Err(format!(
                        "`{pair}` is bound in more than one environment: {}; write \
                         `environment/profile`",
                        list(candidates.into_iter())
                    ));
                }
            }
        }
    };
    Ok(Resolved {
        environment,
        profile,
        effort,
        account,
    })
}

/// A model reference without its trailing `@account`, and that account (ADR-0139 §3
/// rule 2). An account part that is not an account id is an error.
pub fn split_account(reference: &str) -> Result<(&str, Option<String>), String> {
    match reference.rsplit_once('@') {
        Some((model, account)) if p1_assembly::is_account_id(account) => {
            Ok((model, Some(account.to_string())))
        }
        Some((_, account)) => Err(format!(
            "`{reference}` names the account `{account}`, which is not an account id \
             (letters, digits, `-`, `_` and `.`); write `environment/profile[:effort][@account]`"
        )),
        None => Ok((reference, None)),
    }
}

/// The models of a list as `E/P`, joined for an error message.
fn list<'a>(models: impl Iterator<Item = &'a Model>) -> String {
    models.map(Model::id).collect::<Vec<_>>().join(", ")
}

/// The model a bare `p1` would run: `settings.default_model` when set, else the
/// default environment with its own profile. `None` when that environment is not
/// there or names no profile.
pub fn default_model(
    settings: &Settings,
    environment_dirs: &[PathBuf],
    models: &[Model],
) -> Result<Option<String>, String> {
    if let Some(reference) = &settings.default_model {
        let resolved = resolve(reference, DEFAULT_ENV, models)?;
        return Ok(Some(format!(
            "{}/{}",
            resolved.environment, resolved.profile
        )));
    }
    let Ok(environment) = load_environment(DEFAULT_ENV, environment_dirs) else {
        return Ok(None);
    };
    let Some(profile) = environment.profile else {
        return Ok(None);
    };
    let id = format!("{DEFAULT_ENV}/{}", profile.id);
    Ok(models.iter().any(|model| model.id() == id).then_some(id))
}

/// The model selection a command line asks for (spec §2): `--model`, else `--env`,
/// else `settings.toml`'s `default_model`, else the default environment.
///
/// `settings.toml` is read only when neither `--env` nor `--model` was given: that
/// is the one case the spec lets `default_model` decide.
pub fn choose(
    environment_dirs: &[PathBuf],
    locations: &Locations,
    explicit_env: Option<&str>,
    model: Option<&str>,
    effort: Option<Effort>,
    account: Option<&str>,
) -> Result<Choice, String> {
    // `--account` applies to the run's own selection, whichever rule picks it
    // (ADR-0139 §3 rule 1); a `--model` naming another account contradicts it.
    let account = |reference: &str, named: Option<String>| match (account, named) {
        (Some(flag), Some(named)) if flag != named => Err(format!(
            "--account `{flag}` and --model `{reference}` name different accounts"
        )),
        (Some(flag), _) => Ok(Some(flag.to_string())),
        (None, named) => Ok(named),
    };
    if let Some(reference) = model {
        let models = enumerate(environment_dirs)?;
        let resolved = resolve(reference, explicit_env.unwrap_or(DEFAULT_ENV), &models)?;
        if let Some(environment) = explicit_env
            && environment != resolved.environment
        {
            return Err(format!(
                "--env `{environment}` and --model `{reference}` name different environments: \
                 the model is `{}/{}`",
                resolved.environment, resolved.profile
            ));
        }
        return Ok(Choice {
            account: account(reference, resolved.account)?,
            environment: resolved.environment,
            profile: Some(resolved.profile),
            effort: effort.or(resolved.effort),
        });
    }
    if let Some(environment) = explicit_env {
        return Ok(Choice {
            environment: environment.to_string(),
            profile: None,
            effort,
            account: account(environment, None)?,
        });
    }
    if let Some(reference) = &load_settings(locations)?.default_model {
        let models = enumerate(environment_dirs)?;
        let resolved = resolve(reference, DEFAULT_ENV, &models)?;
        return Ok(Choice {
            // `settings.toml` keeps its own `@account`; `--account` replaces it.
            account: account(reference, None)?.or(resolved.account),
            environment: resolved.environment,
            profile: Some(resolved.profile),
            effort: effort.or(resolved.effort),
        });
    }
    Ok(Choice {
        environment: DEFAULT_ENV.to_string(),
        profile: None,
        effort,
        account: account(DEFAULT_ENV, None)?,
    })
}

/// Apply a selection to a loaded environment (spec §1 rule 3, §2): the profile
/// becomes `P`, when an effort was selected `[options] reasoning_effort` is replaced,
/// and when an account was selected the environment's route runs with it (ADR-0139
/// §3): the provider key becomes `<route>@<account>`, refused here when the account
/// does not declare the route's origin. Nothing else of the environment changes — the
/// route binding and the wire model still come from the unchanged resolution path.
pub fn apply(
    environment: &mut EnvironmentFile,
    choice: &Choice,
    environment_dirs: &[PathBuf],
) -> Result<(), String> {
    if let Some(account) = &choice.account {
        if environment.profile.is_none() {
            return Err(format!(
                "environment `{}` names a whole provider, not a route, so it takes no account",
                environment.name
            ));
        }
        // The route the environment means, by its own file id: a legacy route id
        // (ADR-0139 §6) names its route with one account, which this choice replaces.
        let route = crate::routes::load_route_by_id(environment_dirs, &environment.provider)
            .map(|bound| bound.route)
            .unwrap_or_else(|_| {
                environment
                    .provider
                    .split_once('@')
                    .map_or(environment.provider.clone(), |(route, _)| route.to_string())
            });
        let key = format!("{route}@{account}");
        crate::routes::load_route_by_id(environment_dirs, &key).map_err(|error| {
            format!(
                "environment `{}` cannot run with account `{account}`: {error}",
                environment.name
            )
        })?;
        environment.provider = key;
    }
    if let Some(profile_id) = &choice.profile {
        let (profile, text) = load_profile_with_text(environment_dirs, profile_id)?;
        environment.model = profile.model_id.clone();
        environment.family = profile.family.clone();
        environment.profile = Some(profile);
        environment.profile_text = Some(text);
    }
    match choice.effort {
        Some(effort) => {
            let Some(profile) = &environment.profile else {
                return Err(format!(
                    "environment `{}` names no model profile, so it takes no effort",
                    environment.name
                ));
            };
            if !profile.efforts.contains(&effort) {
                return Err(unsupported_effort(profile, effort));
            }
            environment.options.reasoning_effort = Some(effort);
        }
        None => {
            // An effort-less selection keeps the environment's own effort when the
            // profile supports it, else the profile's default (§1 rule 3).
            if let Some(profile) = &environment.profile
                && let Some(current) = environment.options.reasoning_effort
                && !profile.efforts.contains(&current)
            {
                environment.options.reasoning_effort = profile.default_effort;
            }
        }
    }
    Ok(())
}

/// The error for an effort a profile does not list: it names the profile's own
/// efforts, so the operator can pick one.
fn unsupported_effort(profile: &ModelProfile, effort: Effort) -> String {
    let listed = if profile.efforts.is_empty() {
        "none".to_string()
    } else {
        profile
            .efforts
            .iter()
            .map(|listed| effort_name(*listed))
            .collect::<Vec<_>>()
            .join(", ")
    };
    format!(
        "profile `{}` does not list effort `{}`; it lists {listed}",
        profile.id,
        effort_name(effort)
    )
}

// ------------------------------------------------------------------ the scope

/// The active scope (spec §2): `--models` when given, else `enabled_models`. Every
/// pattern must match at least one model — a typo must not silently shrink the
/// scope. An empty scope means every model.
pub fn scope(
    flag: Option<&str>,
    settings: &Settings,
    models: &[Model],
) -> Result<Vec<String>, String> {
    match flag {
        Some(flag) => check_scope(flag, models),
        None => {
            validate(
                &settings.enabled_models,
                "settings.toml `enabled_models`",
                models,
            )?;
            Ok(settings.enabled_models.clone())
        }
    }
}

/// The patterns of one `--models` value, validated: a run checks them too, so a
/// typo fails wherever the scope would have been used.
pub fn check_scope(flag: &str, models: &[Model]) -> Result<Vec<String>, String> {
    let patterns = split_patterns(flag);
    validate(&patterns, "--models", models)?;
    Ok(patterns)
}

/// Every pattern must match at least one model; the error names the source of the
/// pattern and lists what there is to match.
fn validate(patterns: &[String], source: &str, models: &[Model]) -> Result<(), String> {
    for pattern in patterns {
        if !models.iter().any(|model| matches_pattern(pattern, model)) {
            return Err(format!(
                "{source} pattern `{pattern}` matches no model; the models are: {}",
                list(models.iter())
            ));
        }
    }
    Ok(())
}

/// A comma-separated `--models` value: trimmed, empty parts dropped, so a trailing
/// comma is not a pattern that matches nothing.
fn split_patterns(text: &str) -> Vec<String> {
    text.split(',')
        .map(str::trim)
        .filter(|pattern| !pattern.is_empty())
        .map(str::to_string)
        .collect()
}

/// One pattern against one model: a pattern with a `/` is matched against `E/P`,
/// a pattern without one against the profile part only.
fn matches_pattern(pattern: &str, model: &Model) -> bool {
    if pattern.contains('/') {
        glob(pattern, &model.id())
    } else {
        glob(pattern, &model.profile)
    }
}

/// Whether a model is in the scope. An empty scope holds every model.
pub fn in_scope(patterns: &[String], model: &Model) -> bool {
    patterns.is_empty()
        || patterns
            .iter()
            .any(|pattern| matches_pattern(pattern, model))
}

/// A glob over `*` (any run of characters, including none) and `?` (exactly one).
/// Anchored at both ends: a pattern either matches the whole reference or nothing.
pub fn glob(pattern: &str, text: &str) -> bool {
    let pattern: Vec<char> = pattern.chars().collect();
    let text: Vec<char> = text.chars().collect();
    let (mut p, mut t) = (0usize, 0usize);
    // The last `*` seen and how much of the text it had consumed when it was passed.
    let mut star: Option<usize> = None;
    let mut mark = 0usize;
    while t < text.len() {
        if p < pattern.len() && (pattern[p] == '?' || pattern[p] == text[t]) {
            p += 1;
            t += 1;
        } else if p < pattern.len() && pattern[p] == '*' {
            star = Some(p);
            mark = t;
            p += 1;
        } else if let Some(star) = star {
            p = star + 1;
            mark += 1;
            t = mark;
        } else {
            return false;
        }
    }
    pattern[p..].iter().all(|c| *c == '*')
}

// ------------------------------------------------------------------ the table

/// The `p1 models` table (spec §2): one row per model, columns aligned. `default`
/// marks the model a bare `p1` would run, `scoped` the models in the active scope —
/// and `scoped` is omitted entirely when the scope is empty. `credential` reports
/// the source the way `p1 env show` does, never a value.
pub fn table(
    models: &[Model],
    scope: &[String],
    default: Option<&str>,
    credential: impl Fn(&str) -> Result<String, String>,
) -> Result<String, String> {
    let mut rows: Vec<(String, String, String, String)> = Vec::new();
    for model in models {
        rows.push((
            model.id(),
            model.route.clone(),
            model.efforts_line(),
            credential(&model.route)?,
        ));
    }
    // One width per column, so the four columns line up under each other.
    let widths = [0, 1, 2, 3].map(|column| {
        rows.iter()
            .map(|row| [&row.0, &row.1, &row.2, &row.3][column].chars().count())
            .max()
            .unwrap_or(0)
    });
    let [w0, w1, w2, w3] = widths;
    let mut out = String::new();
    for (index, (id, route, efforts, credential)) in rows.iter().enumerate() {
        let mut line = format!("{id:<w0$}  {route:<w1$}  {efforts:<w2$}  {credential:<w3$}");
        let mut markers: Vec<&str> = Vec::new();
        if default == Some(id.as_str()) {
            markers.push("default");
        }
        if !scope.is_empty() && in_scope(scope, &models[index]) {
            markers.push("scoped");
        }
        let shadows = format!("shadows:{}", models[index].shadows.join(","));
        if !models[index].shadows.is_empty() {
            markers.push(&shadows);
        }
        if !markers.is_empty() {
            line.push_str("  ");
            line.push_str(&markers.join(" "));
        }
        out.push_str(line.trim_end());
        out.push('\n');
    }
    Ok(out)
}

/// The `p1 models` rows for one optional search: a case-insensitive substring of
/// `E/P`. `None` keeps every model.
pub fn search<'a>(models: &'a [Model], needle: Option<&str>) -> Vec<&'a Model> {
    let Some(needle) = needle else {
        return models.iter().collect();
    };
    let needle = needle.to_lowercase();
    models
        .iter()
        .filter(|model| model.id().to_lowercase().contains(&needle))
        .collect()
}
