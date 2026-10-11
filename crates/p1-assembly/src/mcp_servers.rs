//! `[[mcp_servers]]` (ADR-0181): the MCP servers an environment names. The environment names a
//! server and how to reach it, never its tools; the host connects at session start and presents
//! what the server serves. A guest never names a server.
//!
//! ```toml
//! [[mcp_servers]]
//! name = "docs"                 # tools become `mcp__docs__<tool>`
//! command = "docs-mcp-server"   # stdio: the child gets only what is named here
//! args = ["--stdio"]
//! env = { DOCS_MODE = "local" } # literal values, never credentials
//! env_pass = ["DOCS_TOKEN"]     # variables copied from p1's own environment
//! cwd = "tools/docs"            # relative to the workspace
//!
//! [[mcp_servers]]
//! name = "tracker"
//! url = "https://tracker.example/mcp"  # streamable HTTP
//! headers = { "X-Team" = "core" }      # literal values, never credentials
//! bearer_token_env = "TRACKER_TOKEN"   # `Authorization: Bearer $TRACKER_TOKEN`
//! ```
//!
//! A credential has exactly two ways in, both by NAME of a variable of p1's own environment
//! (`env_pass`, `bearer_token_env`), so the file never holds a credential value and the host
//! can register every value it passes as a secret to mask.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use serde::Deserialize;

use crate::config_reader::ConfigReader;
use crate::{AssemblyError, ENVIRONMENT_FILE};

/// The longest server name: `mcp__` + 32 + `__` leaves 25 characters of a 64-character tool
/// name for the server's own tool name.
pub const MAX_SERVER_NAME_LEN: usize = 32;

/// Headers the streamable HTTP transport sets itself; an entry cannot override them.
const RESERVED_HEADERS: &[&str] = &[
    "accept",
    "content-type",
    "content-length",
    "host",
    "mcp-session-id",
    "mcp-protocol-version",
];

/// One validated `[[mcp_servers]]` entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct McpServerSpec {
    pub name: String,
    pub transport: McpServerTransport,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum McpServerTransport {
    Stdio {
        command: String,
        args: Vec<String>,
        /// Literal values the child gets.
        env: BTreeMap<String, String>,
        /// Names of p1's own variables the child gets too.
        env_pass: Vec<String>,
        /// A relative path resolves against the workspace; the workspace when absent.
        cwd: Option<PathBuf>,
    },
    Http {
        url: String,
        headers: BTreeMap<String, String>,
        /// The name of p1's own variable whose value is sent as a bearer token.
        bearer_token_env: Option<String>,
    },
}

/// The TOML surface of one entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct McpServerToml {
    name: String,
    command: Option<String>,
    args: Option<Vec<String>>,
    env: Option<BTreeMap<String, String>>,
    env_pass: Option<Vec<String>>,
    cwd: Option<String>,
    url: Option<String>,
    headers: Option<BTreeMap<String, String>>,
    bearer_token_env: Option<String>,
}

