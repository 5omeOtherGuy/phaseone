//! Hand-written argument parsing. No clap: the surface is tiny and the error
//! messages are part of the interface.
//!
//! `p1 [--env NAME] [--model REF] [--effort LEVEL] [--models PATTERNS]
//! [--workspace DIR] [--session FILE] [--resume] [--ask] [PROMPT…]`
//! `p1 models [SEARCH]`
//! `p1 env show NAME`
//! `p1 workflow run FILE [--arg k=v]… [--args FILE] [--role r=E/P[:effort]]…`
//! `p1 login <route>` / `p1 login --list` / `p1 logout <route>`
//! `p1 --help` / `p1 --version`

use std::path::{Path, PathBuf};

use p1_contracts::Effort;

use crate::models::parse_effort;

/// The default environment when `--env` is not given.
pub const DEFAULT_ENV: &str = "claude";

/// Headless continuation budget when `--max-continuations` is not given.
pub const DEFAULT_MAX_CONTINUATIONS: usize = 3;

/// Headless provider-retry budget when `--provider-retries` is not given.
pub const DEFAULT_PROVIDER_RETRIES: usize = 3;

/// Headless bound on consecutive context replacements without progress when
/// `--max-idle-summaries` is not given (completion.md §3c).
pub const DEFAULT_MAX_IDLE_SUMMARIES: usize = 6;

/// Whether `shell` commands run inside the bubblewrap sandbox.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum SandboxMode {
    /// No sandbox: the shell runs directly (the default).
    #[default]
    Off,
    /// Every `shell` command runs under `bwrap` with only the workspace and a
    /// private `/tmp` writable.
    Workspace,
}

/// What the process should do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Run an agent: one headless turn when `prompt` is `Some`, otherwise the
    /// interactive prompt loop.
    Run {
        prompt: Option<String>,
    },
    /// Print the resolved environment as JSON and exit.
    EnvShow {
        name: String,
    },
    /// Every model this host can run: `E/P`, route, efforts and credential source.
    Models {
        search: Option<String>,
    },
    /// Read one API key from stdin and store it for this route (ADR-0044, spec §6).
    Login {
        route: String,
    },
    /// Every route, its credential kind and which source its credential comes from.
    LoginList,
    /// Remove this route's entry from p1's store.
    Logout {
        route: String,
    },
    /// The route quota ledger (ADR-0052): `p1 usage`.
    Usage(UsageOptions),
    /// Run one workflow script with no parent agent (ADR-0053): `p1 workflow run`.
    WorkflowRun(WorkflowRunOptions),
    Help,
    Version,
}

/// Parsed arguments for `p1 workflow run`. `args` and `args_file` stay text here: the
/// file is read and the JSON built when the command runs, so `parse` touches no file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkflowRunOptions {
    pub file: PathBuf,
    /// `--arg k=v`, in order; laid over `--args FILE`.
    pub args: Vec<(String, String)>,
    /// `--args FILE`: a JSON object (arguments too large for a command line).
    pub args_file: Option<PathBuf>,
    /// `--role r=E/P[:effort]`, in order.
    pub roles: Vec<(String, String)>,
    pub resume_from: Option<String>,
    /// `--out DIR`: where the run's directory goes.
    pub out: Option<PathBuf>,
    pub max_workers: usize,
}

/// `--max-workers` when it is not given.
pub const DEFAULT_MAX_WORKERS: usize = 2;

/// Parsed arguments for `p1 usage`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageOptions {
    pub json: bool,
    pub watch: Option<u64>,
    pub plain: bool,
    pub grid: usize,
    pub search: Option<String>,
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub command: Command,
    pub env: String,
    /// Whether `--env` was given. `settings.toml`'s `default_model` decides the
    /// environment only when it was not.
    pub env_given: bool,
    /// `--model REF`: the model to run (ADR-0049 stage 1).
    pub model: Option<String>,
    /// `--effort LEVEL`: the reasoning effort of whatever was selected.
    pub effort: Option<Effort>,
    /// `--models PATTERNS`: the scope for this run, replacing `enabled_models`.
    pub models: Option<String>,
    pub workspace: Option<PathBuf>,
    pub session: Option<PathBuf>,
    pub resume: bool,
    /// Ask before permitting a call: the restrictive policy (ADR-0038). The
    /// default is full access; `--yes` is accepted and means the default.
    pub ask: bool,
    /// Whether `shell` commands run in the bubblewrap sandbox.
    pub sandbox: SandboxMode,
    /// Extra paths the sandbox keeps writable, absolute and canonicalised when
    /// they exist.
    pub sandbox_write: Vec<PathBuf>,
    /// Extra paths the sandbox keeps visible READ-ONLY, absolute and
    /// canonicalised when they exist.
    pub sandbox_read: Vec<PathBuf>,
    /// Extra environment variable NAMES the `shell` tool passes on, on top of
    /// its built-in allow-list. Repeatable; a name never contains `=`.
    pub env_pass: Vec<String>,
    /// At most this many continuations after a premature stop in an unattended
    /// run. `0` disables continuation.
    pub max_continuations: usize,
    /// At most this many CONSECUTIVE transient provider failures to wait out and
    /// retry in an unattended run. `0` disables retrying.
    pub provider_retries: usize,
    /// At most this many CONSECUTIVE context replacements without a workspace
    /// change before an unattended run stalls. `0` disables the guard.
    pub max_idle_summaries: usize,
    /// Run the interactive session in the TUI (issue #12). Interactive only:
    /// headless runs and non-TTY stdout keep the line renderer forever.
    pub tui: bool,
}

impl Options {
    /// Headless means a PROMPT was supplied, or a workflow runs unattended. Used by
    /// the authorization policy.
    pub fn is_headless(&self) -> bool {
        matches!(
            self.command,
            Command::Run { prompt: Some(_) } | Command::WorkflowRun(_)
        )
    }
}

/// A command line the user must fix. Printed to stderr; the process exits 2.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CliError {
    pub message: String,
}

impl std::fmt::Display for CliError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for CliError {}

