//! Environment assembly: an environment file becomes a validated
//! [`ResolvedEnvironment`] plus the provider, tools, prompt and options one agent
//! is built from.
//!
//! This crate names no concrete provider or tool. Everything it can build comes
//! from a [`Catalog`] of closures supplied by the composition root (`p1-host`);
//! an environment file can therefore never select a module the host did not register
//! (compiled into the binary, or an official package the host resolved through
//! [`ModulesLock`] and loaded). `assemble` never runs a model and never touches the
//! network.
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
//!
//! # Conditional sections
//!
//! `{{#tool:<module>}}` opens a conditional section and `{{/tool:<module>}}` closes
//! it. The enclosed text — placeholders inside it included — is kept whole when the
//! module is assembled in this environment and dropped entirely when it is not, so a
//! prompt can talk about a tool a narrower agent (a worker granted only some
//! modules) does not have, and a `{{tool:<module>}}` inside a dropped section is not
//! an error. Sections nest, for different modules.
//!
//! A dropped section also drops ONE newline directly after its closing tag, so a
//! section written on its own lines leaves no blank line behind. For the template
//!
//! ```text
//! A
//! {{#tool:m}}
//! B
//! {{/tool:m}}
//! C
//! ```
//!
//! with `m` assembled the render is `A`, a blank line, `B`, a blank line, `C` (the
//! tags' own line breaks stay); with `m` absent it is `A`, `C` — the section and the
//! newline after `{{/tool:m}}` go, so nothing is left between them.
//!
//! An unclosed section, a close with no open section, and a close naming a module
//! other than the innermost open one are errors
//! ([`AssemblyError::ToolSectionMismatch`]); `{{tool:<module>}}` OUTSIDE any section
//! for a module that is not assembled stays an error, as before.

use std::collections::{BTreeMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::{
    Effort, ModelOptions, Provider, ProviderError, ProviderRequest, RouteDescription, Tool,
    ToolDeclaration, ToolIdentity,
};
use p1_model_profile::ModelProfile;
use p1_redact::MaskCounter;
use p1_workspace::{ObservedFiles, Workspace, WriteGate};
use serde::{Deserialize, Serialize};

mod modules_lock;
pub use modules_lock::{
    LockedModule, LockedProtocol, MODULES_LOCK_FORMAT, ModulesLock, ModulesLockError,
    load_modules_lock,
};

/// File name of an environment definition inside `<dir>/<name>/`.
const ENVIRONMENT_FILE: &str = "environment.toml";
/// Prompt file name inside an environment directory. Not configurable in this slice.
const PROMPT_FILE: &str = "prompt.md";
/// Optional whole-file override of the summarizer prompt, next to `prompt.md`.
const SUMMARIZE_FILE: &str = "summarize.md";
/// Profile directory, next to the environments directory that was selected.
const PROFILES_DIR: &str = "../profiles";
/// Default per-tool-result budget for the summarizer transcript.
const DEFAULT_TOOL_RESULT_EXCERPT_CHARS: usize = 2_000;
/// Default cap on one summary's output tokens (context.md "Revision 2026-09-20").
/// Duplicated from `p1-context` on purpose: `p1-assembly` names no context module.
const DEFAULT_SUMMARY_OUTPUT_TOKENS: u64 = 4_000;

// ------------------------------------------------------------------ public API

/// One environment file, parsed. `description` is filled in from
/// `description_file` by [`load_environment`]. `prompt_template` is the raw text
/// of `prompt.md` (substituted by [`assemble`]).
#[derive(Debug, Clone)]
pub struct EnvironmentFile {
    pub name: String,
    /// The old form's family, or the selected profile's family.
    pub family: String,
    /// A whole-provider catalog key, or a route id when `profile` is `Some`.
    pub provider: String,
    /// The old form's configured model, or the selected profile's `model_id`.
    pub model: String,
    /// `Some` exactly when the environment names `route` + `profile`.
    pub profile: Option<Arc<ModelProfile>>,
    pub options: ModelOptions,
    pub tools: Vec<ToolSpec>,
    pub prompt_template: String,
    /// The validated `[context]` table, when the environment has one.
    pub context: Option<ContextSettings>,
    /// The raw `summarize.md` override, when present.
    pub summarize_prompt: Option<String>,
}

/// The optional `[context]` table of an environment file, already validated.
/// Plain data: `p1-assembly` names no context module and does not depend on one.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextSettings {
    /// Capacity of this model on this route.
    pub window_tokens: u64,
    /// Reserved for the next response.
    pub output_headroom_tokens: u64,
    /// The useful point: where summarizing starts. Below `window - headroom`.
    pub summarize_at_tokens: u64,
    /// Newest part of the history kept verbatim.
    pub keep_recent_tokens: u64,
    /// Budget for user messages kept verbatim.
    pub user_verbatim_tokens: u64,
    /// Per tool result, when rendered for the summarizer.
    #[serde(default = "default_tool_result_excerpt_chars")]
    pub tool_result_excerpt_chars: usize,
    /// Cap on one summary's output tokens: `max_output_tokens` of the summarizing
    /// request, and the room its rendered transcript is measured against.
    #[serde(default = "default_summary_output_tokens")]
    pub summary_output_tokens: u64,
}