/// Validate every entry; the error names the entry and the rule it breaks.
pub(crate) fn validate(entries: &[McpServerToml]) -> Result<Vec<McpServerSpec>, String> {
    let mut names = BTreeSet::new();
    let mut specs = Vec::with_capacity(entries.len());
    for entry in entries {
        let name = &entry.name;
        let invalid = |rule: &str| format!("[[mcp_servers]] `{name}`: {rule}");
        if !server_name(name) {
            return Err(invalid(&format!(
                "a server name is 1 to {MAX_SERVER_NAME_LEN} lowercase letters, digits, `-` \
                 and `_`, starts with a letter or digit and has no `__`"
            )));
        }
        if !names.insert(name.clone()) {
            return Err(invalid("the name is used by another server"));
        }
        let transport = match (&entry.command, &entry.url) {
            (Some(command), None) => {
                if entry.headers.is_some() || entry.bearer_token_env.is_some() {
                    return Err(invalid(
                        "`headers` and `bearer_token_env` belong to a `url` server",
                    ));
                }
                if command.trim().is_empty() {
                    return Err(invalid("`command` is empty"));
                }
                let env = entry.env.clone().unwrap_or_default();
                for (variable, value) in &env {
                    if !variable_name(variable) {
                        return Err(invalid(&format!("`{variable}` is not a variable name")));
                    }
                    if carries_credential(value) {
                        return Err(invalid(&format!(
                            "the value of `{variable}` looks like a credential; name the \
                             variable that holds it in `env_pass` instead"
                        )));
                    }
                }
                let env_pass = entry.env_pass.clone().unwrap_or_default();
                if let Some(variable) = env_pass.iter().find(|variable| !variable_name(variable)) {
                    return Err(invalid(&format!(
                        "`env_pass` entry `{variable}` is not a variable name"
                    )));
                }
                McpServerTransport::Stdio {
                    command: command.clone(),
                    args: entry.args.clone().unwrap_or_default(),
                    env,
                    env_pass,
                    cwd: entry.cwd.as_ref().map(PathBuf::from),
                }
            }
            (None, Some(url)) => {
                if entry.args.is_some()
                    || entry.env.is_some()
                    || entry.env_pass.is_some()
                    || entry.cwd.is_some()
                {
                    return Err(invalid(
                        "`args`, `env`, `env_pass` and `cwd` belong to a `command` server",
                    ));
                }
                if !(url.starts_with("http://") || url.starts_with("https://")) {
                    return Err(invalid("`url` is not an http or https URL"));
                }
                if carries_credential(url) {
                    return Err(invalid(
                        "`url` looks like it carries a credential; use `bearer_token_env`",
                    ));
                }
                let headers = entry.headers.clone().unwrap_or_default();
                for (header, value) in &headers {
                    if header.is_empty()
                        || !header
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte))
                    {
                        return Err(invalid(&format!("`{header}` is not a header name")));
                    }
                    if reserved_header(header) {
                        return Err(invalid(&format!(
                            "header `{header}` is set by the MCP transport itself"
                        )));
                    }
                    if header.eq_ignore_ascii_case("authorization") || carries_credential(value) {
                        return Err(invalid(&format!(
                            "header `{header}` carries a credential; name the variable that \
                             holds the token in `bearer_token_env` instead"
                        )));
                    }
                }
                if let Some(variable) = &entry.bearer_token_env
                    && !variable_name(variable)
                {
                    return Err(invalid(&format!(
                        "`bearer_token_env` `{variable}` is not a variable name"
                    )));
                }
                McpServerTransport::Http {
                    url: url.clone(),
                    headers,
                    bearer_token_env: entry.bearer_token_env.clone(),
                }
            }
            _ => return Err(invalid("a server names exactly one of `command` and `url`")),
        };
        specs.push(McpServerSpec {
            name: name.clone(),
            transport,
        });
    }
    Ok(specs)
}

/// A server name by the rules of an entry's `name`: the host checks a server a session's
/// client names (ACP `mcpServers`, #695) by the same rule.
pub fn valid_server_name(name: &str) -> bool {
    server_name(name)
}

/// A header the streamable HTTP transport sets itself, which no server's `headers` override.
pub fn reserved_header(name: &str) -> bool {
    RESERVED_HEADERS
        .iter()
        .any(|reserved| name.eq_ignore_ascii_case(reserved))
}

fn server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_SERVER_NAME_LEN
        && !name.contains("__")
        && name
            .bytes()
            .next()
            .is_some_and(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"-_".contains(&byte))
}

