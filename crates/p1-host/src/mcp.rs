//! The session's MCP servers (ADR-0181): the environment's `[[mcp_servers]]` and the servers
//! the session's client names (ACP `mcpServers`, #695, ADR-0182), connected once at session
//! start, and their tool generation added to the main agent's assembly.
//!
//! The host decides everything a server is given. A stdio child gets the shell's allow-listed
//! variables (no credential among them), the entry's literal `env` and the variables its
//! `env_pass` names; an HTTP server gets the entry's headers and, from `bearer_token_env`, one
//! bearer token. A client's stdio server gets the same allow-list and the variables the client
//! lists, in the session's folder; a client's HTTP server gets the client's headers. Every value
//! passed by name, and every value a client sends, is registered as a secret, so it is masked
//! wherever it would show; the names follow an environment's rules, and a name two servers
//! share fails the start. The generated tools are wrapped like every assembled tool (masking), their
//! long results go to the session's output store, and they join the assembly before the agent
//! is built, so the core's first `Environment` record journals the generation and the
//! authorization policy sees each call as it sees any other.

use std::collections::{BTreeMap, BTreeSet};
use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use p1_assembly::{McpServerSpec, McpServerTransport, ResolvedTool};
use p1_contracts::mcp::{ClientServer, McpClient, McpServer, Transport};
use p1_contracts::{CancellationToken, Tool};
use p1_mcp_client::{Generation, NativeMcpClient, ResultStore, ServerConnection, ToolGenerations};
use p1_module_runtime::OutputStore;
use p1_module_runtime::outputs::Capture;
use p1_module_runtime::process::{ENV_ALLOW, ENV_ALLOW_PREFIXES};
use p1_redact::{MaskCounter, SecretSet, redacted};

/// How long one connection attempt (start, handshake) may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);
/// How long discovering every server's tools may take.
const DISCOVERY_TIMEOUT: Duration = Duration::from_secs(60);
/// Attempts per server: one retry (a longer reconnect policy is a later slice).
const CONNECT_ATTEMPTS: usize = 2;

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
) -> Result<Option<Arc<ToolGenerations>>, String> {
    let specs = p1_assembly::load_mcp_servers(environment, &deps.environment_dirs)
        .map_err(|error| error.to_string())?;
    if specs.is_empty() && client.is_empty() {
        return Ok(None);
    }
    let snapshot = deps
        .shell_env
        .clone()
        .unwrap_or_else(|| std::env::vars_os().collect());
    let reserved = assembled
        .tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect();
    let Some(generations) = start(
        &NativeMcpClient::new(),
        &specs,
        client,
        workspace,
        &snapshot,
        &deps.secrets,
        reserved,
        deps.tool_outputs.clone(),
        cancel,
    )
    .await?
    else {
        return Ok(None);
    };
    extend(
        &mut assembled.tools,
        &mut assembled.resolved.tools,
        &generations.current(),
        mask,
    )?;
    Ok(Some(generations))
}