fn default_tool_result_excerpt_chars() -> usize {
    DEFAULT_TOOL_RESULT_EXCERPT_CHARS
}

fn default_summary_output_tokens() -> u64 {
    DEFAULT_SUMMARY_OUTPUT_TOKENS
}

impl ContextSettings {
    /// The checks `load_environment` enforces before an agent is built.
    pub fn validate(&self) -> Result<(), String> {
        for (name, value) in [
            ("window_tokens", self.window_tokens),
            ("output_headroom_tokens", self.output_headroom_tokens),
            ("summarize_at_tokens", self.summarize_at_tokens),
            ("keep_recent_tokens", self.keep_recent_tokens),
            ("user_verbatim_tokens", self.user_verbatim_tokens),
            ("summary_output_tokens", self.summary_output_tokens),
        ] {
            if value == 0 {
                return Err(format!("{name} must be greater than zero"));
            }
        }
        if self.tool_result_excerpt_chars == 0 {
            return Err("tool_result_excerpt_chars must be greater than zero".to_string());
        }
        let wall = self
            .window_tokens
            .saturating_sub(self.output_headroom_tokens);
        if self.summarize_at_tokens >= wall {
            return Err(format!(
                "summarize_at_tokens ({}) must be below window_tokens - output_headroom_tokens ({wall})",
                self.summarize_at_tokens
            ));
        }
        if self.summary_output_tokens >= wall {
            return Err(format!(
                "summary_output_tokens ({}) must be below window_tokens - output_headroom_tokens ({wall})",
                self.summary_output_tokens
            ));
        }
        Ok(())
    }
}

/// One `[[tools]]` entry: a catalog key plus the optional per-agent face override.
#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub module: String,
    pub name: Option<String>,
    pub description: Option<String>,
    pub variant: Option<String>,
}

/// What the provider factory is given: the catalog key (a whole-provider key, or a
/// route id when the environment used the new form), the model to request, and the
/// profile the environment selected. `profile` is `Some` exactly in the new form, so
/// a factory can refuse the form its key does not accept instead of guessing.
#[derive(Debug, Clone)]
pub struct ProviderSpec {
    pub key: String,
    pub model: String,
    pub profile: Option<Arc<ModelProfile>>,
}

/// What tools of ONE agent share. Created fresh per [`assemble`] call, so two
/// agents never share read-state.
#[derive(Clone)]
pub struct ToolServices {
    pub workspace: Workspace,
    pub observed: ObservedFiles,
    /// Issue #142: the assembling agent's ONE [`MaskCounter`], shared with the notice
    /// sink the turn reports through. A factory that wraps its tool in `p1-redact`'s
    /// decorator binds THIS counter — the module catalog factory does, so a module
    /// tool's masking is counted in the turn's own notice rather than a throwaway.
    pub mask: Arc<MaskCounter>,
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
    /// The effective context-control table, or absent when the environment does
    /// not opt in (passthrough).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context: Option<ContextSettings>,
    /// The `summarize.md` override, or absent when the compiled-in prompt is used.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summarize_prompt: Option<String>,
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
    /// The provider keys present are neither `route` + `profile` nor
    /// `provider` + `model` + `family`. The message names both valid forms.
    #[error("invalid environment form in {}: {message}", path.display())]
    InvalidEnvironmentForm { path: PathBuf, message: String },
    /// The profile an environment names has no `profiles/<id>.toml`.
    #[error("profile `{profile}` was not found in {}; available: {available:?}", dir.display())]
    ProfileNotFound {
        profile: String,
        dir: PathBuf,
        available: Vec<String>,
    },
    #[error("invalid profile file {}: {message}", path.display())]
    InvalidProfileFile { path: PathBuf, message: String },
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
    /// A conditional section `{{#tool:<module>}}` … `{{/tool:<module>}}` that is not
    /// balanced: an open left unclosed, a close with no open section, or a close
    /// naming another module than the innermost open one.
    #[error("prompt has an unclosed or mismatched conditional section for tool module `{module}`")]
    ToolSectionMismatch { module: String },
    #[error("provider rejected the assembled environment: {0}")]
    ProviderRejected(ProviderError),
    #[error("invalid [context] configuration: {message}")]
    InvalidContext { message: String },
    #[error("invalid workspace: {message}")]
    InvalidWorkspace { message: String },
}

