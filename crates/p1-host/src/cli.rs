//! Hand-written argument parsing. No clap: the surface is tiny and the error
//! messages are part of the interface.
//!
//! `p1 [--env NAME] [--workspace DIR] [--session FILE] [--resume] [--ask] [PROMPT…]`
//! `p1 env show NAME`
//! `p1 --help` / `p1 --version`

use std::path::{Path, PathBuf};

/// The default environment when `--env` is not given.
pub const DEFAULT_ENV: &str = "claude";

/// Headless continuation budget when `--max-continuations` is not given.
pub const DEFAULT_MAX_CONTINUATIONS: usize = 3;

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
    Help,
    Version,
}

/// Parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Options {
    pub command: Command,
    pub env: String,
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
    /// Extra environment variable NAMES the `shell` tool passes on, on top of
    /// its built-in allow-list. Repeatable; a name never contains `=`.
    pub env_pass: Vec<String>,
    /// At most this many continuations after a premature stop in an unattended
    /// run. `0` disables continuation.
    pub max_continuations: usize,
}

impl Options {
    /// Headless means a PROMPT was supplied. Used by the authorization policy.
    pub fn is_headless(&self) -> bool {
        matches!(self.command, Command::Run { prompt: Some(_) })
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
        "  p1 [--env NAME] [--workspace DIR] [--session FILE] [--resume] [--ask] [PROMPT…]\n",
    );
    out.push_str("  p1 env show NAME\n");
    out.push_str("  p1 --help\n");
    out.push_str("  p1 --version\n\n");
    out.push_str("flags:\n");
    out.push_str("  --env NAME        environment to run (default: claude)\n");
    out.push_str("  --workspace DIR   workspace root (default: current directory)\n");
    out.push_str("  --session FILE    write the session journal to FILE as JSONL\n");
    out.push_str("  --resume          continue an existing --session file\n");
    out.push_str(
        "  --ask             ask before permitting a tool call; headless permits only\n                    read-only calls (default: full access, no questions)\n",
    );
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
        "  --env-pass NAME   pass NAME from p1's environment to shell commands\n                    (repeatable; NAME must not contain `=`)\n",
    );
    out.push_str(
        "  --max-continuations N\n                    most continuations after a premature stop in an unattended\n                    run (default: 3; 0 disables)\n",
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
        if first == "--help" || first == "-h" {
            return Ok(defaults(Command::Help));
        }
        if first == "--version" || first == "-V" {
            return Ok(defaults(Command::Version));
        }
        if first == "env" {
            return parse_env_show(args);
        }
    }

    let mut env: Option<String> = None;
    let mut workspace: Option<PathBuf> = None;
    let mut session: Option<PathBuf> = None;
    let mut resume = false;
    let mut ask = false;
    let mut yes = false;
    let mut prompt_words: Vec<String> = Vec::new();

    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        match arg {
            "--env" => {
                let value = take_value(args, &mut index, "--env")?;
                env = Some(value);
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
            // Validated by `parse_sandbox`/`parse_env_pass`/`parse_max_continuations`
            // after the loop; the values are consumed here so they are not mistaken
            // for prompt words.
            "--sandbox" | "--sandbox-write" | "--env-pass" | "--max-continuations" => {
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
    let (sandbox, sandbox_write) = parse_sandbox(args)?;
    let env_pass = parse_env_pass(args)?;
    let max_continuations = parse_max_continuations(args)?;

    let prompt = if prompt_words.is_empty() {
        None
    } else {
        Some(prompt_words.join(" "))
    };

    Ok(Options {
        command: Command::Run { prompt },
        env: env.unwrap_or_else(|| DEFAULT_ENV.to_string()),
        workspace,
        session,
        resume,
        ask,
        sandbox,
        sandbox_write,
        env_pass,
        max_continuations,
    })
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
            "--sandbox" | "--sandbox-write" | "--env-pass" => index += 1,
            other => {
                return Err(CliError {
                    message: format!("unexpected argument `{other}`"),
                });
            }
        }
        index += 1;
    }
    let (sandbox, sandbox_write) = parse_sandbox(args)?;
    let env_pass = parse_env_pass(args)?;
    Ok(Options {
        command: Command::EnvShow { name: name.clone() },
        env: name.clone(),
        workspace: None,
        session: None,
        resume: false,
        ask: false,
        sandbox,
        sandbox_write,
        env_pass,
        max_continuations: DEFAULT_MAX_CONTINUATIONS,
    })
}

/// Parse the sandbox flags out of any argument list, ignoring everything else.
///
/// `parse` and `parse_env_show` both use it, so the flag grammar and its usage
/// errors exist once.
fn parse_sandbox(args: &[String]) -> Result<(SandboxMode, Vec<PathBuf>), CliError> {
    let mut mode = SandboxMode::Off;
    let mut writable: Vec<PathBuf> = Vec::new();
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
                writable.push(resolve_writable(value)?);
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
    Ok((mode, writable))
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

/// A `--sandbox-write` value: absolute, or relative to the current directory;
/// canonicalised when the path exists. A path that does not exist is kept as an
/// absolute path and simply not bound (see `bwrap_args`).
fn resolve_writable(value: &str) -> Result<PathBuf, CliError> {
    let path = Path::new(value);
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|error| CliError {
                message: format!("cannot resolve --sandbox-write `{value}`: {error}"),
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

fn defaults(command: Command) -> Options {
    Options {
        command,
        env: DEFAULT_ENV.to_string(),
        workspace: None,
        session: None,
        resume: false,
        ask: false,
        sandbox: SandboxMode::Off,
        sandbox_write: Vec::new(),
        env_pass: Vec::new(),
        max_continuations: DEFAULT_MAX_CONTINUATIONS,
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
    fn sandbox_flag_misuse_is_a_usage_error() {
        assert!(parse(&args(&["--sandbox-write", "/tmp", "go"])).is_err());
        assert!(parse(&args(&["--sandbox", "bogus"])).is_err());
        assert!(parse(&args(&["--sandbox"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--sandbox-write", "/tmp"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--bogus"])).is_err());
        assert!(parse(&args(&["env", "show", "claude", "--sandbox", "workspace"])).is_ok());
    }
}