/// The usage text for `--help` and usage errors.
pub fn usage() -> String {
    let mut out = String::new();
    out.push_str("p1 — a lean, model-shaped coding harness\n\n");
    out.push_str("usage:\n");
    out.push_str(
        "  p1 [--env NAME] [--model REF] [--effort LEVEL] [--models PATTERNS]\n     [--workspace DIR] [--session FILE] [--resume] [--ask] [PROMPT…]\n",
    );
    out.push_str("  p1 models [SEARCH]   every model: `E/P`, route, efforts, credential source\n");
    out.push_str("  p1 env show NAME\n");
    out.push_str(
        "  p1 workflow run FILE [--arg K=V]… [--args FILE] [--role R=E/P[:effort]]…\n     [--resume-from ID] [--out DIR] [--workspace DIR] [--session FILE]\n     [--max-workers N] [--yes]\n                       run a workflow script without a parent agent; `--arg`\n                       values that parse as JSON are passed as JSON, and lie over\n                       the JSON object in `--args FILE`; exit 0 completed,\n                       2 completed with issues, 1 failed, 130 cancelled\n",
    );
    out.push_str("  p1 usage [--json] [--watch SECONDS] [--plain] [--grid N] [SEARCH]   route quota ledger\n");
    out.push_str("  p1 login <route>     read one API key from stdin and store it for ROUTE\n");
    out.push_str("  p1 login --list      every route, its credential kind and its source\n");
    out.push_str("  p1 logout <route>    remove ROUTE's entry from p1's store\n");
    out.push_str("  p1 --help\n");
    out.push_str("  p1 --version\n\n");
    out.push_str("flags:\n");
    out.push_str("  --env NAME        environment to run (default: claude)\n");
    out.push_str(
        "  --model REF       model to run: `environment/profile`, or a bare profile name\n                    bound in exactly one environment, with an optional `:effort`\n                    (default: the environment's own profile)\n",
    );
    out.push_str(
        "  --effort LEVEL    reasoning effort of the selected model: low, medium, high,\n                    extra_high or max\n",
    );
    out.push_str(
        "  --models PATTERNS comma-separated globs (`*`, `?`) that scope the models this\n                    run may cycle through, replacing `enabled_models` from\n                    settings.toml; a pattern without `/` matches the profile part\n                    (`p1 models` takes it too)\n",
    );
    out.push_str("  --workspace DIR   workspace root (default: current directory)\n");
    out.push_str("  --session FILE    write the session journal to FILE as JSONL\n");
    out.push_str("  --resume          continue an existing --session file\n");
    out.push_str(
        "  --ask             ask before permitting a tool call; headless permits only\n                    read-only calls (default: full access, no questions)\n",
    );
    out.push_str("  --tui             run the interactive session in the TUI\n");
    out.push_str(
        "  --yes             permit every tool call without asking (the default; kept for\n                    compatibility; cannot be combined with --ask)\n",
    );
    out.push_str(
        "  --sandbox MODE    run shell commands in a bubblewrap sandbox: `workspace` or\n                    `off` (default: off)\n",
    );
    out.push_str(
        "  --sandbox-write PATH\n                    keep PATH writable in the sandbox (repeatable;\n                    requires --sandbox workspace)\n",
    );
    out.push_str(
        "  --sandbox-read PATH\n                    keep PATH readable in the sandbox (repeatable;\n                    requires --sandbox workspace)\n",
    );
    out.push_str(
        "  --env-pass NAME   pass NAME from p1's environment to shell commands\n                    (repeatable; NAME must not contain `=`)\n",
    );
    out.push_str(
        "  --max-continuations N\n                    most continuations after a premature stop in an unattended\n                    run (default: 3; 0 disables)\n",
    );
    out.push_str(
        "  --provider-retries N\n                    most consecutive transient provider failures to wait out and\n                    retry in an unattended run (default: 3; 0 disables)\n",
    );
    out.push_str(
        "  --max-idle-summaries N\n                    most consecutive context summaries without a workspace change\n                    before an unattended run stalls (default: 6; 0 disables)\n",
    );
    out.push_str(
        "  --                end option and subcommand parsing; every later token is\n                    the prompt verbatim (e.g. `p1 -- envs` sends \"envs\")\n",
    );
    out
}

/// `p1 <version>`.
pub fn version() -> String {
    format!("p1 {}", env!("CARGO_PKG_VERSION"))
}

