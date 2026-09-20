//! Environment assembly: an environment file becomes a validated
//! [`ResolvedEnvironment`] plus the provider, tools, prompt and options one agent
//! is built from.
//!
//! This crate names no concrete provider or tool. Everything it can build comes
//! from a [`Catalog`] of closures supplied by the composition root (`p1-host`);
//! an environment file can therefore never select a module that was not compiled
//! into the binary. `assemble` never runs a model and never touches the network.
//!
//! # Prompt substitution
//!
//! The only templating is `{{…}}`, and only these placeholders exist:
//!
//! - `{{workspace}}`, `{{date}}`, `{{os}}` — the [`Substitutions`] values verbatim;
//! - `{{tool_names}}` — the assembled model-facing names joined with `", "`, in
//!   environment-file order;
//! - `{{tool:<module>}}` — the model-facing name the module was assembled under.
//!
//! Any other placeholder is an error, and so is `{{tool:<module>}}` for a module
//! the environment does not assemble: a prompt can never mention a tool the agent
//! does not have. Substituted values are inserted verbatim and are NOT rescanned.
//! There is no escape sequence, so a literal `{{` cannot be written in a prompt in
//! this slice — it would be scanned as the start of a placeholder.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::{
    Effort, ModelOptions, Provider, ProviderError, ProviderRequest, RouteDescription, Tool,
    ToolDeclaration, ToolIdentity,
};
use p1_workspace::{ObservedFiles, Workspace, WriteGate};
use serde::{Deserialize, Serialize};

/// File name of an environment definition inside `<dir>/<name>/`.
const ENVIRONMENT_FILE: &str = "environment.toml";
/// Prompt file name inside an environment directory. Not configurable in this slice.
const PROMPT_FILE: &str = "prompt.md";

// ------------------------------------------------------------------ public API

/// One environment file, parsed. `description` is filled in from
/// `description_file` by [`load_environment`]. `prompt_template` is the raw text
/// of `prompt.md` (substituted by [`assemble`]).
#[derive(Debug, Clone)]
pub struct EnvironmentFile {
    pub name: String,
    pub family: String,
    pub provider: String,
    pub model: String,
    pub options: ModelOptions,
    pub tools: Vec<ToolSpec>,
    pub prompt_template: String,
}

/// One `[[tools]]` entry: a catalog key plus the optional per-agent face override.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub module: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub variant: Option<String>,
}

/// What the provider factory is given: the catalog key and the requested model.
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub key: String,
    pub model: String,
}

/// What tools of ONE agent share. Created fresh per [`assemble`] call, so two
/// agents never share read-state.
#[derive(Clone)]
pub struct ToolServices {
    pub workspace: Workspace,
    pub observed: ObservedFiles,
}

/// Builds one provider instance. `Err` is a human-readable reason.
pub type ProviderFactory =
    Box<dyn Fn(&ProviderSpec) -> Result<Arc<dyn Provider>, String> + Send + Sync>;
/// Builds one tool instance. The factory receives its [`ToolSpec`] so a host-side
/// factory can apply a `ToolFace`; [`assemble`] only checks the result.
pub type ToolFactory =
    Box<dyn Fn(&ToolSpec, &ToolServices) -> Result<Arc<dyn Tool>, String> + Send + Sync>;

/// Name → constructor closures. The host builds it; agents never see it.
#[derive(Default)]
pub struct Catalog {
    providers: BTreeMap<String, ProviderFactory>,
    tools: BTreeMap<String, ToolFactory>,
    /// Shared by every agent assembled from this catalog — a parent and its
    /// workers — so their file mutations are serialized (`p1_workspace::WriteGate`).
    write_gate: WriteGate,
}

impl Catalog {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register (or replace) a provider factory under `key`.
    pub fn provider(&mut self, key: &str, make: ProviderFactory) {
        self.providers.insert(key.to_string(), make);
    }

    /// Register (or replace) a tool factory under `key`.
    pub fn tool(&mut self, key: &str, make: ToolFactory) {
        self.tools.insert(key.to_string(), make);
    }