// ------------------------------------------------------------------ loading

/// Search each directory in `search_dirs` in order; the first
/// `<dir>/<name>/environment.toml` wins. The prompt is read from `prompt.md` in
/// the same directory and each `description_file` relative to it. A new-form
/// environment (`route` + `profile`) loads `<dir>/../profiles/<profile>.toml`.
pub fn load_environment(
    name: &str,
    search_dirs: &[PathBuf],
) -> Result<EnvironmentFile, AssemblyError> {
    let (base, dir) = search_dirs
        .iter()
        .map(|base| (base, base.join(name)))
        .find(|(_, dir)| dir.join(ENVIRONMENT_FILE).is_file())
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

    // Which provider the environment names, and — in the new form — the profile
    // that carries the model policy. `family` and the model come from the profile;
    // route bindings (wire model, limits) arrive with route files.
    let (provider, model, family, profile) = match provider_form(&parsed, &path)? {
        ProviderForm::Routed { route, profile } => {
            let profile = load_profile(base, &profile)?;
            (
                route,
                profile.model_id.clone(),
                profile.family.clone(),
                Some(profile),
            )
        }
        ProviderForm::Whole {
            provider,
            model,
            family,
        } => (provider, model, family, None),
    };

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

    // The `[context]` table and its `summarize.md` override are parsed and checked
    // here, next to the files they came from, so an invalid one fails before an
    // agent is built. `p1-assembly` names no context module: this is plain data.
    let context = match parsed.context {
        Some(settings) => {
            settings
                .validate()
                .map_err(|message| AssemblyError::InvalidContext {
                    message: format!("{}: {message}", path.display()),
                })?;
            Some(settings)
        }
        None => None,
    };
    let summarize_path = dir.join(SUMMARIZE_FILE);
    let summarize_prompt = match std::fs::read_to_string(&summarize_path) {
        Ok(text) if text.trim().is_empty() => {
            return Err(AssemblyError::InvalidContext {
                message: format!(
                    "{}: the summarizer prompt is empty",
                    summarize_path.display()
                ),
            });
        }
        Ok(text) => Some(text),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(error) => {
            return Err(AssemblyError::InvalidEnvironmentFile {
                path: summarize_path,
                message: error.to_string(),
            });
        }
    };

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
        family,
        provider,
        model,
        profile,
        options: parsed.options.into(),
        tools,
        prompt_template,
        context,
        summarize_prompt,
    })
}

/// Load `profiles/<id>.toml` next to the environments directory that was selected
/// (`docs/design/routes-and-profiles.md` §1: shipped files live in the repository
/// root, next to `environments/`). A missing file names the profiles that exist.
fn load_profile(environments_base: &Path, id: &str) -> Result<Arc<ModelProfile>, AssemblyError> {
    let dir = environments_base.join(PROFILES_DIR);
    let path = dir.join(format!("{id}.toml"));
    let text = std::fs::read_to_string(&path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            AssemblyError::ProfileNotFound {
                profile: id.to_string(),
                dir: dir.clone(),
                available: available_profiles(&dir),
            }
        } else {
            AssemblyError::InvalidProfileFile {
                path: path.clone(),
                message: error.to_string(),
            }
        }
    })?;
    ModelProfile::from_toml(id, &text)
        .map(Arc::new)
        .map_err(|message| AssemblyError::InvalidProfileFile { path, message })
}

