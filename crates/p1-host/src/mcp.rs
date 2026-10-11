//! The session's MCP servers (ADR-0181): the environment's `[[mcp_servers]]` and the servers
//! the session's client names (ACP `mcpServers`, #695, ADR-0182), connected once at session
//! start, and their tool generation added to the main agent's assembly.
//!
//! The host decides everything a server is given. A stdio child gets the shell's allow-listed
//! variables (no credential among them), the entry's literal `env` and the variables its
//! `env_pass` names; an HTTP server gets the entry's headers and, from `bearer_token_env`, one
//! bearer token. A client's stdio server gets the same allow-list and the variables the client
//! lists, in the session's folder; a client's HTTP server gets the client's headers. Every value
//! passed by name is registered in the session's secrets, so it is masked wherever it would
//! show: p1's own shell can see p1's environment. Every value a client sends for a server goes
//! only to that server, so it masks only that server's tools (ADR-0184): their results, long
//! results and declarations, and a start failure. The names follow an environment's rules,
//! and a name two servers share fails the start. The generated tools are wrapped like every
//! assembled tool (masking), their long results go to the session's output store, and they
//! join the assembly before the agent is built, so the core's first `Environment` record
//! journals the generation and the authorization policy sees each call as it sees any other.
//!
//! A server that fails to connect or to list its tools, or serves a tool whose name no route
//! can carry or whose declaration carries a credential, fails the start when it is required
//! (an environment server, unless its entry says `required = false`); an optional one (that,
//! or any server the client names) is skipped (ADR-0186, ADR-0190). A skip is never silent:
//! the host shows a notice naming the server and its masked reason and records both on the
//! session's assembly line. A name or a tool-name clash fails the start whatever the server.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use p1_assembly::{McpServerSpec, McpServerTransport, ResolvedTool};
use p1_contracts::history::ToolCall;
use p1_contracts::mcp::{ClientServer, McpClient, McpServer, Transport};
use p1_contracts::{AgentEvent, CancellationToken, EventSink, Tool};
use p1_journal::SkippedMcpServer;
use p1_mcp_client::{NativeMcpClient, ResultStore, ServerConnection, ToolGenerations};
use p1_module_runtime::OutputStore;
use p1_module_runtime::outputs::Capture;
use p1_module_runtime::process::{ENV_ALLOW, ENV_ALLOW_PREFIXES};
use p1_redact::{MaskCounter, RedactingTool, SecretSet};

/// How long one connection attempt (start, handshake) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long listing one server's tools may take.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Attempts per server: one retry (a longer reconnect policy is a later slice).
const CONNECT_ATTEMPTS: usize = 2;

/// The session's connected MCP servers: their tool generations, the values each server's
/// tools mask, the optional servers the start skipped, and the client's servers as it named
/// them, which a re-attach compares its own with (#800).
pub(crate) struct McpServers {
    generations: ToolGenerations,
    scopes: Arc<Scopes>,
    skipped: Vec<SkippedMcpServer>,
    client: Vec<ClientServer>,
}

/// The values a server's tools mask (ADR-0184): an environment server's tools mask the
/// session's set; a client server's tools mask it plus every value the client sent for that
/// server.
struct Scopes {
    session: SecretSet,
    clients: BTreeMap<String, SecretSet>,
}

impl Scopes {
    fn of(&self, server: &str) -> &SecretSet {
        self.clients.get(server).unwrap_or(&self.session)
    }
}

/// Connect the servers environment `environment` names, then the ones the session's client
/// names (`client`), and add generation 1 to the main agent's assembly (`assembled`). `None`
/// when there is none.
pub(crate) async fn attach(
    deps: &crate::HostDeps,
    environment: &str,
    client: &[ClientServer],
    workspace: &Path,
    assembled: &mut p1_assembly::Assembled,
    mask: &Arc<MaskCounter>,
    cancel: &CancellationToken,
) -> Result<Option<Arc<McpServers>>, String> {
    let reserved = assembled
        .tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect();
    let Some(servers) =
        connect_all(deps, environment, client, workspace, reserved, mask, cancel).await?
    else {
        return Ok(None);
    };
    servers.extend(&mut assembled.tools, &mut assembled.resolved.tools, mask)?;
    Ok(Some(Arc::new(servers)))
}