/// Parse the arguments after the program name.
pub fn parse(args: &[String]) -> Result<Options, CliError> {
    if let Some(first) = args.first() {
        if first == "--help" || first == "-h" || first == "help" {
            return Ok(defaults(Command::Help));
        }
        if first == "--version" || first == "-V" {
            return Ok(defaults(Command::Version));
        }
        if first == "env" {
            return parse_env_show(args);
        }
        if first == "models" {
            return parse_models(args);
        }
        if first == "usage" {
            return parse_usage(args);
        }
        if first == "login" {
            return parse_login(args);
        }
        if first == "logout" {
            return parse_logout(args);
        }
        if first == "workflow" {
            return parse_workflow(args);
        }
    }

    let mut env: Option<String> = None;
    let mut model: Option<String> = None;
    let mut effort: Option<Effort> = None;
    let mut models: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut session: Option<PathBuf> = None;
    let mut resume = false;
    let mut ask = false;
    let mut tui = false;
    let mut yes = false;
    let mut prompt_words: Vec<String> = Vec::new();
    // `--` ends option and subcommand parsing: every later token is a prompt word,
    // verbatim, even one that looks like a typo of a subcommand (issue #83).
    let mut literal_prompt = false;

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--env" => {
                let value = take_value(args, &mut index, "--env")?;
                env = Some(value);
            }
            "--model" => {
                let value = take_value(args, &mut index, "--model")?;
                model = Some(value);
            }
            "--effort" => {
                let value = take_value(args, &mut index, "--effort")?;
                effort = Some(parse_effort(&value).map_err(|message| CliError { message })?);
            }
            "--models" => {
                let value = take_value(args, &mut index, "--models")?;
                models = Some(value);
            }
            "--workspace" => {
                let value = take_value(args, &mut index, "--workspace")?;
                workspace = Some(PathBuf::from(value));
            }
            "--session" => {
                let value = take_value(args, &mut index, "--session")?;
                session = Some(PathBuf::from(value));
            }
            "--resume" => resume = true,
            "--ask" => ask = true,
            "--yes" => yes = true,
            "--tui" => tui = true,
            // `--` ends option/subcommand parsing; the rest is the prompt.
            "--" => {
                literal_prompt = true;
                index += 1;
                while index < args.len() {
                    prompt_words.push(args[index].clone());
                    index += 1;
                }
                break;
            }
            // Validated by `parse_sandbox`/`parse_env_pass`/`parse_max_continuations`
            // /`parse_provider_retries` after the loop; the values are consumed here
            // so they are not mistaken for prompt words.
            "--sandbox"
            | "--sandbox-write"
            | "--sandbox-read"
            | "--env-pass"
            | "--max-continuations"
            | "--provider-retries"
            | "--max-idle-summaries" => {
                take_value(args, &mut index, arg)?;
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(CliError {
                    message: format!("unknown flag `{other}`"),
                });
            }
            other => prompt_words.push(other.to_string()),
        }
        index += 1;
    }

    // A single positional token shaped like a subcommand and near one is a typo,
    // not a prompt: error before any provider is contacted (issue #83). `--`
    // suppresses this, because it says the token IS the prompt.
    if !literal_prompt
        && prompt_words.len() == 1
        && let Some(suggestion) = unknown_command_suggestion(&prompt_words[0])
    {
        let word = &prompt_words[0];
        return Err(CliError {
            message: format!(
                "unknown command '{word}' — did you mean '{suggestion}'? (to send it as a prompt: p1 -- {word})"
            ),
        });
    }

    if resume && session.is_none() {
        return Err(CliError {
            message: "--resume requires --session".to_string(),
        });
    }
    // `--yes` is the default, so combining it with `--ask` names two policies at once.
    if yes && ask {
        return Err(CliError {
            message: "--yes and --ask cannot be combined: --yes is the default".to_string(),
        });
    }
    // Validate the sandbox and env-pass flags and keep their parsed state on the
    // options.
    let (sandbox, sandbox_write, sandbox_read) = parse_sandbox(args)?;
    let env_pass = parse_env_pass(args)?;
    let max_continuations = parse_max_continuations(args)?;
    let provider_retries = parse_provider_retries(args)?;
    let max_idle_summaries = parse_max_idle_summaries(args)?;

    // The TUI is interactive-only (issue #12): a prompt means headless, and a
    // non-terminal stdio is the line renderer's domain forever.
    if tui {
        if !prompt_words.is_empty() {
            return Err(CliError {
                message: "--tui is interactive-only: it cannot run with a prompt".to_string(),
            });
        }
        use std::io::IsTerminal;
        if !std::io::stdout().is_terminal() || !std::io::stdin().is_terminal() {
            return Err(CliError {
                message: "--tui requires a terminal".to_string(),
            });
        }
    }

    let prompt = if prompt_words.is_empty() {
        None
    } else {
        Some(prompt_words.join(" "))
    };

    // `--env` and `--model` name the same choice twice, so a model reference whose
    // environment is not the one `--env` named is a usage error. A bare profile
    // reference is checked where the routes are known (a bare `P` may be bound in
    // exactly one environment, which `--env` then has to be).
    if let (Some(environment), Some(reference)) = (&env, &model)
        && let Some((named, profile)) = reference.split_once('/')
        && !named.is_empty()
        && !profile.is_empty()
        && named != environment
    {
        return Err(CliError {
            message: format!(
                "--env `{environment}` and --model `{reference}` name different environments"
            ),
        });
    }

    let env_given = env.is_some();
    Ok(Options {
        command: Command::Run { prompt },
        env: env.unwrap_or_else(|| DEFAULT_ENV.to_string()),
        env_given,
        model,
        effort,
        models,
        workspace,
        session,
        resume,
        ask,
        tui,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        max_continuations,
        provider_retries,
        max_idle_summaries,
    })
}

/// The subcommands a first positional token can name. Kept in one place so a
/// near miss suggests from the same list the parser dispatches on.
const SUBCOMMANDS: [&str; 6] = ["models", "env", "workflow", "usage", "login", "logout"];

/// The subcommand a lone positional `token` most likely meant, or `None` when it
/// is not a typo (issue #83). A typo is shaped like a command (`^[a-z][a-z-]*$`),
/// is not itself a subcommand, and is either an exact plural/singular of one or
/// within edit distance 2 of one.
fn unknown_command_suggestion(token: &str) -> Option<&'static str> {
    if !is_command_shaped(token) {
        return None;
    }
    let mut best: Option<(usize, &'static str)> = None;
    for name in SUBCOMMANDS {
        // An exact subcommand is never a typo, even after flags (e.g. `--ask models`).
        if token == name {
            return None;
        }
        let singular = name.strip_suffix('s').unwrap_or(name);
        let distance = if token == singular || token == format!("{name}s") {
            0
        } else {
            edit_distance(token, name)
        };
        if distance <= 2 && best.is_none_or(|(best, _)| distance < best) {
            best = Some((distance, name));
        }
    }
    best.map(|(_, name)| name)
}

/// `^[a-z][a-z-]*$`: a lowercase word that starts with a letter and holds only
/// letters and hyphens. A prompt with capitals, digits, spaces or punctuation is
/// left alone.
fn is_command_shaped(token: &str) -> bool {
    let mut chars = token.chars();
    matches!(chars.next(), Some(first) if first.is_ascii_lowercase())
        && chars.all(|c| c.is_ascii_lowercase() || c == '-')
}

/// The Levenshtein distance between `a` and `b`, one row at a time.
fn edit_distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().collect();
    let b: Vec<char> = b.chars().collect();
    let mut previous: Vec<usize> = (0..=b.len()).collect();
    let mut current = vec![0usize; b.len() + 1];
    for (i, left) in a.iter().enumerate() {
        current[0] = i + 1;
        for (j, right) in b.iter().enumerate() {
            let substitution = previous[j] + usize::from(left != right);
            current[j + 1] = (previous[j + 1] + 1).min(current[j] + 1).min(substitution);
        }
        std::mem::swap(&mut previous, &mut current);
    }
    previous[b.len()]
}

