//! Every directory and environment variable the chain may touch (spec §2).
//!
//! A [`Locations`] is the whole outside world a credential lookup sees: tests build
//! one by hand and therefore never read the real home or the real environment, and
//! [`Locations::from_process`] is the production value.

use std::ffi::OsString;
use std::path::PathBuf;
use std::sync::Arc;

/// A directory or variable lookup, or `None` for "not set".
pub(crate) type EnvLookup = Arc<dyn Fn(&str) -> Option<String> + Send + Sync>;

#[derive(Clone)]
pub struct Locations {
    home: Option<PathBuf>,
    xdg_config_home: Option<PathBuf>,
    xdg_data_home: Option<PathBuf>,
    pi_agent_dir: Option<PathBuf>,
    env: EnvLookup,
}

impl Locations {
    /// No location at all, and an environment that has nothing in it: the starting
    /// point of an explicit (test) environment.
    pub fn none() -> Self {
        Self {
            home: None,
            xdg_config_home: None,
            xdg_data_home: None,
            pi_agent_dir: None,
            env: Arc::new(|_| None),
        }
    }

    /// The production value: `HOME`, `XDG_CONFIG_HOME`, `XDG_DATA_HOME` and
    /// `PI_CODING_AGENT_DIR` as the process has them, plus the process environment
    /// for everything else (`CLAUDE_CONFIG_DIR`, `CODEX_HOME`, the route's variable).
    pub fn from_process() -> Self {
        Self::from_environment(std::env::vars_os())
    }

    /// A value built from an explicit environment, never from the process one. An
    /// injected snapshot (a test, a host that already has one) goes through here.
    pub fn from_environment(vars: impl IntoIterator<Item = (OsString, OsString)>) -> Self {
        let mut locations = Self::none();
        let vars: Vec<(OsString, OsString)> = vars.into_iter().collect();
        let lookup = vars.clone();
        let get = move |name: &str| {
            lookup
                .iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.to_string_lossy().into_owned())
        };
        locations.home = dir(get("HOME"));
        locations.xdg_config_home = dir(get("XDG_CONFIG_HOME"));
        locations.xdg_data_home = dir(get("XDG_DATA_HOME"));
        locations.pi_agent_dir = dir(get("PI_CODING_AGENT_DIR"));
        locations.env = Arc::new(get);
        locations
    }

    /// The home directory, or `None` when the host has none.
    pub fn with_home(mut self, home: Option<PathBuf>) -> Self {
        self.home = home;
        self
    }

    /// The p1 config directory (`$XDG_CONFIG_HOME`).
    pub fn with_xdg_config_home(mut self, dir: Option<PathBuf>) -> Self {
        self.xdg_config_home = dir;
        self
    }

    /// The p1 data directory (`$XDG_DATA_HOME`).
    pub fn with_xdg_data_home(mut self, dir: Option<PathBuf>) -> Self {
        self.xdg_data_home = dir;
        self
    }

    /// The Pi CLI's agent directory (`$PI_CODING_AGENT_DIR`).
    pub fn with_pi_agent_dir(mut self, dir: Option<PathBuf>) -> Self {
        self.pi_agent_dir = dir;
        self
    }

    /// Replace the environment lookup (a mutable one lets a test make a variable
    /// appear between two `access` calls).
    pub fn with_env_lookup(
        mut self,
        env: impl Fn(&str) -> Option<String> + Send + Sync + 'static,
    ) -> Self {
        self.env = Arc::new(env);
        self
    }

    /// One variable, exactly as the environment has it (an empty value is `Some("")`).
    pub fn env(&self, name: &str) -> Option<String> {
        (self.env)(name)
    }

    /// The environment lookup itself, for a source that re-reads a variable on
    /// every access (a variable that appears between two calls must be seen).
    pub(crate) fn lookup(&self) -> EnvLookup {
        self.env.clone()
    }

    /// The p1 store: `$XDG_CONFIG_HOME/p1/auth.json`, else `~/.config/p1/auth.json`.
    pub(crate) fn p1_store_path(&self) -> Option<PathBuf> {
        self.xdg_config_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".config")))
            .map(|config| config.join("p1").join("auth.json"))
    }

    /// OpenCode's credential file: `$XDG_DATA_HOME/opencode/auth.json`, else
    /// `~/.local/share/opencode/auth.json`.
    pub(crate) fn opencode_login_path(&self) -> Option<PathBuf> {
        self.xdg_data_home
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".local/share")))
            .map(|data| data.join("opencode").join("auth.json"))
    }

    /// Pi's credential file: `$PI_CODING_AGENT_DIR/auth.json`, else
    /// `~/.pi/agent/auth.json`.
    pub(crate) fn pi_login_path(&self) -> Option<PathBuf> {
        self.pi_agent_dir
            .clone()
            .or_else(|| self.home.as_ref().map(|home| home.join(".pi/agent")))
            .map(|dir| dir.join("auth.json"))
    }

    /// Claude Code's credential file: `<login_dir>/.credentials.json` when the route
    /// names a directory (ADR-0075), else `$CLAUDE_CONFIG_DIR/.credentials.json`, else
    /// `~/.claude/.credentials.json`.
    pub(crate) fn claude_code_path(&self, login_dir: Option<&str>) -> Option<PathBuf> {
        self.claude_code_dir(login_dir)
            .map(|dir| dir.join(".credentials.json"))
    }

    /// The Claude Code config directory a route borrows its login from: the route's
    /// `login_dir` with a leading `~` expanded against the home directory, else
    /// `$CLAUDE_CONFIG_DIR`, else `~/.claude`. `None` when the directory needs a home
    /// and the host has none.
    pub fn claude_code_dir(&self, login_dir: Option<&str>) -> Option<PathBuf> {
        match login_dir {
            Some(dir) => self.expand_home(dir),
            None => self
                .tool_dir("CLAUDE_CONFIG_DIR")
                .or_else(|| self.home.as_ref().map(|home| home.join(".claude"))),
        }
    }

    /// A path with a leading `~` (alone or `~/…`) expanded against the home directory.
    /// Any other path is taken as written.
    pub fn expand_home(&self, path: &str) -> Option<PathBuf> {
        if path == "~" {
            return self.home.clone();
        }
        match path.strip_prefix("~/") {
            Some(rest) => self.home.as_ref().map(|home| home.join(rest)),
            None => Some(PathBuf::from(path)),
        }
    }

    /// The Codex CLI's auth file: `$CODEX_HOME/auth.json`, else
    /// `~/.codex/auth.json`.
    pub(crate) fn codex_path(&self) -> Option<PathBuf> {
        self.tool_dir("CODEX_HOME")
            .or_else(|| self.home.as_ref().map(|home| home.join(".codex")))
            .map(|dir| dir.join("auth.json"))
    }

    /// A per-tool override directory from the environment. Blank means "not set",
    /// exactly as the borrowed sources read it before.
    fn tool_dir(&self, name: &str) -> Option<PathBuf> {
        dir(self.env(name))
    }
}

/// A directory variable: a blank value counts as unset, exactly as the borrowed
/// sources read it before.
fn dir(value: Option<String>) -> Option<PathBuf> {
    value.filter(|value| !value.is_empty()).map(PathBuf::from)
}