/// Connect the servers environment `environment` names, then the ones the session's client
/// names (`client`), into a new generation, as [`start`] does with the host's own client, the
/// shell's environment and the session's output store. `None` when there is none. A session
/// start and a re-attach that names other client servers (#800) connect through it.
pub(crate) async fn connect_all(
    deps: &crate::HostDeps,
    environment: &str,
    client: &[ClientServer],
    workspace: &Path,
    reserved: BTreeSet<String>,
    mask: &Arc<MaskCounter>,
    cancel: &CancellationToken,
) -> Result<Option<McpServers>, String> {
    let specs = p1_assembly::load_mcp_servers(environment, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    if specs.is_empty() && client.is_empty() {
        return Ok(None);
    }
    let snapshot = deps
        .shell_env
        .clone()
        .unwrap_or_else(|| std::env::vars_os().collect());
    start(
        &NativeMcpClient::new(),
        &specs,
        client,
        workspace,
        &snapshot,
        mask,
        reserved,
        deps.tool_outputs.clone(),
        cancel,
    )
    .await
}

/// Connect every server `specs` and `named` (the client's) name and discover generation 1.
/// `None` when there is none. Every name and transport is checked before the first server
/// starts. A required server that fails to connect or to list its tools, or serves a tool no
/// route can carry or whose declaration carries a credential, fails the start; an optional one
/// is skipped, its reason masked with the values its tools would mask.
/// `reserved` holds the names the main agent's assembly already uses; a clash with one fails
/// the start, as a duplicate tool name does. A value passed by name is registered in `mask`'s
/// set, the session's; a long result's masking is counted in `mask`.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start(
    client: &dyn McpClient,
    specs: &[McpServerSpec],
    named: &[ClientServer],
    workspace: &Path,
    snapshot: &[(OsString, OsString)],
    mask: &Arc<MaskCounter>,
    reserved: BTreeSet<String>,
    outputs: Arc<OutputStore>,
    cancel: &CancellationToken,
) -> Result<Option<McpServers>, String> {
    if specs.is_empty() && named.is_empty() {
        return Ok(None);
    }
    let secrets = mask.secrets();
    let mut clients = BTreeMap::new();
    let mut transports = Vec::with_capacity(specs.len() + named.len());
    for spec in specs {
        transports.push((
            spec.name.clone(),
            transport(spec, workspace, snapshot, secrets)?,
            spec.required,
        ));
    }
    for server in named {
        let name = &server.name;
        if !p1_assembly::valid_server_name(name) {
            return Err(format!(
                "MCP server `{name}` (from the client): a server name is 1 to {} lowercase \
                 letters, digits, `-` and `_`, starts with a letter or digit and has no `__`",
                p1_assembly::MAX_SERVER_NAME_LEN
            ));
        }
        if let Some(index) = transports.iter().position(|(taken, ..)| taken == name) {
            let owner = if index < specs.len() {
                "the environment"
            } else {
                "the client"
            };
            return Err(format!(
                "MCP server `{name}` (from the client): {owner} already names a server `{name}`"
            ));
        }
        let transport = client_transport(server, workspace, snapshot)?;
        clients.insert(
            name.clone(),
            client_values(secrets, std::slice::from_ref(server)),
        );
        // ADR-0186: the client cannot mark a server required, and one broken entry in an
        // editor's configuration must not block every session.
        transports.push((name.clone(), transport, false));
    }
    let scopes = Arc::new(Scopes {
        session: secrets.clone(),
        clients,
    });
    let store: Arc<dyn ResultStore> = Arc::new(StoredResults {
        store: outputs,
        scopes: scopes.clone(),
        counter: mask.clone(),
    });
    let mut listed = Vec::with_capacity(transports.len());
    let mut skipped = Vec::new();
    for (name, transport, required) in transports {
        let failure = match connect(client, &name, transport, cancel).await {
            Ok(server) => {
                let server: Arc<dyn McpServer> = Arc::from(server);
                match list_tools(server.as_ref(), cancel)
                    .await
                    .and_then(|tools| servable(&name, &tools, scopes.of(&name)).map(|()| tools))
                {
                    Ok(tools) => {
                        listed.push((ServerConnection { name, server }, tools));
                        continue;
                    }
                    Err(failure) => failure,
                }
            }
            Err(failure) => failure,
        };
        // A cancelled start is not a failed server: it stops whatever the server.
        if required || cancel.is_cancelled() {
            return Err(format!("MCP server `{name}`: {failure}"));
        }
        skipped.push(SkippedMcpServer {
            reason: p1_redact::redact_with(&failure, scopes.of(&name)).text,
            name,
        });
    }
    let generations = ToolGenerations::with_tools(listed, reserved, Some(store))
        .map_err(|error| error.to_string())?;
    Ok(Some(McpServers {
        generations,
        scopes,
        skipped,
        client: named.to_vec(),
    }))
}

/// List one connected server's tools, bounded by [`DISCOVERY_TIMEOUT`] and the run's
/// cancellation. The error says why, without the server's name.
async fn list_tools(
    server: &dyn McpServer,
    cancel: &CancellationToken,
) -> Result<Vec<p1_contracts::mcp::RemoteTool>, String> {
    let listing = tokio::time::timeout(DISCOVERY_TIMEOUT, server.tools());
    tokio::select! {
        biased;
        () = cancel.cancelled() => Err("cancelled while listing its tools".to_owned()),
        listed = listing => match listed {
            Ok(Ok(tools)) => Ok(tools),
            Ok(Err(error)) => Err(format!("listing its tools failed: {error}")),
            Err(_) => Err(format!(
                "listing its tools took longer than {} s",
                DISCOVERY_TIMEOUT.as_secs()
            )),
        },
    }
}

/// Whether every tool server `server` listed can be served: its name is one every route
/// carries and its declaration carries no credential and none of `secrets` (ADR-0190). The
/// error names the tool, not the server, and never a value: a required server's error is not
/// masked later, and a key glued into a tool name escapes masking, so a name that carries one
/// is not shown.
fn servable(
    server: &str,
    tools: &[p1_contracts::mcp::RemoteTool],
    secrets: &SecretSet,
) -> Result<(), String> {
    for remote in tools {
        let generated = p1_mcp_client::tool_name(server, &remote.name);
        let tool = if plain_name(&generated, secrets) {
            format!("its tool `{}` (`{generated}`)", remote.name)
        } else {
            "its tool whose name carries a credential value".to_owned()
        };
        let refused = match p1_mcp_client::declare(server, remote) {
            Err(_) => format!(
                "{tool} has a name no route can carry (at most {} letters, digits, `_` and `-`)",
                p1_mcp_client::generation::MAX_NAME_LEN
            ),
            Ok((declaration, identity)) => {
                match p1_redact::check_declaration(&declaration, &identity, secrets) {
                    Ok(()) => continue,
                    Err(message) => format!("{tool}: {message}"),
                }
            }
        };
        return Err(p1_redact::redact_with(&refused, secrets).text);
    }
    Ok(())
}

/// Whether a tool name carries no credential, by the rule a declaration is checked with.
fn plain_name(name: &str, secrets: &SecretSet) -> bool {
    let probe = p1_contracts::ToolDeclaration {
        name: name.to_owned(),
        description: String::new(),
        kind: p1_contracts::DeclarationKind::Function {
            input_schema: serde_json::Value::Null,
        },
    };
    let identity = p1_contracts::ToolIdentity {
        implementation: String::new(),
        variant: String::new(),
    };
    p1_redact::check_declaration(&probe, &identity, secrets).is_ok()
}

impl McpServers {
    /// The optional servers the start skipped, for the session's assembly line.
    pub(crate) fn skipped(&self) -> Vec<SkippedMcpServer> {
        self.skipped.clone()
    }

    /// The servers the session's client named for this generation.
    pub(crate) fn client(&self) -> &[ClientServer] {
        &self.client
    }

    /// One notice per skipped server: its name and masked reason.
    pub(crate) fn notices(&self) -> Vec<String> {
        self.skipped
            .iter()
            .map(|server| {
                format!(
                    "MCP server `{}` skipped (optional), the session runs without its tools: {}",
                    server.name, server.reason
                )
            })
            .collect()
    }