fn parse_env_show(args: &[String]) -> Result<Options, CliError> {
    // `env show NAME`, followed only by the sandbox flags.
    if args.get(1).map(String::as_str) != Some("show") {
        let show = args.get(1).map(String::as_str).unwrap_or("");
        return Err(CliError {
            message: format!("unknown env subcommand `{show}`"),
        });
    }
    let Some(name) = args.get(2) else {
        return Err(CliError {
            message: "usage: p1 env show NAME".to_string(),
        });
    };
    if name.starts_with('-') {
        return Err(CliError {
            message: "usage: p1 env show NAME".to_string(),
        });
    }
    let mut index = 3;
    while index < args.len() {
        match args[index].as_str() {
            "--sandbox" | "--sandbox-write" | "--sandbox-read" | "--env-pass" => index += 1,
            other => {
                return Err(CliError {
                    message: format!("unexpected argument `{other}`"),
                });
            }
        }
        index += 1;
    }
    let (sandbox, sandbox_write, sandbox_read) = parse_sandbox(args)?;
    let env_pass = parse_env_pass(args)?;
    Ok(Options {
        command: Command::EnvShow { name: name.clone() },
        env: name.clone(),
        env_given: true,
        model: None,
        effort: None,
        models: None,
        workspace: None,
        session: None,
        resume: false,
        ask: false,
        tui: false,
        sandbox,
        sandbox_write,
        sandbox_read,
        env_pass,
        max_continuations: DEFAULT_MAX_CONTINUATIONS,
        provider_retries: DEFAULT_PROVIDER_RETRIES,
        max_idle_summaries: DEFAULT_MAX_IDLE_SUMMARIES,
    })
}

/// `p1 models [SEARCH]` (ADR-0049 stage 1, spec §2): one row per model. SEARCH is a
/// case-insensitive substring of `E/P`. `--models PATTERNS` scopes the run this
/// listing is for, exactly as it does on a run.
fn parse_models(args: &[String]) -> Result<Options, CliError> {
    let mut search: Option<String> = None;
    let mut models: Option<String> = None;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--models" => {
                models = Some(take_value(args, &mut index, "--models")?);
            }
            other if other.starts_with('-') && other != "-" => {
                return Err(CliError {
                    message: format!("unknown flag `{other}`"),
                });
            }
            other => {
                if search.is_some() {
                    return Err(CliError {
                        message: format!("unexpected argument `{other}`"),
                    });
                }
                search = Some(other.to_string());
            }
        }
        index += 1;
    }
    let mut options = defaults(Command::Models { search });
    options.models = models;
    Ok(options)
}

fn parse_usage(args: &[String]) -> Result<Options, CliError> {
    let (mut json, mut plain, mut watch, mut grid, mut search) = (false, false, None, 32, None);
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--json" => json = true,
            "--plain" => plain = true,
            "--watch" => {
                let value = take_value(args, &mut index, "--watch")?;
                let seconds = value.parse::<u64>().map_err(|_| CliError {
                    message: "--watch requires SECONDS >= 5".into(),
                })?;
                if seconds < 5 {
                    return Err(CliError {
                        message: "--watch requires SECONDS >= 5".into(),
                    });
                }
                watch = Some(seconds);
            }
            "--grid" => {
                let value = take_value(args, &mut index, "--grid")?;
                grid = value
                    .parse::<usize>()
                    .ok()
                    .filter(|n| *n > 0)
                    .ok_or_else(|| CliError {
                        message: "--grid requires a positive number".into(),
                    })?;
            }
            other if other.starts_with('-') => {
                return Err(CliError {
                    message: format!("unknown flag `{other}`"),
                });
            }
            other if search.is_none() => search = Some(other.to_string()),
            other => {
                return Err(CliError {
                    message: format!("unexpected argument `{other}`"),
                });
            }
        }
        index += 1;
    }
    if json && watch.is_some() {
        return Err(CliError {
            message: "--json and --watch cannot be combined".into(),
        });
    }
    Ok(defaults(Command::Usage(UsageOptions {
        json,
        watch,
        plain,
        grid,
        search,
    })))
}

/// `p1 workflow run FILE …` (ADR-0053). Only `run` exists; the sub-command is still
/// required so `p1 workflow FILE` is not silently a run.
fn parse_workflow(args: &[String]) -> Result<Options, CliError> {
    const USAGE: &str = "usage: p1 workflow run FILE [--arg K=V]… [--args FILE] \
                         [--role R=E/P[:effort]]… [--resume-from ID] [--out DIR] \
                         [--workspace DIR] [--session FILE] [--max-workers N] [--yes]";
    if args.get(1).map(String::as_str) != Some("run") {
        return Err(CliError {
            message: USAGE.to_string(),
        });
    }
    let pair = |flag: &str, value: String| -> Result<(String, String), CliError> {
        match value.split_once('=') {
            Some((key, value)) if !key.is_empty() => Ok((key.to_string(), value.to_string())),
            _ => Err(CliError {
                message: format!("{flag} requires KEY=VALUE, got `{value}`"),
            }),
        }
    };
    let mut file: Option<PathBuf> = None;
    let mut workflow = WorkflowRunOptions {
        file: PathBuf::new(),
        args: Vec::new(),
        args_file: None,
        roles: Vec::new(),
        resume_from: None,
        out: None,
        max_workers: DEFAULT_MAX_WORKERS,
    };
    let (mut workspace, mut session) = (None, None);
    let mut index = 2;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--arg" => workflow
                .args
                .push(pair(arg, take_value(args, &mut index, arg)?)?),
            "--args" => {
                workflow.args_file = Some(PathBuf::from(take_value(args, &mut index, arg)?))
            }
            "--role" => workflow
                .roles
                .push(pair(arg, take_value(args, &mut index, arg)?)?),
            "--resume-from" => workflow.resume_from = Some(take_value(args, &mut index, arg)?),
            "--out" => workflow.out = Some(PathBuf::from(take_value(args, &mut index, arg)?)),
            "--workspace" => workspace = Some(PathBuf::from(take_value(args, &mut index, arg)?)),
            "--session" => session = Some(PathBuf::from(take_value(args, &mut index, arg)?)),
            "--max-workers" => {
                let value = take_value(args, &mut index, arg)?;
                workflow.max_workers =
                    value
                        .parse::<usize>()
                        .ok()
                        .filter(|n| *n > 0)
                        .ok_or_else(|| CliError {
                            message: "--max-workers requires a positive number".into(),
                        })?;
            }
            // Full access is already the default; accepted like the run's `--yes`.
            "--yes" => {}
            other if other.starts_with('-') => {
                return Err(CliError {
                    message: format!("unknown flag `{other}`"),
                });
            }
            other if file.is_none() => file = Some(PathBuf::from(other)),
            other => {
                return Err(CliError {
                    message: format!("unexpected argument `{other}`"),
                });
            }
        }
        index += 1;
    }
    let Some(file) = file else {
        return Err(CliError {
            message: USAGE.to_string(),
        });
    };
    workflow.file = file;
    let mut options = defaults(Command::WorkflowRun(workflow));
    options.workspace = workspace;
    options.session = session;
    Ok(options)
}