    /// Every registered provider key, sorted.
    pub fn provider_keys(&self) -> Vec<String> {
        self.providers.keys().cloned().collect()
    }

    /// Every registered tool key, sorted.
    pub fn tool_keys(&self) -> Vec<String> {
        self.tools.keys().cloned().collect()
    }
}

/// Values a prompt is substituted with. All plain text: the caller decides what
/// the date format and the OS name are.
#[derive(Debug, Clone)]
pub struct Substitutions {
    pub workspace: String,
    pub date: String,
    pub os: String,
}

/// Everything one agent is assembled from, minus host-owned policies. The host
/// turns this into `AgentParts`.
pub struct Assembled {
    pub resolved: ResolvedEnvironment,
    pub provider: Arc<dyn Provider>,
    pub tools: Vec<Arc<dyn Tool>>,
    pub system_prompt: String,
    pub options: ModelOptions,
}

impl std::fmt::Debug for Assembled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Assembled")
            .field("resolved", &self.resolved)
            .field("provider", &"<provider>")
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|tool| tool.declaration().name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("system_prompt", &self.system_prompt)
            .field("options", &self.options)
            .finish()
    }
}

/// The effective environment: safe to serialise, journal, print and log. It is
/// built only from `RouteDescription`, declarations, identities, the prompt and
/// the options, so it can never contain a credential.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedEnvironment {
    pub environment: String,
    pub family: String,
    pub route: RouteDescription,
    pub system_prompt: String,
    pub tools: Vec<ResolvedTool>,
    pub options: ModelOptions,
}

/// One assembled tool: the catalog key, what the model is told, and the stable
/// implementation+variant identity that is journalled with each call.
#[derive(Debug, Clone, Serialize)]
pub struct ResolvedTool {
    pub module: String,
    pub declaration: ToolDeclaration,
    pub identity: ToolIdentity,
}

/// Every way assembly can fail. One variant per failure so a caller can act on
/// the kind instead of parsing text.
#[derive(Debug, thiserror::Error)]
pub enum AssemblyError {
    #[error("environment `{name}` was not found in any of {searched:?}")]
    EnvironmentNotFound {
        name: String,
        searched: Vec<PathBuf>,
    },
    #[error("invalid environment file {}: {message}", path.display())]
    InvalidEnvironmentFile { path: PathBuf, message: String },
    #[error("missing prompt file: {}", path.display())]
    MissingPrompt { path: PathBuf },
    #[error("unknown provider key `{key}`; available: {available:?}")]
    UnknownProvider { key: String, available: Vec<String> },
    #[error("unknown tool module `{module}`; available: {available:?}")]
    UnknownToolModule {
        module: String,
        available: Vec<String>,
    },
    #[error("factory for {what} failed: {message}")]
    FactoryFailed { what: String, message: String },
    #[error("two assembled tools share the model-facing name `{name}`")]
    DuplicateToolName { name: String },
    #[error(
        "tool module `{module}` did not apply the requested name (expected `{expected}`, got `{got}`)"
    )]
    FaceNotApplied {
        module: String,
        expected: String,
        got: String,
    },
    #[error("unknown prompt placeholder `{placeholder}`")]
    UnknownPlaceholder { placeholder: String },
    #[error(
        "prompt refers to `{{{{tool:{module}}}}}` but module `{module}` is not assembled in this environment"
    )]
    ToolNotInEnvironment { module: String },
    #[error("provider rejected the assembled environment: {0}")]
    ProviderRejected(ProviderError),
    #[error("invalid workspace: {message}")]
    InvalidWorkspace { message: String },
}

// ------------------------------------------------------------------ loading