    /// Add the current generation's tools to an assembly: masked like every assembled tool,
    /// each with its server's values, refused when a declaration carries one of them or a
    /// credential, or when its name is already taken (a switched assembly can hold a name the
    /// start's did not), each with a resolved entry naming `mcp:<server>`.
    pub(crate) fn extend(
        &self,
        tools: &mut Vec<Arc<dyn Tool>>,
        resolved: &mut Vec<ResolvedTool>,
        mask: &Arc<MaskCounter>,
    ) -> Result<(), String> {
        for tool in self.generations.current().mcp_tools() {
            let name = &tool.declaration().name;
            if tools.iter().any(|taken| &taken.declaration().name == name) {
                return Err(format!(
                    "the MCP tool name `{name}` clashes with another tool of the same agent"
                ));
            }
            let secrets = self.scopes.of(tool.server());
            p1_redact::check_declaration(tool.declaration(), tool.identity(), secrets)
                .map_err(|message| format!("MCP tool `{name}`: {message}"))?;
            resolved.push(ResolvedTool {
                module: tool.identity().implementation.clone(),
                declaration: tool.declaration().clone(),
                identity: tool.identity().clone(),
            });
            tools.push(Arc::new(RedactingTool::with_secrets(
                tool.clone(),
                mask.clone(),
                secrets.clone(),
            )));
        }
        Ok(())
    }
}

/// The skip notices that wait for the next turn to start ([`skip_notices`]). A swap (#800)
/// replaces them with its own generation's.
pub(crate) type PendingNotices = Arc<Mutex<Vec<String>>>;

/// `events`, showing one notice per server the start skipped (`servers`) right after the next
/// turn starts, on the display-only notice path the masking notice uses (ADR-0184): a front end
/// may show nothing before its session is open (an ACP session process drops an operator
/// notice until it has answered `session/new`, and nothing may follow a `session/load` answer).
/// The notices still waiting, for a swap to replace.
pub(crate) fn skip_notices(
    events: Arc<dyn EventSink>,
    servers: Option<&McpServers>,
) -> (Arc<dyn EventSink>, PendingNotices) {
    let pending = Arc::new(Mutex::new(
        servers.map(McpServers::notices).unwrap_or_default(),
    ));
    let events = Arc::new(SkipNotices {
        inner: events,
        pending: pending.clone(),
    });
    (events, pending)
}

struct SkipNotices {
    inner: Arc<dyn EventSink>,
    pending: PendingNotices,
}

impl EventSink for SkipNotices {
    fn emit(&self, event: AgentEvent) {
        let turn_started = matches!(event, AgentEvent::TurnStarted);
        self.inner.emit(event);
        if turn_started {
            let pending = std::mem::take(
                &mut *self
                    .pending
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
            );
            for text in pending {
                self.inner.emit(AgentEvent::ProviderNotice { text });
            }
        }
    }
}

/// The server's transport, as the host decides it.
pub(crate) fn transport(
    spec: &McpServerSpec,
    workspace: &Path,
    snapshot: &[(OsString, OsString)],
    secrets: &SecretSet,
) -> Result<Transport, String> {
    let lookup = |variable: &str| -> Result<String, String> {
        let value = snapshot
            .iter()
            .find(|(name, _)| name == variable)
            .and_then(|(_, value)| value.to_str())
            .ok_or_else(|| {
                format!(
                    "MCP server `{}`: the variable `{variable}` it names is not set, or not UTF-8",
                    spec.name
                )
            })?;
        secrets.register(value);
        Ok(value.to_owned())
    };
    match &spec.transport {
        McpServerTransport::Stdio {
            command,
            args,
            env,
            env_pass,
            cwd,
        } => {
            let mut child = allowed_environment(snapshot);
            for variable in env_pass {
                child.insert(variable.clone(), lookup(variable)?);
            }
            child.extend(env.clone());
            Ok(Transport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: child,
                cwd: Some(match cwd {
                    Some(cwd) => workspace.join(cwd),
                    None => workspace.to_path_buf(),
                }),
            })
        }
        McpServerTransport::Http {
            url,
            headers,
            bearer_token_env,
        } => {
            let mut headers = headers.clone();
            if let Some(variable) = bearer_token_env {
                headers.insert(
                    "Authorization".into(),
                    format!("Bearer {}", lookup(variable)?),
                );
            }
            Ok(Transport::Http {
                url: url.clone(),
                headers,
            })
        }
    }
}

/// A server the session's client names, as the host completes it: a stdio child gets the
/// allow-list and the client's variables, in the session's folder (ACP names no folder for a
/// server); an HTTP server gets the client's headers, none of the ones the transport sets. The
/// values the client sent are masked by [`client_values`].
pub(crate) fn client_transport(
    server: &ClientServer,
    workspace: &Path,
    snapshot: &[(OsString, OsString)],
) -> Result<Transport, String> {
    let name = &server.name;
    match &server.transport {
        Transport::Stdio {
            command, args, env, ..
        } => {
            let mut child = allowed_environment(snapshot);
            child.extend(env.clone());
            Ok(Transport::Stdio {
                command: command.clone(),
                args: args.clone(),
                env: child,
                cwd: Some(workspace.to_path_buf()),
            })
        }
        Transport::Http { url, headers } => {
            if !(url.starts_with("http://") || url.starts_with("https://")) {
                return Err(format!(
                    "MCP server `{name}` (from the client): the url is not an http or https URL"
                ));
            }
            for (header, value) in headers {
                let token = !header.is_empty()
                    && header
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte));
                // What an HTTP header value may hold; refused here, before any server starts.
                let printable = value
                    .bytes()
                    .all(|byte| byte == b'\t' || (0x20..0x7f).contains(&byte));
                if !token || !printable {
                    return Err(format!(
                        "MCP server `{name}` (from the client): header `{}` is not a valid \
                         header name and value",
                        header.escape_debug()
                    ));
                }
                if p1_assembly::reserved_header(header) {
                    return Err(format!(
                        "MCP server `{name}` (from the client): header `{header}` is set by the \
                         MCP transport itself"
                    ));
                }
            }
            Ok(Transport::Http {
                url: url.clone(),
                headers: headers.clone(),
            })
        }
    }
}

/// The session's set plus every value the client sent for `servers`, any of which may be a
/// credential: each stdio variable value and each header value, and the credential of a
/// `<scheme> <credential>` header value (`Bearer …`) on its own, as a server may answer with
/// the bare token. For one server, what its tools mask (ADR-0184); for all of them, what a
/// session start failure is masked with, as a server's error may echo any of them. The
/// session's set never holds them: p1's own tools cannot see them.
pub(crate) fn client_values(session: &SecretSet, servers: &[ClientServer]) -> SecretSet {
    let values = session.scoped();
    for server in servers {
        let sent = match &server.transport {
            Transport::Stdio { env, .. } => env,
            Transport::Http { headers, .. } => headers,
        };
        for value in sent.values() {
            values.register(value);
            if let Some((_, credential)) = value.trim().rsplit_once(char::is_whitespace) {
                values.register(credential);
            }
        }
    }
    values
}