/// `p1 login <route>` and `p1 login --list` (ADR-0044, spec §6). The key is never an
/// argument: it is read from stdin, so arguments would only put it in shell history
/// and in `ps`.
fn parse_login(args: &[String]) -> Result<Options, CliError> {
    const USAGE: &str = "usage: p1 login <route> | p1 login --list";
    match args.get(1).map(String::as_str) {
        None => Err(CliError {
            message: USAGE.to_string(),
        }),
        Some("--list") => {
            if let Some(extra) = args.get(2) {
                return Err(CliError {
                    message: format!("unexpected argument `{extra}`"),
                });
            }
            Ok(defaults(Command::LoginList))
        }
        Some(other) if other.starts_with('-') => Err(CliError {
            message: format!("unknown flag `{other}`"),
        }),
        Some(route) => {
            if let Some(extra) = args.get(2) {
                return Err(CliError {
                    message: format!("unexpected argument `{extra}`"),
                });
            }
            Ok(defaults(Command::Login {
                route: route.to_string(),
            }))
        }
    }
}

/// `p1 logout <route>` (ADR-0044, spec §6).
fn parse_logout(args: &[String]) -> Result<Options, CliError> {
    match args.get(1).map(String::as_str) {
        None => Err(CliError {
            message: "usage: p1 logout <route>".to_string(),
        }),
        Some(other) if other.starts_with('-') => Err(CliError {
            message: format!("unknown flag `{other}`"),
        }),
        Some(route) => {
            if let Some(extra) = args.get(2) {
                return Err(CliError {
                    message: format!("unexpected argument `{extra}`"),
                });
            }
            Ok(defaults(Command::Logout {
                route: route.to_string(),
            }))
        }
    }
}

/// Parse the sandbox flags out of any argument list, ignoring everything else.
///
/// `parse` and `parse_env_show` both use it, so the flag grammar and its usage
/// errors exist once.
fn parse_sandbox(args: &[String]) -> Result<(SandboxMode, Vec<PathBuf>, Vec<PathBuf>), CliError> {
    let mut mode = SandboxMode::Off;
    let mut writable: Vec<PathBuf> = Vec::new();
    let mut readable: Vec<PathBuf> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--sandbox" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(CliError {
                        message: "--sandbox requires a value".to_string(),
                    });
                };
                mode = match value.as_str() {
                    "off" => SandboxMode::Off,
                    "workspace" => SandboxMode::Workspace,
                    other => {
                        return Err(CliError {
                            message: format!(
                                "unknown sandbox mode `{other}`; expected `workspace` or `off`"
                            ),
                        });
                    }
                };
            }
            "--sandbox-write" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(CliError {
                        message: "--sandbox-write requires a value".to_string(),
                    });
                };
                writable.push(resolve_sandbox_path(value, "--sandbox-write")?);
            }
            "--sandbox-read" => {
                index += 1;
                let Some(value) = args.get(index) else {
                    return Err(CliError {
                        message: "--sandbox-read requires a value".to_string(),
                    });
                };
                readable.push(resolve_sandbox_path(value, "--sandbox-read")?);
            }
            _ => {}
        }
        index += 1;
    }
    if !writable.is_empty() && mode == SandboxMode::Off {
        return Err(CliError {
            message: "--sandbox-write requires --sandbox workspace".to_string(),
        });
    }
    if !readable.is_empty() && mode == SandboxMode::Off {
        return Err(CliError {
            message: "--sandbox-read requires --sandbox workspace".to_string(),
        });
    }
    Ok((mode, writable, readable))
}

/// Parse the repeatable `--env-pass NAME` flags out of any argument list.
///
/// `parse` and `parse_env_show` both use it, so the flag grammar and its usage
/// errors exist once. `A=B` is a value, not a name, and an empty name is
/// meaningless, so both are usage errors.
fn parse_env_pass(args: &[String]) -> Result<Vec<String>, CliError> {
    let mut names: Vec<String> = Vec::new();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--env-pass" {
            index += 1;
            let Some(value) = args.get(index) else {
                return Err(CliError {
                    message: "--env-pass requires a value".to_string(),
                });
            };
            if value.is_empty() || value.contains('=') {
                return Err(CliError {
                    message: format!("--env-pass takes a variable NAME without `=`, got `{value}`"),
                });
            }
            names.push(value.clone());
        }
        index += 1;
    }
    Ok(names)
}

/// A `--sandbox-write`/`--sandbox-read` value: absolute, or relative to the
/// current directory; canonicalised when the path exists. A path that does not
/// exist is kept as an absolute path and simply not bound (see `bwrap_args`).
fn resolve_sandbox_path(value: &str, flag: &str) -> Result<PathBuf, CliError> {
    let path = Path::new(value);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| CliError {
                message: format!("cannot resolve {flag} `{value}`: {error}"),
            })?
            .join(path)
    };
    Ok(std::fs::canonicalize(&absolute).unwrap_or(absolute))
}

fn take_value(args: &[String], index: &mut usize, flag: &str) -> Result<String, CliError> {
    *index += 1;
    let Some(value) = args.get(*index) else {
        return Err(CliError {
            message: format!("{flag} requires a value"),
        });
    };
    Ok(value.clone())
}

/// Parse `--max-continuations N` out of any argument list. A missing or
/// non-numeric value is a usage error.
fn parse_max_continuations(args: &[String]) -> Result<usize, CliError> {
    let mut value = DEFAULT_MAX_CONTINUATIONS;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--max-continuations" {
            index += 1;
            let Some(raw) = args.get(index) else {
                return Err(CliError {
                    message: "--max-continuations requires a value".to_string(),
                });
            };
            value = raw.parse::<usize>().map_err(|_| CliError {
                message: format!("--max-continuations takes a non-negative integer, got `{raw}`"),
            })?;
        }
        index += 1;
    }
    Ok(value)
}