/// Connect every server `specs` and `named` (the client's) name and discover generation 1.
/// `None` when there is none. Every name and transport is checked before the first server
/// starts. `reserved` holds the names the main agent's assembly already uses; a clash with one
/// fails the start, as a duplicate tool name does.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn start(
    client: &dyn McpClient,
    specs: &[McpServerSpec],
    named: &[ClientServer],
    workspace: &Path,
    snapshot: &[(OsString, OsString)],
    secrets: &SecretSet,
    reserved: BTreeSet<String>,
    outputs: Arc<OutputStore>,
    cancel: &CancellationToken,
) -> Result<Option<Arc<ToolGenerations>>, String> {
    if specs.is_empty() && named.is_empty() {
        return Ok(None);
    }
    let mut transports = Vec::with_capacity(specs.len() + named.len());
    for spec in specs {
        transports.push((
            spec.name.clone(),
            transport(spec, workspace, snapshot, secrets)?,
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
        if let Some(index) = transports.iter().position(|(taken, _)| taken == name) {
            let owner = if index < specs.len() {
                "the environment"
            } else {
                "the client"
            };
            return Err(format!(
                "MCP server `{name}` (from the client): {owner} already names a server `{name}`"
            ));
        }
        let transport = client_transport(server, workspace, snapshot, secrets)?;
        transports.push((name.clone(), transport));
    }
    let mut connections = Vec::with_capacity(transports.len());
    for (name, transport) in transports {
        let server = connect(client, &name, transport, cancel).await?;
        connections.push(ServerConnection {
            name,
            server: Arc::from(server),
        });
    }
    let store: Arc<dyn ResultStore> = Arc::new(StoredResults {
        store: outputs,
        secrets: secrets.clone(),
    });
    let discovery = tokio::time::timeout(
        DISCOVERY_TIMEOUT,
        ToolGenerations::discover(connections, reserved, Some(store)),
    );
    let generations = tokio::select! {
        biased;
        () = cancel.cancelled() => {
            return Err("MCP servers: cancelled while discovering their tools".into());
        }
        discovered = discovery => discovered.map_err(|_| {
            format!(
                "MCP servers: discovering their tools took longer than {} s",
                DISCOVERY_TIMEOUT.as_secs()
            )
        })?,
    }
    .map_err(|error| error.to_string())?;
    Ok(Some(Arc::new(generations)))
}

/// Add `generation`'s tools to an assembly: masked like every assembled tool, refused when a
/// declaration carries a credential or its name is already taken (a switched assembly can hold
/// a name the start's did not), each with a resolved entry naming `mcp:<server>`.
pub(crate) fn extend(
    tools: &mut Vec<Arc<dyn Tool>>,
    resolved: &mut Vec<ResolvedTool>,
    generation: &Generation,
    mask: &Arc<MaskCounter>,
) -> Result<(), String> {
    for tool in generation.mcp_tools() {
        let name = &tool.declaration().name;
        if tools.iter().any(|taken| &taken.declaration().name == name) {
            return Err(format!(
                "the MCP tool name `{name}` clashes with another tool of the same agent"
            ));
        }
        p1_redact::check_declaration(tool.declaration(), tool.identity(), mask.secrets())
            .map_err(|message| format!("MCP tool `{}`: {message}", tool.declaration().name))?;
        resolved.push(ResolvedTool {
            module: tool.identity().implementation.clone(),
            declaration: tool.declaration().clone(),
            identity: tool.identity().clone(),
        });
        tools.push(redacted(tool.clone(), mask));
    }
    Ok(())
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
/// server); an HTTP server gets the client's headers, none of the ones the transport sets. Every
/// value the client sent is registered as a secret: any of them may be a credential.
pub(crate) fn client_transport(
    server: &ClientServer,
    workspace: &Path,
    snapshot: &[(OsString, OsString)],
    secrets: &SecretSet,
) -> Result<Transport, String> {
    let name = &server.name;
    match &server.transport {
        Transport::Stdio {
            command, args, env, ..
        } => {
            let mut child = allowed_environment(snapshot);
            for (variable, value) in env {
                register(secrets, value);
                child.insert(variable.clone(), value.clone());
            }
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
                register(secrets, value);
            }
            Ok(Transport::Http {
                url: url.clone(),
                headers: headers.clone(),
            })
        }
    }
}

/// Register a value the client sent, and the credential of a `<scheme> <credential>` header
/// value (`Bearer …`) on its own, as a server may answer with the bare token.
fn register(secrets: &SecretSet, value: &str) {
    secrets.register(value);
    if let Some((_, credential)) = value.trim().rsplit_once(char::is_whitespace) {
        secrets.register(credential);
    }
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

async fn connect(
    client: &dyn McpClient,
    name: &str,
    transport: Transport,
    cancel: &CancellationToken,
) -> Result<Box<dyn McpServer>, String> {
    let mut failure = String::new();
    for _ in 0..CONNECT_ATTEMPTS {
        if cancel.is_cancelled() {
            return Err(format!("MCP server `{name}`: cancelled while connecting"));
        }
        let attempt = client.connect(name, transport.clone(), cancel.clone());
        match tokio::time::timeout(CONNECT_TIMEOUT, attempt).await {
            Ok(Ok(server)) => return Ok(server),
            Ok(Err(p1_contracts::mcp::McpError::Cancelled)) => {
                return Err(format!("MCP server `{name}`: cancelled while connecting"));
            }
            Ok(Err(error)) => failure = error.to_string(),
            Err(_) => {
                failure = format!("no answer within {} s", CONNECT_TIMEOUT.as_secs());
            }
        }
    }
    Err(format!("MCP server `{name}`: {failure}"))
}

/// The output store behind a generated tool's long results (ADR-0109).
struct StoredResults {
    store: Arc<OutputStore>,
    secrets: SecretSet,
}

impl ResultStore for StoredResults {
    fn mask(&self, text: String) -> String {
        p1_redact::redact_with(&text, &self.secrets).text
    }

    fn store(&self, text: &str) -> String {
        let info = self.store.store_text(self.secrets.clone(), text);
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
    struct FakeClient {
        server: Arc<FakeServer>,
        failures: Mutex<usize>,
        attempts: Mutex<usize>,
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
            _name: &str,
            _transport: Transport,
            _cancel: CancellationToken,
        ) -> BoxFuture<'_, Result<Box<dyn McpServer>, McpError>> {
            *self.attempts.lock().unwrap() += 1;
            let mut failures = self.failures.lock().unwrap();
            let result: Result<Box<dyn McpServer>, McpError> = if *failures > 0 {
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
            &secrets,
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
        extend(&mut tools, &mut resolved, &generations.current(), &mask).unwrap();
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
            &SecretSet::new(),
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
        extend(&mut tools, &mut resolved, &generations.current(), &mask).unwrap();
        let mut switched = vec![tools[0].clone()];
        let error = extend(
            &mut switched,
            &mut Vec::new(),
            &generations.current(),
            &mask,
        )
        .unwrap_err();
        assert!(error.contains("mcp__docs__lookup"), "{error}");
    }

    #[tokio::test]
    async fn a_cancelled_start_stops_connecting() {
        let client = fake_client("unused-value", 0);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let result = start(
            &client,
            &[stdio_spec(&[])],
            &[],
            Path::new("/ws"),
            &snapshot(&[]),
            &SecretSet::new(),
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
            &SecretSet::new(),
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
            &SecretSet::new(),
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
            &SecretSet::new(),
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
        let secrets = SecretSet::new();
        let env = snapshot(&[
            ("PATH", "/usr/bin"),
            ("LANG", "C.UTF-8"),
            ("ANTHROPIC_API_KEY", "not-for-the-child"),
            ("SSH_AUTH_SOCK", "/run/agent"),
        ]);
        let Transport::Stdio {
            env: child,
            cwd,
            command,
            args,
        } = client_transport(
            &client_stdio("files", &[("FILES_TOKEN", &key), ("PATH", "/opt/bin")]),
            Path::new("/ws"),
            &env,
            &secrets,
        )
        .unwrap()
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
        assert!(
            secrets.contains_secret(&key),
            "a value the client sent is masked from now on"
        );
    }

    #[test]
    fn a_client_http_server_keeps_its_headers_masked_and_cannot_set_the_transports_own() {
        let key = fake_key();
        let secrets = SecretSet::new();
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
        let Transport::Http { headers, .. } = client_transport(
            &server(&[("Authorization", format!("Bearer {key}"))]),
            Path::new("/ws"),
            &[],
            &secrets,
        )
        .unwrap() else {
            panic!("not http");
        };
        assert_eq!(headers.get("Authorization"), Some(&format!("Bearer {key}")));
        assert!(
            secrets.contains_secret(&key),
            "the bare token is masked too"
        );

        let error = client_transport(
            &server(&[("Mcp-Session-Id", key.clone())]),
            Path::new("/ws"),
            &[],
            &SecretSet::new(),
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
            &SecretSet::new(),
        )
        .unwrap_err();
        assert!(error.contains("not an http or https URL"), "{error}");
        for (header, value) in [
            ("X Team", "core".to_string()),
            ("X-Team", format!("{key}\r\n")),
        ] {
            let error = client_transport(
                &server(&[(header, value)]),
                Path::new("/ws"),
                &[],
                &SecretSet::new(),
            )
            .unwrap_err();
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
            &SecretSet::new(),
            BTreeSet::new(),
            outputs(),
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .expect("two servers");
        let mut tools: Vec<Arc<dyn Tool>> = Vec::new();
        let mut resolved = Vec::new();
        extend(
            &mut tools,
            &mut resolved,
            &generations.current(),
            &Arc::new(MaskCounter::new()),
        )
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
                &SecretSet::new(),
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
        let store = StoredResults {
            store: outputs(),
            secrets: SecretSet::new(),
        };
        let notice = store.store(&"x".repeat(1000));
        assert!(
            notice.starts_with("[stored output: handle_id out-")
                && notice.contains("1000 bytes, complete"),
            "{notice}"
        );
    }
}