fn variable_name(name: &str) -> bool {
    name.bytes()
        .next()
        .is_some_and(|byte| byte.is_ascii_alphabetic() || byte == b'_')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

fn carries_credential(text: &str) -> bool {
    p1_redact::redact(text).masked > 0
}

/// Only the `[[mcp_servers]]` table of an environment file; every other key is
/// [`load_environment`](crate::load_environment)'s to check.
#[derive(Debug, Deserialize)]
struct McpServersOnly {
    #[serde(default)]
    mcp_servers: Vec<McpServerToml>,
    alias_of: Option<String>,
}

/// The validated `[[mcp_servers]]` of environment `name`, found as
/// [`load_environment`](crate::load_environment) finds it (an alias uses the servers of the
/// environment it names). Servers belong to the session, not to one agent's assembly: the host
/// connects them once at session start, so they are read apart from the assembled
/// [`EnvironmentFile`](crate::EnvironmentFile).
pub fn load_mcp_servers(
    name: &str,
    search_dirs: &[PathBuf],
) -> Result<Vec<McpServerSpec>, AssemblyError> {
    load_with_reader(name, search_dirs, &ConfigReader::from_environment(), true)
}

fn load_with_reader(
    name: &str,
    search_dirs: &[PathBuf],
    reader: &ConfigReader,
    follow_alias: bool,
) -> Result<Vec<McpServerSpec>, AssemblyError> {
    let path = search_dirs
        .iter()
        .map(|base| base.join(name).join(ENVIRONMENT_FILE))
        .find(|path| path.is_file())
        .ok_or_else(|| AssemblyError::EnvironmentNotFound {
            name: name.to_string(),
            searched: search_dirs.to_vec(),
        })?;
    let invalid = |path: &Path, message: String| AssemblyError::InvalidEnvironmentFile {
        path: path.to_path_buf(),
        message,
    };
    let text = reader
        .read(&path)
        .map_err(|error| invalid(&path, error.to_string()))?;
    let parsed: McpServersOnly =
        toml::from_str(&text).map_err(|error| invalid(&path, error.to_string()))?;
    match parsed.alias_of {
        Some(target) if follow_alias => load_with_reader(&target, search_dirs, reader, false),
        Some(_) => Err(invalid(&path, "an alias names another alias".into())),
        None => validate(&parsed.mcp_servers).map_err(|message| invalid(&path, message)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<Vec<McpServerSpec>, String> {
        let parsed: McpServersOnly = toml::from_str(text).map_err(|error| error.to_string())?;
        validate(&parsed.mcp_servers)
    }

    #[test]
    fn a_stdio_and_an_http_server_parse() {
        let specs = parse(
            r#"
            [[mcp_servers]]
            name = "docs"
            command = "docs-mcp"
            args = ["--stdio"]
            env = { DOCS_MODE = "local" }
            env_pass = ["DOCS_TOKEN"]
            cwd = "tools"

            [[mcp_servers]]
            name = "tracker"
            url = "https://tracker.example/mcp"
            headers = { "X-Team" = "core" }
            bearer_token_env = "TRACKER_TOKEN"
            "#,
        )
        .unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(
            specs[0].transport,
            McpServerTransport::Stdio {
                command: "docs-mcp".into(),
                args: vec!["--stdio".into()],
                env: BTreeMap::from([("DOCS_MODE".into(), "local".into())]),
                env_pass: vec!["DOCS_TOKEN".into()],
                cwd: Some(PathBuf::from("tools")),
            }
        );
        assert!(matches!(
            &specs[1].transport,
            McpServerTransport::Http { bearer_token_env: Some(variable), .. } if variable == "TRACKER_TOKEN"
        ));
    }

    #[test]
    fn every_rule_is_enforced_and_named() {
        let cases = [
            (
                r#"name = "Docs"
                command = "x""#,
                "a server name",
            ),
            (
                r#"name = "a__b"
                command = "x""#,
                "a server name",
            ),
            (r#"name = "docs""#, "exactly one of"),
            (
                r#"name = "docs"
                command = "x"
                url = "https://x""#,
                "exactly one of",
            ),
            (
                r#"name = "docs"
                command = "x"
                headers = { A = "b" }"#,
                "belong to a `url` server",
            ),
            (
                r#"name = "docs"
                url = "https://x"
                env = { A = "b" }"#,
                "belong to a `command` server",
            ),
            (
                r#"name = "docs"
                url = "ftp://x""#,
                "not an http or https URL",
            ),
            (
                r#"name = "docs"
                url = "https://x"
                headers = { Authorization = "Bearer abc" }"#,
                "carries a credential",
            ),
            (
                r#"name = "docs"
                command = "x"
                env = { "1A" = "b" }"#,
                "not a variable name",
            ),
            (
                r#"name = "docs"
                command = "x"
                env_pass = ["A-B"]"#,
                "not a variable name",
            ),
            (
                r#"name = "docs"
                command = "x"
                surprise = 1"#,
                "unknown field",
            ),
        ];
        for (entry, expected) in cases {
            let error = parse(&format!("[[mcp_servers]]\n{entry}")).unwrap_err();
            assert!(error.contains(expected), "{entry}: {error}");
        }
        let twice = "[[mcp_servers]]\nname = \"a\"\ncommand = \"x\"\n\
                     [[mcp_servers]]\nname = \"a\"\ncommand = \"y\"\n";
        assert!(parse(twice).unwrap_err().contains("used by another server"));
        // A fake key of the `sk-` shape, built at run time: no credential-shaped literal.
        let key = format!("sk-{}", "Zx8cVb3nMq".repeat(3));
        let literal = format!(
            "[[mcp_servers]]\nname = \"a\"\ncommand = \"x\"\nenv = {{ KEY = \"{key}\" }}\n"
        );
        assert!(
            parse(&literal)
                .unwrap_err()
                .contains("looks like a credential")
        );
        let reserved = "[[mcp_servers]]\nname = \"a\"\nurl = \"https://x\"\n\
                        headers = { Mcp-Session-Id = \"s\" }\n";
        assert!(
            parse(reserved)
                .unwrap_err()
                .contains("set by the MCP transport itself")
        );
    }
}
