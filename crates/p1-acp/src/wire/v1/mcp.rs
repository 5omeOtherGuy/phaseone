//! The `mcpServers` of `session/new`, `session/load`, `session/resume` and `session/fork`
//! (#695): stdio servers (no `type`, or `"stdio"`) and streamable HTTP servers
//! (`"type": "http"`); the legacy `"sse"` transport is refused, as `mcpCapabilities`
//! advertises. An error names the entry and the field, never a value: an `env` or header
//! value may be a credential.

use p1_contracts::mcp::{ClientServer, Transport};
use serde_json::Value;
use std::collections::BTreeMap;

/// The servers `params` names; none when `mcpServers` is absent or null.
pub(crate) fn servers(params: &Value) -> Result<Vec<ClientServer>, String> {
    let entries = match params.get("mcpServers") {
        None | Some(Value::Null) => return Ok(Vec::new()),
        Some(Value::Array(entries)) => entries,
        Some(_) => return Err("mcpServers must be an array".into()),
    };
    entries
        .iter()
        .enumerate()
        .map(|(index, entry)| server(index, entry))
        .collect()
}

fn server(index: usize, entry: &Value) -> Result<ClientServer, String> {
    let at = format!("mcpServers[{index}]");
    if !entry.is_object() {
        return Err(format!("{at} must be an object"));
    }
    let name = text(entry, "name").ok_or_else(|| format!("{at} needs a string name"))?;
    // The name is the client's label, not a secret; it is shown so the client can find it.
    let at = format!("MCP server `{name}`");
    let kind = match entry.get("type") {
        None | Some(Value::Null) => None,
        Some(Value::String(kind)) => Some(kind.as_str()),
        Some(_) => return Err(format!("{at}: type must be a string")),
    };
    let transport = match kind {
        None | Some("stdio") => {
            let command = text(entry, "command")
                .filter(|command| !command.trim().is_empty())
                .ok_or_else(|| format!("{at} needs a command"))?;
            let args = match entry.get("args") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(args)) => args
                    .iter()
                    .map(|arg| arg.as_str().map(str::to_string))
                    .collect::<Option<_>>()
                    .ok_or_else(|| format!("{at}: every arg must be a string"))?,
                Some(_) => return Err(format!("{at}: args must be an array")),
            };
            Transport::Stdio {
                command: command.to_string(),
                args,
                env: pairs(entry, "env", &at)?,
                cwd: None,
            }
        }
        Some("http") => Transport::Http {
            url: text(entry, "url")
                .ok_or_else(|| format!("{at} needs a url"))?
                .to_string(),
            headers: pairs(entry, "headers", &at)?,
        },
        Some("sse") => {
            return Err(format!(
                "{at}: p1 does not serve the sse transport (mcpCapabilities.sse is false); use \
                 http or stdio"
            ));
        }
        Some(_) => return Err(format!("{at}: type must be \"http\" or absent (stdio)")),
    };
    Ok(ClientServer {
        name: name.to_string(),
        transport,
    })
}

fn text<'a>(entry: &'a Value, key: &str) -> Option<&'a str> {
    entry.get(key).and_then(Value::as_str)
}

/// `env` or `headers`: an array of `{name, value}`. A name given twice is refused, so the
/// server never gets a value the client did not mean last.
fn pairs(entry: &Value, key: &str, at: &str) -> Result<BTreeMap<String, String>, String> {
    let items = match entry.get(key) {
        None | Some(Value::Null) => return Ok(BTreeMap::new()),
        Some(Value::Array(items)) => items,
        Some(_) => return Err(format!("{at}: {key} must be an array")),
    };
    let mut pairs = BTreeMap::new();
    for item in items {
        let (Some(name), Some(value)) = (text(item, "name"), text(item, "value")) else {
            return Err(format!(
                "{at}: every {key} entry needs a string name and a string value"
            ));
        };
        if name.is_empty() {
            return Err(format!("{at}: a {key} entry has an empty name"));
        }
        if pairs.insert(name.to_string(), value.to_string()).is_some() {
            return Err(format!("{at}: {key} names `{name}` twice"));
        }
    }
    Ok(pairs)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn stdio_and_http_servers_decode() {
        let servers = servers(&json!({"mcpServers": [
            {"name": "files", "command": "files-mcp", "args": ["--stdio"],
             "env": [{"name": "FILES_TOKEN", "value": "value-one"}]},
            {"type": "stdio", "name": "plain", "command": "plain-mcp"},
            {"type": "http", "name": "web", "url": "http://127.0.0.1:1/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer value-two"}]},
        ]}))
        .unwrap();
        assert_eq!(
            servers,
            [
                ClientServer {
                    name: "files".into(),
                    transport: Transport::Stdio {
                        command: "files-mcp".into(),
                        args: vec!["--stdio".into()],
                        env: BTreeMap::from([("FILES_TOKEN".into(), "value-one".into())]),
                        cwd: None,
                    },
                },
                ClientServer {
                    name: "plain".into(),
                    transport: Transport::Stdio {
                        command: "plain-mcp".into(),
                        args: Vec::new(),
                        env: BTreeMap::new(),
                        cwd: None,
                    },
                },
                ClientServer {
                    name: "web".into(),
                    transport: Transport::Http {
                        url: "http://127.0.0.1:1/mcp".into(),
                        headers: BTreeMap::from([(
                            "Authorization".into(),
                            "Bearer value-two".into()
                        )]),
                    },
                },
            ]
        );
    }

    #[test]
    fn no_list_is_no_server() {
        assert!(servers(&json!({})).unwrap().is_empty());
        assert!(servers(&json!({"mcpServers": null})).unwrap().is_empty());
        assert!(servers(&json!({"mcpServers": []})).unwrap().is_empty());
    }

    #[test]
    fn a_malformed_entry_is_refused_naming_the_server_but_no_value() {
        for (entry, expected) in [
            (
                json!({"name": "x", "type": "sse", "url": "http://h", "headers": []}),
                "sse",
            ),
            (json!({"name": "x", "type": "ws"}), "type must be"),
            (json!({"name": "x"}), "needs a command"),
            (json!({"name": "x", "command": " "}), "needs a command"),
            (json!({"name": "x", "type": "http"}), "needs a url"),
            (json!({"command": "c"}), "mcpServers[0] needs a string name"),
            (
                json!({"name": "x", "command": "c", "args": [1]}),
                "every arg",
            ),
            (
                json!({"name": "x", "command": "c",
                       "env": [{"name": "A", "value": "secret-one"}, {"name": "A", "value": "secret-two"}]}),
                "names `A` twice",
            ),
            (
                json!({"name": "x", "type": "http", "url": "http://h",
                       "headers": [{"name": "Authorization"}]}),
                "string name and a string value",
            ),
        ] {
            let error = servers(&json!({"mcpServers": [entry]})).unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert!(!error.contains("secret-"), "{error}");
        }
        assert!(
            servers(&json!({"mcpServers": {}}))
                .unwrap_err()
                .contains("array")
        );
    }
}