/// The variables every shell command keeps (`ENV_ALLOW`): what a server needs to run and no
/// credential.
fn allowed_environment(snapshot: &[(OsString, OsString)]) -> BTreeMap<String, String> {
    snapshot
        .iter()
        .filter_map(|(name, value)| Some((name.to_str()?, value.to_str()?)))
        .filter(|(name, _)| {
            ENV_ALLOW.contains(name)
                || ENV_ALLOW_PREFIXES
                    .iter()
                    .any(|prefix| name.starts_with(prefix))
        })
        .map(|(name, value)| (name.to_owned(), value.to_owned()))
        .collect()
}

/// Connect one server, with one retry. The error says why, without the server's name.
async fn connect(
    client: &dyn McpClient,
    name: &str,
    transport: Transport,
    cancel: &CancellationToken,
) -> Result<Box<dyn McpServer>, String> {
    let mut failure = String::new();
    for _ in 0..CONNECT_ATTEMPTS {
        if cancel.is_cancelled() {
            return Err("cancelled while connecting".to_owned());
        }
        let attempt = client.connect(name, transport.clone(), cancel.clone());
        match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
            Ok(Ok(server)) => return Ok(server),
            Ok(Err(p1_contracts::mcp::McpError::Cancelled)) => {
                return Err("cancelled while connecting".to_owned());
            }
            Ok(Err(error)) => failure = error.to_string(),
            Err(_) => {
                failure = format!("no answer within {} s", CONNECT_TIMEOUT.as_secs());
            }
        }
    }
    Err(failure)
}

/// The output store behind a generated tool's long results (ADR-0109), masked with the values
/// of the server that produced them and counted in the agent's `counter`.
struct StoredResults {
    store: Arc<OutputStore>,
    scopes: Arc<Scopes>,
    counter: Arc<MaskCounter>,
}

impl ResultStore for StoredResults {
    fn mask(&self, server: &str, call: &ToolCall, text: String) -> String {
        let redaction = p1_redact::redact_with(&text, self.scopes.of(server));
        self.counter
            .add_call(&call.name, &call.call_id, redaction.masked);
        redaction.text
    }