/// The profile ids a directory holds, sorted. A directory that does not exist or
/// cannot be read holds none.
fn available_profiles(dir: &Path) -> Vec<String> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut ids: Vec<String> = entries
        .flatten()
        .filter(|entry| entry.path().extension().and_then(|ext| ext.to_str()) == Some("toml"))
        .filter_map(|entry| {
            entry
                .path()
                .file_stem()
                .and_then(|stem| stem.to_str())
                .map(str::to_string)
        })
        .collect();
    ids.sort();
    ids
}

/// Which provider form the environment file uses. The two forms are exclusive, and
/// every other combination of keys is an error naming both.
enum ProviderForm {
    /// `route` + `profile` (spec 1.3): the profile carries the model policy.
    Routed { route: String, profile: String },
    /// `provider` + `model` + `family`: a whole provider that consumes no profile.
    Whole {
        provider: String,
        model: String,
        family: String,
    },
}

fn provider_form(parsed: &EnvironmentToml, path: &Path) -> Result<ProviderForm, AssemblyError> {
    let form_error = |present: &str| AssemblyError::InvalidEnvironmentForm {
        path: path.to_path_buf(),
        message: format!(
            "{present}; an environment names either `route` + `profile` or \
             `provider` + `model` + `family`"
        ),
    };
    match (
        parsed.route.as_deref(),
        parsed.profile.as_deref(),
        parsed.provider.as_deref(),
        parsed.model.as_deref(),
        parsed.family.as_deref(),
    ) {
        (Some(route), Some(profile), None, None, None) => Ok(ProviderForm::Routed {
            route: route.to_string(),
            profile: profile.to_string(),
        }),
        (None, None, Some(provider), Some(model), Some(family)) => Ok(ProviderForm::Whole {
            provider: provider.to_string(),
            model: model.to_string(),
            family: family.to_string(),
        }),
        _ => Err(form_error(&present_keys(parsed))),
    }
}

/// The provider-selecting keys an environment file actually sets, for the error.
fn present_keys(parsed: &EnvironmentToml) -> String {
    let keys: Vec<String> = [
        ("route", &parsed.route),
        ("profile", &parsed.profile),
        ("provider", &parsed.provider),
        ("model", &parsed.model),
        ("family", &parsed.family),
    ]
    .into_iter()
    .filter(|(_, value)| value.is_some())
    .map(|(name, _)| format!("`{name}`"))
    .collect();
    if keys.is_empty() {
        return "no provider keys were set".to_string();
    }
    format!("found {}", keys.join(" + "))
}

/// The TOML surface of `environment.toml`. `deny_unknown_fields` everywhere, so a
/// typo is an error naming the key rather than a silently ignored setting. The
/// provider keys are optional here: [`provider_form`] enforces that exactly one
/// form is present.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct EnvironmentToml {
    route: Option<String>,
    profile: Option<String>,
    family: Option<String>,
    provider: Option<String>,
    model: Option<String>,
    #[serde(default)]
    options: OptionsToml,
    #[serde(default)]
    context: Option<ContextSettings>,
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
///
/// The final [`ModelOptions`] are the environment file's own; a composition
/// root that must decide options FROM the resolved route (the host's cache-key
/// policy) uses [`assemble_with_route_options`] instead, which still builds the
/// provider and the tools exactly once.
///
/// The [`MaskCounter`] in the shared [`ToolServices`] is a fresh one no caller
/// can read: a caller that reports masking (the host's notice sink) assembles
/// through [`assemble_with_route_options`] with the agent's own counter, so the
/// count and the report share ONE counter (issue #142).
pub fn assemble(
    catalog: &Catalog,
    environment: &EnvironmentFile,
    workspace: &Path,
    substitutions: &Substitutions,
) -> Result<Assembled, AssemblyError> {
    let options = environment.options.clone();
    let mask = Arc::new(MaskCounter::new());
    assemble_with_route_options(
        catalog,
        environment,
        workspace,
        substitutions,
        &mask,
        move |_| options,
    )
}