/// Search each directory in `search_dirs` in order; the first
/// `<dir>/<name>/environment.toml` wins. The prompt is read from `prompt.md` in
/// the same directory and each `description_file` relative to it.
pub fn load_environment(
    name: &str,
    search_dirs: &[PathBuf],
) -> Result<EnvironmentFile, AssemblyError> {
    let dir = search_dirs
        .iter()
        .map(|base| base.join(name))
        .find(|dir| dir.join(ENVIRONMENT_FILE).is_file())
        .ok_or_else(|| AssemblyError::EnvironmentNotFound {
            name: name.to_string(),
            searched: search_dirs.to_vec(),
        })?;

    let path = dir.join(ENVIRONMENT_FILE);
    let text =
        std::fs::read_to_string(&path).map_err(|error| AssemblyError::InvalidEnvironmentFile {
            path: path.clone(),
            message: error.to_string(),
        })?;
    let parsed: EnvironmentToml =
        toml::from_str(&text).map_err(|error| AssemblyError::InvalidEnvironmentFile {
            path: path.clone(),
            message: error.to_string(),
        })?;

    let prompt_path = dir.join(PROMPT_FILE);
    let prompt_template = std::fs::read_to_string(&prompt_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssemblyError::MissingPrompt {
                path: prompt_path.clone(),
            }
        } else {
            AssemblyError::InvalidEnvironmentFile {
                path: prompt_path.clone(),
                message: error.to_string(),
            }
        }
    })?;

    let mut tools = Vec::with_capacity(parsed.tools.len());
    for tool in parsed.tools {
        let description = match &tool.description_file {
            None => None,
            Some(relative) => {
                let description_path = dir.join(relative);
                let text = std::fs::read_to_string(&description_path).map_err(|error| {
                    AssemblyError::InvalidEnvironmentFile {
                        path: description_path.clone(),
                        message: error.to_string(),
                    }
                })?;
                Some(text)
            }
        };
        tools.push(ToolSpec {
            module: tool.module,
            name: tool.name,
            description,
            variant: tool.variant,
        });
    }

    Ok(EnvironmentFile {
        name: name.to_string(),
        family: parsed.family,
        provider: parsed.provider,
        model: parsed.model,
        options: parsed.options.into(),
        tools,
        prompt_template,
    })
}

/// The TOML surface of `environment.toml`. `deny_unknown_fields` everywhere, so a
/// typo is an error naming the key rather than a silently ignored setting.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentToml {
    family: String,
    provider: String,
    model: String,
    #[serde(default)]
    options: OptionsToml,
    #[serde(default)]
    tools: Vec<ToolToml>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
struct OptionsToml {
    reasoning_effort: Option<Effort>,
    max_output_tokens: Option<u32>,
    cache_key: Option<String>,
    #[serde(default)]
    native: BTreeMap<String, serde_json::Value>,
}

