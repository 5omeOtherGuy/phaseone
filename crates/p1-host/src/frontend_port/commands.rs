//! The host's slash commands for a front end (#676): the line mode's and the frozen
//! TUI's `/compact`, `/status`, `/access` and `/modules reload`, and one command per
//! skill the session's environment lists. `/model` and `/effort` are the front end's,
//! over the session's settings (`config.rs`). The list is closed: the host builds it.

use p1_contracts::frontend::{CommandInfo, CommandOutput, ConfigKind};

use crate::HostDeps;
use crate::cli::SandboxMode;
use crate::run::ModelSwitch;

/// The module a skill-loading tool is assembled from.
const SKILL_MODULE: &str = "skill";

fn command(name: &str, description: &str, hint: Option<&str>) -> CommandInfo {
    CommandInfo {
        name: name.to_string(),
        description: description.to_string(),
        hint: hint.map(str::to_string),
    }
}

/// The commands the session serves now.
pub(super) fn list(deps: &HostDeps) -> Vec<CommandInfo> {
    let mut commands = vec![command(
        "compact",
        "Summarize the session's history now",
        None,
    )];
    let Some(switch) = &deps.model_switch else {
        return commands;
    };
    commands.extend([
        command(
            "status",
            "Show the environment, model, route, effort, access and sandbox",
            None,
        ),
        command(
            "access",
            "Show the access mode and sandbox, fixed per process",
            None,
        ),
        command("modules", "Reload the installed modules", Some("reload")),
    ]);
    match skills(deps, switch) {
        Ok(skills) => {
            for (name, description) in skills {
                if !commands.iter().any(|known| known.name == name) {
                    commands.push(command(&name, &description, Some("what to do (optional)")));
                }
            }
        }
        Err(reason) => crate::run::write_stderr(deps, &format!("· no skill commands: {reason}\n")),
    }
    commands
}

/// The skills of the session's environment, by name and description, when it
/// assembles a skill tool: the listing that tool gives the model.
fn skills(deps: &HostDeps, switch: &ModelSwitch) -> Result<Vec<(String, String)>, String> {
    let (environment, _, _) = switch.current_model();
    let file = p1_assembly::load_environment(&environment, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    if !file.tools.iter().any(|tool| tool.module == SKILL_MODULE) {
        return Ok(Vec::new());
    }
    let (_, _, workspace) = switch.access();
    let source = p1_skill_fs::FilesystemSkills::discover(
        workspace,
        deps.home.as_deref(),
        &file.skills.roots,
        &crate::auth::locations(deps).credential_paths(),
    );
    Ok(p1_contracts::skill::SkillSource::list(&source)
        .skills
        .into_iter()
        // A command name is one word.
        .filter(|skill| !skill.name.is_empty() && !skill.name.contains(char::is_whitespace))
        .map(|skill| (skill.name, skill.description))
        .collect())
}

/// `/NAME [what to do]` for a skill: a turn that asks the model to load it, through
/// the environment's own skill tool, and follow it.
pub(super) fn skill(
    deps: &HostDeps,
    switch: &ModelSwitch,
    name: &str,
    argument: &str,
) -> Result<CommandOutput, String> {
    if !skills(deps, switch)?.iter().any(|(skill, _)| skill == name) {
        return Err(format!("/{name} is not a command of this session"));
    }
    let (environment, _, _) = switch.current_model();
    let file = p1_assembly::load_environment(&environment, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    let tool = file
        .tools
        .iter()
        .find(|tool| tool.module == SKILL_MODULE)
        .and_then(|tool| tool.name.clone())
        .unwrap_or_else(|| SKILL_MODULE.to_string());
    let mut text = format!("Load the skill `{name}` with the `{tool}` tool and follow it.");
    if !argument.is_empty() {
        text.push_str("\n\n");
        text.push_str(argument);
    }
    Ok(CommandOutput::Prompt(text))
}

fn access_mode(ask: bool) -> &'static str {
    if ask {
        "ask · every tool prompts"
    } else {
        "full · every tool runs"
    }
}

fn sandbox_name(sandbox: SandboxMode) -> &'static str {
    match sandbox {
        SandboxMode::Off => "off",
        SandboxMode::Workspace => "workspace",
    }
}

/// `/status`: the facts the frozen TUI's `/status` shows, one line each.
pub(super) fn status(deps: &HostDeps, switch: &ModelSwitch) -> Result<String, String> {
    let (environment, _, _) = switch.current_model();
    let choices = super::config::choices(deps, switch)?;
    let current = |kind| {
        choices
            .iter()
            .find(|choice| choice.kind == kind)
            .map(|choice| choice.current.clone())
    };
    let model = current(ConfigKind::Model);
    let route = model.as_deref().and_then(|model| {
        crate::models::enumerate(&deps.environment_dirs)
            .ok()?
            .into_iter()
            .find(|known| known.id() == model)
            .map(|known| known.route)
    });
    let (ask, sandbox, workspace) = switch.access();
    let unknown = || "unknown".to_string();
    Ok(format!(
        "environment  {environment}\nmodel        {}\nroute        {}\neffort       {}\naccess       {}\nsandbox      {}\nworkspace    {}\n",
        model.unwrap_or_else(unknown),
        route.unwrap_or_else(unknown),
        current(ConfigKind::Effort).unwrap_or_else(|| "default".to_string()),
        access_mode(ask),
        sandbox_name(sandbox),
        workspace.display(),
    ))
}

/// `/access`: the policy facts; `--ask` and `--sandbox` restart to change (ADR-0038).
pub(super) fn access(switch: &ModelSwitch) -> String {
    let (ask, sandbox, _) = switch.access();
    format!(
        "mode     {}\nsandbox  {}\nchange   restart p1 acp with --ask or --sandbox to change it\n",
        access_mode(ask),
        sandbox_name(sandbox),
    )
}