/// As [`assemble`], with the final options supplied by `route_options` once the
/// provider is built and its [`RouteDescription`] is known. The provider and the
/// tools are each built ONCE: the description is read before anything else is
/// constructed, so a caller can finalise options (e.g. generate a cache key)
/// without a second assembly attempt. `p1-assembly` knows nothing about what the
/// caller does with the description.
///
/// `mask` is the assembling agent's ONE [`MaskCounter`] (issue #142): it is put in
/// the shared [`ToolServices`], so a factory that wraps its tool for masking binds
/// the counter the turn reports through, and the caller can wrap the assembled
/// tools with the same counter.
pub fn assemble_with_route_options(
    catalog: &Catalog,
    environment: &EnvironmentFile,
    workspace: &Path,
    substitutions: &Substitutions,
    mask: &Arc<MaskCounter>,
    route_options: impl FnOnce(&RouteDescription) -> ModelOptions,
) -> Result<Assembled, AssemblyError> {
    let workspace = Workspace::new(workspace)
        .map_err(|error| AssemblyError::InvalidWorkspace {
            message: error.to_string(),
        })?
        .with_write_gate(catalog.write_gate.clone());
    let services = ToolServices {
        workspace,
        observed: ObservedFiles::new(),
        mask: mask.clone(),
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
        profile: environment.profile.clone(),
    };
    let provider =
        make_provider(&provider_spec).map_err(|message| AssemblyError::FactoryFailed {
            what: format!("provider `{provider_key}`"),
            message,
        })?;
    let route = provider.describe();
    let options = route_options(&route);

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
        route,
        system_prompt: system_prompt.clone(),
        tools: resolved_tools,
        options: options.clone(),
        context: environment.context.clone(),
        summarize_prompt: environment.summarize_prompt.clone(),
    };

    Ok(Assembled {
        resolved,
        provider,
        tools,
        system_prompt,
        options,
    })
}

/// Render a prompt template against an assembled `(module, model-facing name)` list.
///
/// This is the substitution [`assemble`] performs, exposed so a caller can render
/// the same template for a different tool set — the tools one worker was granted —
/// without assembling a second agent. The rules are the ones in the crate docs,
/// conditional sections included.
pub fn render_prompt(
    template: &str,
    modules: &[(String, String)],
    substitutions: &Substitutions,
) -> Result<String, AssemblyError> {
    substitute_prompt(template, modules, substitutions)
}

/// Substitute exactly the documented placeholders and conditional sections. A literal
/// `{{` cannot be escaped in this slice: any `{{` starts a placeholder or a tag.
fn substitute_prompt(
    template: &str,
    modules: &[(String, String)],
    substitutions: &Substitutions,
) -> Result<String, AssemblyError> {
    let mut out = String::with_capacity(template.len());
    let mut rest = template;
    // The open conditional sections, outermost first, each with whether the text it
    // encloses is kept. `kept` is false as soon as one enclosing section goes.
    let mut open: Vec<(String, bool)> = Vec::new();
    let mut kept = true;
    while let Some(start) = rest.find("{{") {
        if kept {
            out.push_str(&rest[..start]);
        }
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            return Err(AssemblyError::UnknownPlaceholder {
                placeholder: after.to_string(),
            });
        };
        let placeholder = &after[..end];
        let mut next = &after[end + 2..];
        if let Some(module) = placeholder.strip_prefix("#tool:") {
            let assembled = modules.iter().any(|(key, _)| key == module);
            open.push((module.to_string(), kept && assembled));
            kept = kept && assembled;
        } else if let Some(module) = placeholder.strip_prefix("/tool:") {
            match open.pop() {
                Some((name, was_kept)) if name == module => {
                    if !was_kept {
                        // A dropped section also drops one newline directly after its
                        // closing tag (crate docs).
                        next = next.strip_prefix('\n').unwrap_or(next);
                    }
                    kept = open.iter().all(|(_, kept)| *kept);
                }
                // A close with no open section, or one naming another module than the
                // innermost open section.
                _ => {
                    return Err(AssemblyError::ToolSectionMismatch {
                        module: module.to_string(),
                    });
                }
            }
        } else if kept {
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
        }
        rest = next;
    }
    if let Some((module, _)) = open.last() {
        return Err(AssemblyError::ToolSectionMismatch {
            module: module.clone(),
        });
    }
    out.push_str(rest);
    Ok(out)
}
