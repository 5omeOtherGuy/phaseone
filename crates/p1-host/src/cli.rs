//! Hand-written argument parsing. No clap: the surface is tiny and the error
//! messages are part of the interface.
//!
//! `p1 [--env NAME] [--workspace DIR] [--session FILE] [--resume] [--yes] [PROMPT…]`
//! `p1 env show NAME`
//! `p1 --help` / `p1 --version`

use std::path::PathBuf;

/// The default environment when `--env` is not given.
pub const DEFAULT_ENV: &str = "claude";

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
    pub yes: bool,
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
        "  p1 [--env NAME] [--workspace DIR] [--session FILE] [--resume] [--yes] [PROMPT…]\n",
    );
    out.push_str("  p1 env show NAME\n");
    out.push_str("  p1 --help\n");
    out.push_str("  p1 --version\n\n");
    out.push_str("flags:\n");
    out.push_str("  --env NAME        environment to run (default: claude)\n");
    out.push_str("  --workspace DIR   workspace root (default: current directory)\n");
    out.push_str("  --session FILE    write the session journal to FILE as JSONL\n");
    out.push_str("  --resume          continue an existing --session file\n");
    out.push_str("  --yes             permit every tool call without asking\n");
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
            "--yes" => yes = true,
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
        yes,
    })
}

fn parse_env_show(args: &[String]) -> Result<Options, CliError> {
    // Exactly `env show NAME`; no flags, no extra arguments.
    let [_, show, name] = args else {
        return Err(CliError {
            message: "usage: p1 env show NAME".to_string(),
        });
    };
    if show != "show" {
        return Err(CliError {
            message: format!("unknown env subcommand `{show}`"),
        });
    }
    if name.starts_with('-') {
        return Err(CliError {
            message: "usage: p1 env show NAME".to_string(),
        });
    }
    Ok(Options {
        command: Command::EnvShow { name: name.clone() },
        env: name.clone(),
        workspace: None,
        session: None,
        resume: false,
        yes: false,
    })
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

fn defaults(command: Command) -> Options {
    Options {
        command,
        env: DEFAULT_ENV.to_string(),
        workspace: None,
        session: None,
        resume: false,
        yes: false,
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
}