/// Parse `--provider-retries N` out of any argument list. A missing or
/// non-numeric value is a usage error.
fn parse_provider_retries(args: &[String]) -> Result<usize, CliError> {
    let mut value = DEFAULT_PROVIDER_RETRIES;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--provider-retries" {
            index += 1;
            let Some(raw) = args.get(index) else {
                return Err(CliError {
                    message: "--provider-retries requires a value".to_string(),
                });
            };
            value = raw.parse::<usize>().map_err(|_| CliError {
                message: format!("--provider-retries takes a non-negative integer, got `{raw}`"),
            })?;
        }
        index += 1;
    }
    Ok(value)
}

/// Parse `--max-idle-summaries N` out of any argument list. A missing or
/// non-numeric value is a usage error.
fn parse_max_idle_summaries(args: &[String]) -> Result<usize, CliError> {
    let mut value = DEFAULT_MAX_IDLE_SUMMARIES;
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--max-idle-summaries" {
            index += 1;
            let Some(raw) = args.get(index) else {
                return Err(CliError {
                    message: "--max-idle-summaries requires a value".to_string(),
                });
            };
            value = raw.parse::<usize>().map_err(|_| CliError {
                message: format!("--max-idle-summaries takes a non-negative integer, got `{raw}`"),
            })?;
        }
        index += 1;
    }
    Ok(value)
}