    fn store(&self, server: &str, text: &str) -> String {
        let info = self.store.store_text(self.scopes.of(server).clone(), text);
        let state = match info.capture {
            Capture::Complete => "complete",
            Capture::StoredCapReached => "stopped at the store's cap, later output not stored",
            Capture::StorageIncomplete => "incomplete, later output was not stored",
            Capture::StorageFailed => {
                return "[full output not stored; recovery unavailable]".into();
            }
        };
        format!(
            "[stored output: handle_id {}, {} bytes, {state}; page it with read_output]",
            info.handle, info.stored_bytes
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::history::{ToolCall, ToolStatus};
    use p1_contracts::mcp::{ContentBlock, McpError, RemoteTool, ResourcePage};
    use p1_contracts::{BoxFuture, Effect, ToolContext, ToolInput};
    use std::sync::Mutex;

    /// A fake key of the `sk-` shape, built at run time: no credential-shaped literal.
    fn fake_key() -> String {
        format!("sk-{}", "Mn4bVc8xZl".repeat(3))
    }

    fn snapshot(pairs: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
        pairs
            .iter()
            .map(|(name, value)| (OsString::from(name), OsString::from(value)))
            .collect()
    }

    fn stdio_spec(env_pass: &[&str]) -> McpServerSpec {
        McpServerSpec {
            name: "docs".into(),
            required: true,
            transport: McpServerTransport::Stdio {
                command: "docs-mcp".into(),
                args: vec!["--stdio".into()],
                env: BTreeMap::from([("DOCS_MODE".into(), "local".into())]),
                env_pass: env_pass.iter().map(|name| name.to_string()).collect(),
                cwd: Some("tools".into()),
            },
        }
    }

    #[test]
    fn a_stdio_child_gets_the_allow_list_its_env_and_what_env_pass_names_only() {
        let key = fake_key();
        let secrets = SecretSet::new();
        let env = snapshot(&[
            ("PATH", "/usr/bin"),
            ("HOME", "/home/someone"),
            ("LC_ALL", "C"),
            ("ANTHROPIC_API_KEY", &key),
            ("SSH_AUTH_SOCK", "/run/agent"),
            ("DOCS_TOKEN", &key),
        ]);
        let Transport::Stdio {
            env: child, cwd, ..
        } = transport(&stdio_spec(&[]), Path::new("/ws"), &env, &secrets).unwrap()
        else {
            panic!("not stdio");
        };
        assert_eq!(
            child.keys().map(String::as_str).collect::<Vec<_>>(),
            ["DOCS_MODE", "HOME", "LC_ALL", "PATH"]
        );
        assert_eq!(cwd.as_deref(), Some(Path::new("/ws/tools")));
        assert!(
            secrets.is_empty(),
            "nothing passed by name, nothing registered"
        );

        let Transport::Stdio { env: child, .. } = transport(
            &stdio_spec(&["DOCS_TOKEN"]),
            Path::new("/ws"),
            &env,
            &secrets,
        )
        .unwrap() else {
            panic!("not stdio");
        };
        assert_eq!(child.get("DOCS_TOKEN"), Some(&key));
        assert!(!child.contains_key("ANTHROPIC_API_KEY"));
        assert!(
            secrets.contains_secret(&key),
            "a passed value is masked from now on"
        );
    }

    #[test]
    fn a_missing_variable_is_an_error_naming_it_not_its_value() {
        let error = transport(
            &stdio_spec(&["DOCS_TOKEN"]),
            Path::new("/ws"),
            &snapshot(&[]),
            &SecretSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("`DOCS_TOKEN`"), "{error}");
    }

    #[test]
    fn a_bearer_token_becomes_the_authorization_header_and_a_secret() {
        let key = fake_key();
        let secrets = SecretSet::new();
        let spec = McpServerSpec {
            name: "tracker".into(),
            required: true,
            transport: McpServerTransport::Http {
                url: "https://tracker.example/mcp".into(),
                headers: BTreeMap::from([("X-Team".into(), "core".into())]),
                bearer_token_env: Some("TRACKER_TOKEN".into()),
            },
        };
        let Transport::Http { headers, .. } = transport(
            &spec,
            Path::new("/ws"),
            &snapshot(&[("TRACKER_TOKEN", &key)]),
            &secrets,
        )
        .unwrap() else {
            panic!("not http");
        };
        assert_eq!(headers.get("Authorization"), Some(&format!("Bearer {key}")));
        assert_eq!(headers.get("X-Team").map(String::as_str), Some("core"));
        assert!(secrets.contains_secret(&key));
    }

    /// An in-process server: two tools, one read-only; `leak` answers with `secret`.
    struct FakeServer {
        secret: String,
        calls: Mutex<Vec<String>>,
    }

    impl McpServer for FakeServer {
        fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
            Box::pin(async {
                Ok(vec![
                    RemoteTool {
                        name: "lookup".into(),
                        description: "Look up.".into(),
                        input_schema: serde_json::json!({"type": "object"}),
                        read_only: true,
                    },
                    RemoteTool {
                        name: "leak".into(),
                        description: "Leaks.".into(),
                        input_schema: serde_json::json!({"type": "object"}),
                        read_only: false,
                    },
                ])
            })
        }

        fn call(
            &self,
            raw_name: &str,
            _args: serde_json::Value,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            self.calls.lock().unwrap().push(raw_name.to_owned());
            let text = format!("value {}", self.secret);
            Box::pin(async move { Ok(vec![ContentBlock::Text { text }]) })
        }

        fn resources(&self, _: Option<String>) -> BoxFuture<'_, Result<ResourcePage, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }

        fn read_resource(&self, _: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
    }

    /// Connects every name to one shared fake server; fails the first `failures` attempts.
    /// A name in `refused` never connects, and its error echoes every value the server was
    /// given; a name in `unlisted` connects but fails to list its tools; a name in `listings`
    /// connects and lists the tools given there.
    struct FakeClient {
        server: Arc<FakeServer>,
        failures: Mutex<usize>,
        attempts: Mutex<usize>,
        refused: Vec<&'static str>,
        unlisted: Vec<&'static str>,
        listings: Vec<(&'static str, Vec<RemoteTool>)>,
    }

    /// A server that lists the tools it was given and serves none of them.
    struct Listing(Vec<RemoteTool>);

    impl McpServer for Listing {
        fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
            let tools = self.0.clone();
            Box::pin(async move { Ok(tools) })
        }
        fn call(
            &self,
            _: &str,
            _: serde_json::Value,
            _: CancellationToken,
        ) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
        fn resources(&self, _: Option<String>) -> BoxFuture<'_, Result<ResourcePage, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
        fn read_resource(&self, _: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
    }

    /// A server whose tool listing fails.
    struct Unlisted;

    impl McpServer for Unlisted {
        fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
            Box::pin(async { Err(McpError::Protocol("tools/list: method not found".into())) })
        }
        fn call(
            &self,
            _: &str,
            _: serde_json::Value,
            _: CancellationToken,
        ) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
        fn resources(&self, _: Option<String>) -> BoxFuture<'_, Result<ResourcePage, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
        fn read_resource(&self, _: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            Box::pin(async { Err(McpError::Protocol("none".into())) })
        }
    }

    struct Shared(Arc<FakeServer>);

    impl McpServer for Shared {
        fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
            self.0.tools()
        }
        fn call(
            &self,
            raw_name: &str,
            args: serde_json::Value,
            cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            self.0.call(raw_name, args, cancel)
        }
        fn resources(
            &self,
            cursor: Option<String>,
        ) -> BoxFuture<'_, Result<ResourcePage, McpError>> {
            self.0.resources(cursor)
        }
        fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
            self.0.read_resource(uri)
        }
    }

    impl McpClient for FakeClient {
        fn connect(
            &self,
            name: &str,
            transport: Transport,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<Box<dyn McpServer>, McpError>> {
            *self.attempts.lock().unwrap() += 1;
            let mut failures = self.failures.lock().unwrap();
            let result: Result<Box<dyn McpServer>, McpError> = if self.refused.contains(&name) {
                let given: Vec<String> = match transport {
                    Transport::Stdio { env, .. } => env.into_values().collect(),
                    Transport::Http { headers, .. } => headers.into_values().collect(),
                };
                Err(McpError::Connect(format!("refused {}", given.join(" "))))
            } else if self.unlisted.contains(&name) {
                Ok(Box::new(Unlisted))
            } else if let Some((_, tools)) = self.listings.iter().find(|(n, _)| *n == name) {
                Ok(Box::new(Listing(tools.clone())))
            } else if *failures > 0 {
                *failures -= 1;
                Err(McpError::Connect("not yet".into()))
            } else {
                Ok(Box::new(Shared(self.server.clone())))
            };
            Box::pin(async move { result })
        }
    }

    fn fake_client(secret: &str, failures: usize) -> FakeClient {
        FakeClient {
            server: Arc::new(FakeServer {
                secret: secret.to_owned(),
                calls: Mutex::new(Vec::new()),
            }),
            failures: Mutex::new(failures),
            attempts: Mutex::new(0),
            refused: Vec::new(),
            unlisted: Vec::new(),
            listings: Vec::new(),
        }
    }

    fn outputs() -> Arc<OutputStore> {
        Arc::new(OutputStore::temporary(
            p1_module_runtime::OutputCaps::DEFAULT,
        ))
    }

    #[tokio::test]
    async fn the_generation_joins_the_assembly_masked_with_resolved_entries() {
        let key = fake_key();
        let client = fake_client(&key, 1);
        let secrets = SecretSet::new();
        secrets.register(&key);
        let mask = Arc::new(MaskCounter::with_secrets(secrets.clone()));
        let generations = start(
            &client,
            &[stdio_spec(&[])],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &mask,
            BTreeSet::from(["read".to_owned()]),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("one server");
        assert_eq!(
            *client.attempts.lock().unwrap(),
            2,
            "one retry after a failure"
        );

        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut resolved = Vec::new();
        generations
            .extend(&mut tools, &mut resolved, &mask)
            .unwrap();
        assert_eq!(resolved.len(), 2);
        assert_eq!(resolved[0].module, "mcp:docs");
        assert_eq!(resolved[0].declaration.name, "mcp__docs__lookup");
        assert_eq!(resolved[1].identity.variant, "leak");

        let call = |name: &str| ToolCall {
            call_id: "c".into(),
            name: name.into(),
            input: ToolInput::Json("{}".into()),
        };
        assert_eq!(
            tools[0].effect(&call("mcp__docs__lookup")),
            Effect::ReadOnly
        );
        assert_eq!(tools[1].effect(&call("mcp__docs__leak")), Effect::Executes);
        let outcome = tools[1]
            .execute(
                &call("mcp__docs__leak"),
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(outcome.status, ToolStatus::Ok);
        assert!(!outcome.content.contains(&key), "the result is masked");
        assert_eq!(*client.server.calls.lock().unwrap(), ["leak"]);
    }

    #[tokio::test]
    async fn extending_an_assembly_that_already_holds_the_name_fails() {
        let client = fake_client("unused-value", 0);
        let generations = start(
            &client,
            &[stdio_spec(&[])],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("one server");
        let mask = Arc::new(MaskCounter::new());
        // A switched assembly whose own tools already hold one generated name.
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut resolved = Vec::new();
        generations
            .extend(&mut tools, &mut resolved, &mask)
            .unwrap();
        let mut switched = vec![tools[0].clone()];
        let error = generations
            .extend(&mut switched, &mut Vec::new(), &mask)
            .unwrap_err();
        assert!(error.contains("mcp__docs__lookup"), "{error}");
    }

    #[tokio::test]
    async fn a_cancelled_start_stops_connecting() {
        // A cancelled start stops whether the server is required or not (a client server
        // is optional).
        for (specs, named) in [
            (vec![stdio_spec(&[])], vec![]),
            (vec![], vec![client_stdio("files", &[])]),
        ] {
            a_cancelled_start_stops_connecting_to(specs, named).await;
        }
    }

    async fn a_cancelled_start_stops_connecting_to(
        specs: Vec<McpServerSpec>,
        named: Vec<ClientServer>,
    ) {
        let client = fake_client("unused-value", 0);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = start(
            &client,
            &specs,
            &named,
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::new(),
            outputs(),
            &cancel,
        )
        .await;
        assert_eq!(
            *client.attempts.lock().unwrap(),
            0,
            "no attempt after a cancellation"
        );
        assert!(
            result
                .err()
                .is_some_and(|error| error.contains("cancelled"))
        );
    }

    #[tokio::test]
    async fn a_server_that_never_connects_fails_the_start_after_one_retry() {
        let client = fake_client("unused-value", 5);
        let error = start(
            &client,
            &[stdio_spec(&[])],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("the start fails");
        assert!(error.contains("MCP server `docs`"), "{error}");
        assert_eq!(*client.attempts.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn a_clash_with_the_assembly_fails_the_start() {
        let client = fake_client("unused-value", 0);
        let error = start(
            &client,
            &[stdio_spec(&[])],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::from(["mcp__docs__lookup".to_owned()]),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("the start fails");
        assert!(error.contains("clashes"), "{error}");
    }

    #[tokio::test]
    async fn no_servers_is_no_generation() {
        let client = fake_client("unused-value", 0);
        let none = start(
            &client,
            &[],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
        assert!(none.is_none());
        assert_eq!(*client.attempts.lock().unwrap(), 0);
    }

    fn client_stdio(name: &str, env: &[(&str, &str)]) -> ClientServer {
        ClientServer {
            name: name.into(),
            transport: Transport::Stdio {
                command: "files-mcp".into(),
                args: vec!["--stdio".into()],
                env: env
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.to_string()))
                    .collect(),
                cwd: None,
            },
        }
    }

    #[test]
    fn a_client_stdio_child_gets_the_allow_list_and_the_client_variables_in_the_workspace() {
        let key = fake_key();
        let env = snapshot(&[
            ("PATH", "/usr/bin"),
            ("LANG", "C.UTF-8"),
            ("ANTHROPIC_API_KEY", "not-for-the-child"),
            ("SSH_AUTH_SOCK", "/run/agent"),
        ]);
        let server = client_stdio("files", &[("FILES_TOKEN", &key), ("PATH", "/opt/bin")]);
        let Transport::Stdio {
            env: child,
            cwd,
            command,
            args,
        } = client_transport(&server, Path::new("/ws"), &env).unwrap()
        else {
            panic!("not stdio");
        };
        assert_eq!(
            child.keys().map(String::as_str).collect::<Vec<_>>(),
            ["FILES_TOKEN", "LANG", "PATH"],
            "the baseline plus the client's variables, nothing else inherited"
        );
        assert_eq!(child.get("PATH").map(String::as_str), Some("/opt/bin"));
        assert_eq!((command.as_str(), args.len()), ("files-mcp", 1));
        assert_eq!(cwd.as_deref(), Some(Path::new("/ws")));
        let session = SecretSet::new();
        assert!(
            client_values(&session, &[server]).contains_secret(&key),
            "a value the client sent is masked for its server"
        );
        assert!(session.is_empty(), "never in the session's set");
    }

    #[test]
    fn a_client_http_server_keeps_its_headers_masked_and_cannot_set_the_transports_own() {
        let key = fake_key();
        let server = |headers: &[(&str, String)]| ClientServer {
            name: "web".into(),
            transport: Transport::Http {
                url: "http://127.0.0.1:9/mcp".into(),
                headers: headers
                    .iter()
                    .map(|(name, value)| (name.to_string(), value.clone()))
                    .collect(),
            },
        };
        let bearer = server(&[("Authorization", format!("Bearer {key}"))]);
        let Transport::Http { headers, .. } =
            client_transport(&bearer, Path::new("/ws"), &[]).unwrap()
        else {
            panic!("not http");
        };
        assert_eq!(headers.get("Authorization"), Some(&format!("Bearer {key}")));
        assert_eq!(
            client_values(&SecretSet::new(), &[bearer])
                .mask(&format!("t={key}"))
                .text,
            "t=<redacted:secret:33 chars>",
            "the bare token is masked too"
        );

        let error = client_transport(
            &server(&[("Mcp-Session-Id", key.clone())]),
            Path::new("/ws"),
            &[],
        )
        .unwrap_err();
        assert!(
            error.contains("`web`") && error.contains("`Mcp-Session-Id`"),
            "{error}"
        );
        assert!(!error.contains(&key), "{error}");
        let error = client_transport(
            &ClientServer {
                name: "web".into(),
                transport: Transport::Http {
                    url: "file:///etc/passwd".into(),
                    headers: BTreeMap::new(),
                },
            },
            Path::new("/ws"),
            &[],
        )
        .unwrap_err();
        assert!(error.contains("not an http or https URL"), "{error}");
        for (header, value) in [
            ("X Team", "core".to_string()),
            ("X-Team", format!("{key}\r\n")),
        ] {
            let error =
                client_transport(&server(&[(header, value)]), Path::new("/ws"), &[]).unwrap_err();
            assert!(error.contains("not a valid header"), "{error}");
            assert!(!error.contains(&key), "{error}");
        }
    }

    #[tokio::test]
    async fn a_client_server_joins_the_environments_in_one_generation() {
        let client = fake_client("unused-value", 0);
        let generations = start(
            &client,
            &[stdio_spec(&[])],
            &[client_stdio("files", &[])],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("two servers");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut resolved = Vec::new();
        generations
            .extend(&mut tools, &mut resolved, &Arc::new(MaskCounter::new()))
            .unwrap();
        let names: BTreeSet<_> = resolved
            .iter()
            .map(|tool| tool.declaration.name.as_str())
            .collect();
        assert!(
            names.contains("mcp__docs__lookup") && names.contains("mcp__files__lookup"),
            "{names:?}"
        );
        assert_eq!(*client.attempts.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn a_client_server_name_clash_or_invalid_name_fails_before_any_server_starts() {
        for (named, expected) in [
            (
                vec![client_stdio("docs", &[])],
                "MCP server `docs` (from the client): the environment already names",
            ),
            (
                vec![client_stdio("files", &[]), client_stdio("files", &[])],
                "MCP server `files` (from the client): the client already names",
            ),
            (vec![client_stdio("Files", &[])], "MCP server `Files`"),
            (vec![client_stdio("a__b", &[])], "has no `__`"),
        ] {
            let client = fake_client("unused-value", 0);
            let error = start(
                &client,
                &[stdio_spec(&[])],
                &named,
                Path::new("/ws"),
                &snapshot(&[]),
                &Arc::new(MaskCounter::new()),
                BTreeSet::new(),
                outputs(),
                &CancellationToken::new(),
            )
            .await
            .err()
            .expect("the start fails");
            assert!(error.contains(expected), "{error}");
            assert_eq!(
                *client.attempts.lock().unwrap(),
                0,
                "no server started: {error}"
            );
        }
    }

    #[test]
    fn a_long_result_is_stored_and_the_notice_names_its_handle() {
        let session = SecretSet::new();
        let counter = Arc::new(MaskCounter::with_secrets(session.clone()));
        let store = StoredResults {
            store: outputs(),
            scopes: Arc::new(Scopes {
                clients: BTreeMap::from([(
                    "files".to_owned(),
                    client_values(
                        &session,
                        &[client_stdio("files", &[("MODE", "production-mode")])],
                    ),
                )]),
                session,
            }),
            counter: counter.clone(),
        };
        let notice = store.store("files", &"x".repeat(1000));
        assert!(
            notice.starts_with("[stored output: handle_id out-")
                && notice.contains("1000 bytes, complete"),
            "{notice}"
        );

        // A long result is masked with its own server's values only, and counted.
        let call = |name: &str| ToolCall {
            call_id: format!("{name}-call"),
            name: name.into(),
            input: ToolInput::Json("{}".into()),
        };
        let text = "mode=production-mode".to_owned();
        assert_eq!(
            store.mask("files", &call("mcp__files__env"), text.clone()),
            "mode=<redacted:secret:15 chars>"
        );
        assert_eq!(
            store.mask("docs", &call("mcp__docs__env"), text.clone()),
            text
        );
        assert_eq!(counter.take_call("mcp__files__env-call"), 1);
        assert_eq!(counter.take_call("mcp__docs__env-call"), 0);
        assert_eq!(
            counter.take_by_tool(),
            BTreeMap::from([("mcp__files__env".to_owned(), 1)])
        );
    }

    #[tokio::test]
    async fn a_client_value_masks_only_its_own_servers_tools() {
        // Every server answers `value production-mode`; only the client's `files` server
        // was sent that value, so only its tools mask it (ADR-0184).
        let client = fake_client("production-mode", 0);
        let mask = Arc::new(MaskCounter::new());
        let servers = start(
            &client,
            &[stdio_spec(&[])],
            &[client_stdio("files", &[("FILES_MODE", "production-mode")])],
            Path::new("/ws"),
            &snapshot(&[]),
            &mask,
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("two servers");
        assert!(mask.secrets().is_empty(), "the session's set is untouched");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        servers.extend(&mut tools, &mut Vec::new(), &mask).unwrap();
        let mut results = BTreeMap::new();
        for name in ["mcp__docs__leak", "mcp__files__leak"] {
            let tool = tools
                .iter()
                .find(|tool| tool.declaration().name == name)
                .unwrap();
            let call = ToolCall {
                call_id: name.into(),
                name: name.into(),
                input: ToolInput::Json("{}".into()),
            };
            let context = ToolContext {
                cancel: CancellationToken::new(),
            };
            results.insert(name, tool.execute(&call, context).await.content);
        }
        assert_eq!(results["mcp__docs__leak"], "value production-mode");
        assert_eq!(
            results["mcp__files__leak"],
            "value <redacted:secret:15 chars>"
        );
        assert_eq!(mask.take_call("mcp__files__leak"), 1);
        assert_eq!(
            mask.take_by_tool(),
            BTreeMap::from([("mcp__files__leak".to_owned(), 1)])
        );
    }

    fn optional(name: &str, env_pass: &[&str]) -> McpServerSpec {
        McpServerSpec {
            name: name.into(),
            required: false,
            ..stdio_spec(env_pass)
        }
    }

    #[tokio::test]
    async fn an_optional_server_that_fails_is_skipped_with_its_masked_reason() {
        let key = fake_key();
        let mut client = fake_client("unused-value", 0);
        client.refused = vec!["extra"];
        client.unlisted = vec!["quiet"];
        let mask = Arc::new(MaskCounter::new());
        let servers = start(
            &client,
            &[
                stdio_spec(&[]),
                optional("extra", &["EXTRA_TOKEN"]),
                optional("quiet", &[]),
            ],
            &[],
            Path::new("/ws"),
            &snapshot(&[("EXTRA_TOKEN", &key)]),
            &mask,
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("the start succeeds");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut resolved = Vec::new();
        servers.extend(&mut tools, &mut resolved, &mask).unwrap();
        let modules: BTreeSet<_> = resolved.iter().map(|tool| tool.module.as_str()).collect();
        assert_eq!(
            modules,
            BTreeSet::from(["mcp:docs"]),
            "no skipped server's tools"
        );
        let skipped = servers.skipped();
        assert_eq!(
            skipped
                .iter()
                .map(|server| server.name.as_str())
                .collect::<Vec<_>>(),
            ["extra", "quiet"]
        );
        // The refused server echoed the passed credential: masked with the session's set.
        assert!(
            skipped[0].reason.starts_with("could not connect: refused ")
                && skipped[0].reason.contains("<redacted:")
                && !skipped[0].reason.contains(&key),
            "{}",
            skipped[0].reason
        );
        assert_eq!(
            skipped[1].reason,
            "listing its tools failed: protocol error: tools/list: method not found"
        );
        let notices = servers.notices();
        assert_eq!(notices.len(), 2);
        assert!(
            notices[1]
                == format!(
                    "MCP server `quiet` skipped (optional), the session runs without its \
                     tools: {}",
                    skipped[1].reason
                ),
            "{}",
            notices[1]
        );
        assert!(!notices[0].contains(&key), "{}", notices[0]);
        // One retry for the refused server, as for any server.
        assert_eq!(*client.attempts.lock().unwrap(), 1 + 2 + 1);

        // Shown once, right after the first turn starts, when every front end shows notices.
        let recorded = Arc::new(p1_testkit::RecordingEvents::default());
        let (events, _) = skip_notices(recorded.clone(), Some(&servers));
        events.emit(AgentEvent::RequestStarted { request_index: 0 });
        events.emit(AgentEvent::TurnStarted);
        events.emit(AgentEvent::TurnStarted);
        let shown: Vec<String> = recorded
            .events()
            .into_iter()
            .map(|event| match event {
                AgentEvent::ProviderNotice { text } => text,
                AgentEvent::TurnStarted => "turn".into(),
                _ => "other".into(),
            })
            .collect();
        assert_eq!(shown, ["other", "turn", &notices[0], &notices[1], "turn"]);
    }

    #[tokio::test]
    async fn a_required_server_that_cannot_list_its_tools_fails_the_start() {
        let mut client = fake_client("unused-value", 0);
        client.unlisted = vec!["docs"];
        let error = start(
            &client,
            &[stdio_spec(&[])],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("the start fails");
        assert!(
            error.starts_with("MCP server `docs`: listing its tools failed"),
            "{error}"
        );
    }

    /// ADR-0190: an optional server that serves a tool no route can carry, or whose declaration
    /// carries a credential, is skipped with its reason, the value masked; the same server
    /// required fails the start.
    #[tokio::test]
    async fn an_optional_server_with_a_bad_declaration_is_skipped_and_a_required_one_fails() {
        let key = fake_key();
        let remote = |name: &str, input_schema: serde_json::Value| RemoteTool {
            name: name.into(),
            description: "A tool.".into(),
            input_schema,
            read_only: true,
        };
        let mut client = fake_client("unused-value", 0);
        client.listings = vec![
            (
                "dotted",
                vec![remote("read.file", serde_json::json!({"type": "object"}))],
            ),
            (
                "leaky",
                vec![remote(
                    "search",
                    serde_json::json!({"type": "object", "properties": {"q": {
                        "type": "string", "description": format!("send {key}")}}}),
                )],
            ),
            (
                "named",
                vec![remote(&key, serde_json::json!({"type": "object"}))],
            ),
        ];
        let start_with = |specs: Vec<McpServerSpec>| {
            let client = &client;
            async move {
                start(
                    client,
                    &specs,
                    &[],
                    Path::new("/ws"),
                    &snapshot(&[]),
                    &Arc::new(MaskCounter::new()),
                    BTreeSet::new(),
                    outputs(),
                    &CancellationToken::new(),
                )
                .await
            }
        };

        let servers = start_with(vec![
            stdio_spec(&[]),
            optional("dotted", &[]),
            optional("leaky", &[]),
            optional("named", &[]),
        ])
        .await
        .unwrap()
        .expect("the start succeeds without the bad servers");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut resolved = Vec::new();
        servers
            .extend(&mut tools, &mut resolved, &Arc::new(MaskCounter::new()))
            .unwrap();
        let modules: BTreeSet<_> = resolved.iter().map(|tool| tool.module.as_str()).collect();
        assert_eq!(modules, BTreeSet::from(["mcp:docs"]));
        let skipped = servers.skipped();
        assert_eq!(
            skipped
                .iter()
                .map(|server| (server.name.as_str(), server.reason.as_str()))
                .collect::<Vec<_>>(),
            [
                (
                    "dotted",
                    "its tool `read.file` (`mcp__dotted__read.file`) has a name no route can \
                     carry (at most 64 letters, digits, `_` and `-`)"
                ),
                (
                    "leaky",
                    "its tool `search` (`mcp__leaky__search`): the tool's input schema \
                     carries a credential value; the tool is refused"
                ),
                (
                    "named",
                    "its tool whose name carries a credential value: the tool's name carries \
                     a credential value; the tool is refused"
                ),
            ]
        );
        assert!(
            servers
                .notices()
                .iter()
                .all(|notice| !notice.contains(&key))
        );

        for (name, expected) in [
            (
                "dotted",
                "MCP server `dotted`: its tool `read.file` (`mcp__dotted__read.file`) has a name",
            ),
            (
                "leaky",
                "MCP server `leaky`: its tool `search` (`mcp__leaky__search`): the tool's input",
            ),
            // The tool's name is the credential: the error does not show it.
            (
                "named",
                "MCP server `named`: its tool whose name carries a credential value: the tool's \
                 name",
            ),
        ] {
            let required = McpServerSpec {
                name: name.into(),
                ..stdio_spec(&[])
            };
            let error = start_with(vec![required])
                .await
                .err()
                .expect("a required server fails the start");
            assert!(error.starts_with(expected), "{error}");
            assert!(!error.contains(&key), "{error}");
        }
    }

    #[tokio::test]
    async fn a_client_server_that_fails_is_skipped_and_its_values_masked() {
        let mut client = fake_client("unused-value", 0);
        client.refused = vec!["files"];
        let mask = Arc::new(MaskCounter::new());
        let servers = start(
            &client,
            &[],
            &[client_stdio("files", &[("FILES_MODE", "production-mode")])],
            Path::new("/ws"),
            &snapshot(&[]),
            &mask,
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("the start succeeds without the server");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        servers.extend(&mut tools, &mut Vec::new(), &mask).unwrap();
        assert!(tools.is_empty());
        let skipped = servers.skipped();
        assert_eq!(skipped.len(), 1);
        assert_eq!(
            skipped[0].reason,
            "could not connect: refused <redacted:secret:15 chars>"
        );
        assert!(mask.secrets().is_empty(), "the session's set is untouched");
    }

    #[tokio::test]
    async fn a_client_servers_tool_name_clash_still_fails_the_start() {
        let client = fake_client("unused-value", 0);
        let error = start(
            &client,
            &[],
            &[client_stdio("files", &[])],
            Path::new("/ws"),
            &snapshot(&[]),
            &Arc::new(MaskCounter::new()),
            BTreeSet::from(["mcp__files__lookup".to_owned()]),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .err()
        .expect("the start fails");
        assert!(error.contains("clashes"), "{error}");
    }
}