impl From<OptionsToml> for ModelOptions {
    fn from(options: OptionsToml) -> Self {
        Self {
            reasoning_effort: options.reasoning_effort,
            max_output_tokens: options.max_output_tokens,
            cache_key: options.cache_key,
            native: options.native,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct ToolToml {
    module: String,
    name: Option<String>,
    description_file: Option<String>,
    variant: Option<String>,
}

// ------------------------------------------------------------------ assembly

/// Assemble one agent's environment. The order matters: workspace, provider,
/// tools (in file order, sharing ONE fresh [`ToolServices`]), duplicate-name
/// check, prompt substitution, `Provider::validate` on the empty-history
/// first request. Fails before a run starts.
pub fn assemble(
    catalog: &Catalog,
    environment: &EnvironmentFile,
    workspace: &Path,
    substitutions: &Substitutions,
) -> Result<Assembled, AssemblyError> {
    let workspace = Workspace::new(workspace)
        .map_err(|error| AssemblyError::InvalidWorkspace {
            message: error.to_string(),
        })?
        .with_write_gate(catalog.write_gate.clone());
    let services = ToolServices {
        workspace,
        observed: ObservedFiles::new(),
    };

    let provider_key = environment.provider.as_str();
    let make_provider =
        catalog
            .providers
            .get(provider_key)
            .ok_or_else(|| AssemblyError::UnknownProvider {
                key: environment.provider.clone(),
                available: catalog.provider_keys(),
            })?;
    let provider_spec = ProviderSpec {
        key: environment.provider.clone(),
        model: environment.model.clone(),
    };
    let provider =
        make_provider(&provider_spec).map_err(|message| AssemblyError::FactoryFailed {
            what: format!("provider `{provider_key}`"),
            message,
        })?;

    // `modules` maps a catalog key to its assembled model-facing name for
    // `{{tool:<module>}}`. If a key appears twice, the first occurrence wins.
    let mut built: Vec<(String, Arc<dyn Tool>)> = Vec::with_capacity(environment.tools.len());
    let mut modules: Vec<(String, String)> = Vec::with_capacity(environment.tools.len());
    for spec in &environment.tools {
        let make_tool = catalog.tools.get(spec.module.as_str()).ok_or_else(|| {
            AssemblyError::UnknownToolModule {
                module: spec.module.clone(),
                available: catalog.tool_keys(),
            }
        })?;
        let tool = make_tool(spec, &services).map_err(|message| AssemblyError::FactoryFailed {
            what: format!("tool module `{}`", spec.module),
            message,
        })?;
        if let Some(expected) = &spec.name {
            let got = tool.declaration().name.clone();
            if &got != expected {
                return Err(AssemblyError::FaceNotApplied {
                    module: spec.module.clone(),
                    expected: expected.clone(),
                    got,
                });
            }
        }
        if !modules.iter().any(|(module, _)| module == &spec.module) {
            modules.push((spec.module.clone(), tool.declaration().name.clone()));
        }
        built.push((spec.module.clone(), tool));
    }

    let mut seen = HashSet::new();
    for (_, tool) in &built {
        let name = tool.declaration().name.clone();
        if !seen.insert(name.clone()) {
            return Err(AssemblyError::DuplicateToolName { name });
        }
    }

    let system_prompt = substitute_prompt(&environment.prompt_template, &modules, substitutions)?;
    let declarations: Vec<ToolDeclaration> = built
        .iter()
        .map(|(_, tool)| tool.declaration().clone())
        .collect();
    let options = environment.options.clone();

    // Fail fast on anything this route cannot carry, against the exact first
    // request the core would send: prompt, declarations in order, options, no history.
    let request = ProviderRequest {
        system_prompt: system_prompt.clone(),
        history: Vec::new(),
        tools: declarations,
        options: options.clone(),
    };
    provider
        .validate(&request)
        .map_err(AssemblyError::ProviderRejected)?;

    let resolved_tools: Vec<ResolvedTool> = built
        .iter()
        .map(|(module, tool)| ResolvedTool {
            module: module.clone(),
            declaration: tool.declaration().clone(),
            identity: tool.identity().clone(),
        })
        .collect();
    let tools: Vec<Arc<dyn Tool>> = built.into_iter().map(|(_, tool)| tool).collect();
    let resolved = ResolvedEnvironment {
        environment: environment.name.clone(),
        family: environment.family.clone(),
        route: provider.describe(),
        system_prompt: system_prompt.clone(),
        tools: resolved_tools,
        options: options.clone(),
    };

    Ok(Assembled {
        resolved,
        provider,
        tools,
        system_prompt,
        options,
    })
}

/// Substitute exactly the documented placeholders. A literal `{{` cannot be
/// escaped in this slice: any `{{` starts a placeholder.
fn substitute_prompt(
    template: &str,
    modules: &[(String, String)],
    substitutions: &Substitutions,
) -> Result<String, AssemblyError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(AssemblyError::UnknownPlaceholder {
                placeholder: after.to_string(),
            });
        };
        let placeholder = &after[..end];
        match placeholder {
            "workspace" => out.push_str(&substitutions.workspace),
            "date" => out.push_str(&substitutions.date),
            "os" => out.push_str(&substitutions.os),
            "tool_names" => {
                let names: Vec<&str> = modules.iter().map(|(_, name)| name.as_str()).collect();
                out.push_str(&names.join(", "));
            }
            other => {
                let Some(module) = other.strip_prefix("tool:") else {
                    return Err(AssemblyError::UnknownPlaceholder {
                        placeholder: other.to_string(),
                    });
                };
                let Some((_, name)) = modules.iter().find(|(key, _)| key == module) else {
                    return Err(AssemblyError::ToolNotInEnvironment {
                        module: module.to_string(),
                    });
                };
                out.push_str(name);
            }
        }
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    Ok(out)
}