fn defaults(command: Command) -> Options {
    Options {
        command,
        env: DEFAULT_ENV.to_string(),
        env_given: false,
        model: None,
        effort: None,
        models: None,
        workspace: None,
        session: None,
        resume: false,
        ask: false,
        tui: false,
        sandbox: SandboxMode::Off,
        sandbox_write: Vec::new(),
        sandbox_read: Vec::new(),
        env_pass: Vec::new(),
        max_continuations: DEFAULT_MAX_CONTINUATIONS,
        provider_retries: DEFAULT_PROVIDER_RETRIES,
        max_idle_summaries: DEFAULT_MAX_IDLE_SUMMARIES,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(parts: &[&str]) -> Vec<String> {
        parts.iter().map(|part| part.to_string()).collect()
    }

    #[test]
    fn parses_the_headless_form() {
        let options = parse(&args(&["--env", "gpt", "--workspace", "/w", "do", "it"])).unwrap();
        assert_eq!(options.env, "gpt");
        assert_eq!(options.workspace, Some(PathBuf::from("/w")));
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("do it".to_string())
            }
        );
        assert!(options.is_headless());
    }

    #[test]
    fn no_prompt_is_interactive_and_default_env_is_claude() {
        let options = parse(&args(&[])).unwrap();
        assert_eq!(options.env, "claude");
        assert_eq!(options.command, Command::Run { prompt: None });
        assert!(!options.is_headless());
    }

    #[test]
    fn parses_env_show_and_the_meta_flags() {
        assert_eq!(
            parse(&args(&["env", "show", "gpt"])).unwrap().command,
            Command::EnvShow {
                name: "gpt".to_string()
            }
        );
        assert_eq!(parse(&args(&["--help"])).unwrap().command, Command::Help);
        assert_eq!(
            parse(&args(&["--version"])).unwrap().command,
            Command::Version
        );
    }

    #[test]
    fn parses_login_and_logout() {
        // Route ids here are neutral: a real route's endpoint, header or model name
        // compiled into this crate is a load error the route tests guard against.
        assert_eq!(
            parse(&args(&["login", "a-route"])).unwrap().command,
            Command::Login {
                route: "a-route".to_string()
            }
        );
        assert_eq!(
            parse(&args(&["login", "--list"])).unwrap().command,
            Command::LoginList
        );
        assert_eq!(
            parse(&args(&["logout", "another-route"])).unwrap().command,
            Command::Logout {
                route: "another-route".to_string()
            }
        );

        assert!(parse(&args(&["login"])).is_err());
        assert!(parse(&args(&["logout"])).is_err());
        assert!(parse(&args(&["login", "--bogus"])).is_err());
        assert!(parse(&args(&["login", "-"])).is_err());
        assert!(parse(&args(&["login", "a", "b"])).is_err());
        assert!(parse(&args(&["login", "--list", "a"])).is_err());
        assert!(parse(&args(&["logout", "--list"])).is_err());
        assert!(parse(&args(&["logout", "a", "b"])).is_err());

        assert!(usage().contains("p1 login <route>"));
        assert!(usage().contains("p1 login --list"));
        assert!(usage().contains("p1 logout <route>"));
    }

    #[test]
    fn unknown_flags_and_missing_values_are_usage_errors() {
        assert!(parse(&args(&["--bogus"])).is_err());
        assert!(parse(&args(&["--env"])).is_err());
        assert!(parse(&args(&["--resume"])).is_err());
        assert!(parse(&args(&["env", "show"])).is_err());
        assert!(parse(&args(&["env", "list"])).is_err());
    }

    #[test]
    fn parses_the_sandbox_flags_and_keeps_the_prompt() {
        let options = parse(&args(&["--sandbox", "workspace"])).unwrap();
        assert_eq!(options.sandbox, SandboxMode::Workspace);
        assert!(options.sandbox_write.is_empty());
        assert!(options.sandbox_read.is_empty());

        assert_eq!(
            parse(&args(&["--sandbox", "off"])).unwrap().sandbox,
            SandboxMode::Off
        );
        // A relative --sandbox-write is canonicalised against the current dir.
        let options = parse(&args(&["--sandbox", "workspace", "--sandbox-write", "."])).unwrap();
        assert_eq!(
            options.sandbox_write,
            vec![std::env::current_dir().unwrap().canonicalize().unwrap()]
        );
        // A relative --sandbox-read is resolved the same way.
        let options = parse(&args(&["--sandbox", "workspace", "--sandbox-read", "."])).unwrap();
        assert_eq!(
            options.sandbox_read,
            vec![std::env::current_dir().unwrap().canonicalize().unwrap()]
        );
        let options = parse(&args(&["--sandbox", "workspace", "--yes", "do", "it"])).unwrap();
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("do it".to_string())
            }
        );
        assert!(!options.ask, "--yes means the default, so no asking");
    }

    #[test]
    fn ask_opts_into_the_restrictive_policy() {
        let options = parse(&args(&["--ask", "do", "it"])).unwrap();
        assert!(options.ask);
        assert!(!parse(&args(&["do", "it"])).unwrap().ask);
        assert!(usage().contains("--ask"));
        assert!(usage().contains("default: full access"));
    }

    #[test]
    fn yes_with_ask_is_a_usage_error() {
        let error = parse(&args(&["--yes", "--ask", "go"])).unwrap_err();
        assert!(error.message.contains("--ask"), "{}", error.message);
        assert!(parse(&args(&["--ask", "--yes", "go"])).is_err());
    }

    #[test]
    fn parses_env_pass_and_rejects_a_malformed_name() {
        let options = parse(&args(&["--env-pass", "A", "--env-pass", "B", "go"])).unwrap();
        assert_eq!(options.env_pass, ["A", "B"]);
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("go".to_string())
            }
        );

        assert!(parse(&args(&["--env-pass", "A=B"])).is_err());
        assert!(parse(&args(&["--env-pass", ""])).is_err());
        assert!(parse(&args(&["--env-pass"])).is_err());
    }

    #[test]
    fn parses_max_continuations_and_rejects_a_non_numeric_value() {
        let options = parse(&args(&["--max-continuations", "5", "go"])).unwrap();
        assert_eq!(options.max_continuations, 5);
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("go".to_string())
            }
        );
        assert_eq!(parse(&args(&[])).unwrap().max_continuations, 3);
        assert_eq!(
            parse(&args(&["--max-continuations", "0"]))
                .unwrap()
                .max_continuations,
            0
        );

        assert!(parse(&args(&["--max-continuations", "many"])).is_err());
        assert!(parse(&args(&["--max-continuations"])).is_err());
        assert!(usage().contains("--max-continuations N"));
    }

    #[test]
    fn parses_provider_retries_and_rejects_a_non_numeric_value() {
        let options = parse(&args(&["--provider-retries", "7", "go"])).unwrap();
        assert_eq!(options.provider_retries, 7);
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("go".to_string())
            }
        );
        assert_eq!(parse(&args(&[])).unwrap().provider_retries, 3);
        assert_eq!(
            parse(&args(&["--provider-retries", "0"]))
                .unwrap()
                .provider_retries,
            0
        );

        assert!(parse(&args(&["--provider-retries", "many"])).is_err());
        assert!(parse(&args(&["--provider-retries"])).is_err());
        assert!(usage().contains("--provider-retries N"));
        assert!(
            parse(&args(&["--provider-retries", "1", "do", "it"]))
                .unwrap()
                .is_headless(),
            "the flag value is not taken for a prompt word"
        );
    }

    #[test]
    fn parses_max_idle_summaries_and_rejects_a_non_numeric_value() {
        let options = parse(&args(&["--max-idle-summaries", "7", "go"])).unwrap();
        assert_eq!(options.max_idle_summaries, 7);
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("go".to_string())
            }
        );
        assert_eq!(parse(&args(&[])).unwrap().max_idle_summaries, 6);
        assert_eq!(
            parse(&args(&["--max-idle-summaries", "0"]))
                .unwrap()
                .max_idle_summaries,
            0
        );

        assert!(parse(&args(&["--max-idle-summaries", "many"])).is_err());
        assert!(parse(&args(&["--max-idle-summaries"])).is_err());
        assert!(usage().contains("--max-idle-summaries N"));
        assert!(
            parse(&args(&["--max-idle-summaries", "2", "do", "it"]))
                .unwrap()
                .is_headless(),
            "the flag value is not taken for a prompt word"
        );
    }

    #[test]
    fn sandbox_flag_misuse_is_a_usage_error() {
        assert!(parse(&args(&["--sandbox-write", "/tmp", "go"])).is_err());
        assert!(parse(&args(&["--sandbox-read", "/tmp", "go"])).is_err());
        assert!(parse(&args(&["--sandbox", "bogus"])).is_err());
        assert!(parse(&args(&["--sandbox"])).is_err());
        assert!(parse(&args(&["--sandbox-read"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--sandbox-write", "/tmp"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--sandbox-read", "/tmp"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--bogus"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--sandbox", "workspace"])).is_ok());
        // `env show` accepts both read and write alongside the mode.
        assert!(
            parse(&args(&[
                "env",
                "show",
                "claude",
                "--sandbox",
                "workspace",
                "--sandbox-read",
                ".",
            ]))
            .is_ok()
        );
    }

    /// `--sandbox-read` names itself in both of its usage errors, exactly as
    /// `--sandbox-write` does.
    #[test]
    fn sandbox_read_usage_errors_name_the_flag() {
        let error = parse(&args(&["--sandbox-read", "/tmp", "go"])).unwrap_err();
        assert_eq!(error.message, "--sandbox-read requires --sandbox workspace");
        let error = parse(&args(&["--sandbox", "workspace", "--sandbox-read"])).unwrap_err();
        assert_eq!(error.message, "--sandbox-read requires a value");
        assert!(usage().contains("--sandbox-read PATH"));
    }

    #[test]
    fn parses_the_model_flags_and_keeps_the_prompt() {
        let options = parse(&args(&["--model", "claude/claude-opus-5", "go"])).unwrap();
        assert_eq!(options.model.as_deref(), Some("claude/claude-opus-5"));
        assert_eq!(options.effort, None);
        assert_eq!(options.models, None);
        assert!(!options.env_given, "no --env was given");
        assert_eq!(options.env, DEFAULT_ENV);
        assert_eq!(
            options.command,
            Command::Run {
                prompt: Some("go".to_string())
            }
        );

        let options = parse(&args(&[
            "--env",
            "claude",
            "--model",
            "claude/claude-opus-5:high",
            "--effort",
            "extra_high",
            "--models",
            "claude/*,gpt/gpt-5.6-sol*",
            "go",
        ]))
        .unwrap();
        assert!(options.env_given);
        assert_eq!(
            options.model.as_deref(),
            Some("claude/claude-opus-5:high"),
            "the reference is kept verbatim: `:effort` is resolved with the routes"
        );
        assert_eq!(options.effort, Some(Effort::ExtraHigh));
        assert_eq!(options.models.as_deref(), Some("claude/*,gpt/gpt-5.6-sol*"));
        assert!(
            options.is_headless(),
            "the flag values are not prompt words"
        );

        // `--effort` alone selects nothing but the effort of whatever runs.
        let options = parse(&args(&["--effort", "max"])).unwrap();
        assert_eq!(options.effort, Some(Effort::Max));
        assert_eq!(options.model, None);
    }

    #[test]
    fn an_unknown_effort_or_a_missing_value_is_a_usage_error() {
        let error = parse(&args(&["--effort", "loud"])).unwrap_err();
        assert!(error.message.contains("unknown effort `loud`"), "{error}");
        assert!(error.message.contains("extra_high"), "{error}");
        assert!(parse(&args(&["--effort"])).is_err());
        assert!(parse(&args(&["--model"])).is_err());
        assert!(parse(&args(&["--models"])).is_err());
        assert!(usage().contains("--model REF"));
        assert!(usage().contains("--effort LEVEL"));
        assert!(usage().contains("--models PATTERNS"));
    }

    /// `--env` and `--model` name the same choice twice; a pair that names another
    /// environment is a usage error before anything is loaded.
    #[test]
    fn an_env_that_disagrees_with_a_model_pair_is_a_usage_error() {
        let error = parse(&args(&["--env", "claude", "--model", "gpt/gpt-5.6-sol"])).unwrap_err();
        assert!(error.message.contains("--env `claude`"), "{error}");
        assert!(
            error.message.contains("--model `gpt/gpt-5.6-sol`"),
            "{error}"
        );

        // Agreeing forms parse; a bare profile is left to the routes.
        assert!(parse(&args(&["--env", "gpt", "--model", "gpt/gpt-5.5"])).is_ok());
        assert!(parse(&args(&["--env", "claude", "--model", "claude-opus-5"])).is_ok());
        assert!(parse(&args(&["--model", "claude/claude-opus-5"])).is_ok());
    }

    #[test]
    fn parses_usage_subcommand() {
        let options = parse(&args(&[
            "usage", "--plain", "--grid", "48", "--watch", "5", "claude",
        ]))
        .unwrap();
        assert_eq!(
            options.command,
            Command::Usage(UsageOptions {
                plain: true,
                grid: 48,
                watch: Some(5),
                json: false,
                search: Some("claude".into()),
            })
        );
        assert!(!options.is_headless());
        assert!(matches!(
            parse(&args(&["usage", "--json"])).unwrap().command,
            Command::Usage(UsageOptions { json: true, .. })
        ));
        assert!(parse(&args(&["usage", "--json", "--watch", "5"])).is_err());
        assert!(parse(&args(&["usage", "--watch", "4"])).is_err());
        assert!(parse(&args(&["usage", "--watch", "oops"])).is_err());
        assert!(parse(&args(&["usage", "--grid", "oops"])).is_err());
        assert!(parse(&args(&["usage", "--bogus"])).is_err());
        assert!(parse(&args(&["usage", "a", "b"])).is_err());
        assert!(usage().contains("p1 usage [--json]"));
        assert_eq!(parse(&args(&["help"])).unwrap().command, Command::Help);
    }

    #[test]
    fn parses_the_models_subcommand() {
        assert_eq!(
            parse(&args(&["models"])).unwrap().command,
            Command::Models { search: None }
        );
        let options = parse(&args(&["models", "gpt/"])).unwrap();
        assert_eq!(
            options.command,
            Command::Models {
                search: Some("gpt/".to_string())
            }
        );
        assert_eq!(
            parse(&args(&["models", "--models", "gpt/*"]))
                .unwrap()
                .models
                .as_deref(),
            Some("gpt/*")
        );

        assert!(parse(&args(&["models", "a", "b"])).is_err());
        assert!(parse(&args(&["models", "--bogus"])).is_err());
        assert!(parse(&args(&["models", "--models"])).is_err());
        assert!(usage().contains("p1 models [SEARCH]"));
    }

    /// A lone word that is a near miss for a subcommand is a usage error naming the
    /// command meant, never a prompt (issue #83).
    #[test]
    fn a_lone_near_miss_for_a_subcommand_is_a_usage_error() {
        for (typo, meant) in [
            ("envs", "env"),
            ("model", "models"),
            ("usge", "usage"),
            ("workfow", "workflow"),
            ("logi", "login"),
            ("loguot", "logout"),
        ] {
            let error = parse(&args(&[typo])).unwrap_err();
            assert!(
                error.message.contains(&format!("unknown command '{typo}'")),
                "{typo}: {}",
                error.message
            );
            assert!(
                error.message.contains(&format!("did you mean '{meant}'?")),
                "{typo}: {}",
                error.message
            );
            assert!(
                error.message.contains(&format!("p1 -- {typo}")),
                "{typo}: {}",
                error.message
            );
        }
    }

    /// Several words, a far-off single word, a capitalized word and `--` are all
    /// prompts: only a lone near miss is an error.
    #[test]
    fn a_prompt_that_is_not_a_near_miss_is_kept() {
        let prompt = |options: Options| match options.command {
            Command::Run { prompt } => prompt,
            other => panic!("expected a run, got {other:?}"),
        };

        assert_eq!(
            prompt(parse(&args(&["fix", "the", "bug"])).unwrap()),
            Some("fix the bug".to_string())
        );
        assert_eq!(
            prompt(parse(&args(&["refactor"])).unwrap()),
            Some("refactor".to_string())
        );
        // The shape guard: capitals, digits and punctuation are never a command.
        assert_eq!(
            prompt(parse(&args(&["Envs"])).unwrap()),
            Some("Envs".to_string())
        );
        // `--` ends parsing: the token is the prompt, typo or not.
        assert_eq!(
            prompt(parse(&args(&["--", "envs"])).unwrap()),
            Some("envs".to_string())
        );
        // An exact subcommand after a flag is a prompt, not a typo.
        assert_eq!(
            prompt(parse(&args(&["--ask", "models"])).unwrap()),
            Some("models".to_string())
        );
    }

    /// The help text documents `--`.
    #[test]
    fn help_documents_the_double_dash() {
        let usage = usage();
        assert!(usage.contains("  --  "), "{usage}");
        assert!(usage.contains("p1 -- envs"), "{usage}");
    }
}
