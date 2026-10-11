//! The native MCP client (seam 10, ADR-0181): the one adapter behind the
//! [`McpClient`](p1_contracts::mcp::McpClient) port, and the tool generation that presents a
//! server's tools to an agent.
//!
//! A WebAssembly module can open neither a process nor a socket, so this crate is a host OS
//! service (`foundation` in the shipping audit, ADR-0081): it starts stdio servers and talks
//! streamable HTTP, with its own JSON-RPC framing over the workspace's existing dependencies
//! (no MCP SDK). What a served tool does is decided by the server; how a call is authorized,
//! masked and journalled is decided by the host, exactly as for any other tool.
//!
//! - [`NativeMcpClient`] connects ([`Transport::Stdio`] or [`Transport::Http`]) and completes
//!   the MCP handshake.
//! - [`generation`] builds one [`Tool`](p1_contracts::Tool) per served tool, named
//!   `mcp__<server>__<raw>`, and swaps whole generations.

pub mod generation;
mod http;
mod rpc;
mod stdio;
mod wire;

pub use generation::{
    Generation, GenerationError, McpTool, ResultStore, ServerConnection, ToolGenerations, declare,
    tool_name,
};

use p1_contracts::mcp::{
    ContentBlock, McpClient, McpError, McpServer, RemoteTool, ResourcePage, Transport,
};
use p1_contracts::{BoxFuture, CancellationToken};
use serde_json::{Value, json};

/// The largest single message either transport accepts from a server.
pub const MAX_MESSAGE_BYTES: usize = 16 * 1024 * 1024;

/// How long telling a server about a cancellation may take before the client stops waiting.
const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// The protocol version this client asks for, and every version it accepts in the answer.
const PROTOCOL_VERSION: &str = "2025-06-18";
const SUPPORTED_VERSIONS: &[&str] = &["2025-06-18", "2025-03-26", "2024-11-05"];
/// The most `tools/list` or `resources/list` pages one listing follows, so a server that
/// keeps handing out cursors cannot hold a session start forever.
const MAX_PAGES: usize = 100;

/// One open JSON-RPC conversation, whatever carries it.
pub(crate) trait Channel: Send + Sync {
    fn request<'a>(
        &'a self,
        method: &'a str,
        params: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<Value, McpError>>;

    fn notify<'a>(&'a self, method: &'a str, params: Value) -> BoxFuture<'a, Result<(), McpError>>;

    /// The version `initialize` negotiated, before any later message is sent.
    fn negotiated(&self, _version: &str) {}
}

/// The adapter: stdio children and streamable HTTP.
#[derive(Debug, Default, Clone, Copy)]
pub struct NativeMcpClient;

impl NativeMcpClient {
    pub fn new() -> Self {
        Self
    }
}

impl McpClient for NativeMcpClient {
    fn connect(
        &self,
        name: &str,
        transport: Transport,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn McpServer>, McpError>> {
        let name = name.to_owned();
        Box::pin(async move {
            let server = match transport {
                Transport::Stdio {
                    command,
                    args,
                    env,
                    cwd,
                } => {
                    let channel =
                        stdio::StdioChannel::spawn(&command, &args, &env, cwd.as_deref())?;
                    initialize(&channel, cancel).await?;
                    Server {
                        name,
                        channel: Box::new(channel),
                    }
                }
                Transport::Http { url, headers } => {
                    let channel = http::HttpChannel::new(&url, &headers)?;
                    initialize(&channel, cancel).await?;
                    Server {
                        name,
                        channel: Box::new(channel),
                    }
                }
            };
            Ok(Box::new(server) as Box<dyn McpServer>)
        })
    }
}

/// The handshake: `initialize`, a version this client speaks, `notifications/initialized`.
async fn initialize(channel: &dyn Channel, cancel: CancellationToken) -> Result<(), McpError> {
    let params = json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": { "name": "p1", "version": env!("CARGO_PKG_VERSION") },
    });
    let result = channel.request("initialize", params, cancel).await?;
    let version = result
        .get("protocolVersion")
        .and_then(Value::as_str)
        .ok_or_else(|| McpError::Protocol("`initialize` named no protocol version".into()))?;
    if !SUPPORTED_VERSIONS.contains(&version) {
        return Err(McpError::Protocol(format!(
            "the server speaks protocol version {version}, which this client does not"
        )));
    }
    channel.negotiated(version);
    channel.notify("notifications/initialized", json!({})).await
}

/// One initialized server.
struct Server {
    name: String,
    channel: Box<dyn Channel>,
}

impl Server {
    /// Every page of a cursor-paginated listing, each page's items under `key`.
    async fn list_all(&self, method: &str, key: &str) -> Result<Vec<Value>, McpError> {
        let mut items = Vec::new();
        let mut cursor: Option<String> = None;
        for _ in 0..MAX_PAGES {
            let params = match &cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self
                .channel
                .request(method, params, CancellationToken::new())
                .await?;
            let (page, next) = wire::page(&result, key)?;
            items.extend(page);
            match next {
                Some(next) if Some(&next) != cursor.as_ref() => cursor = Some(next),
                Some(_) => {
                    return Err(McpError::Protocol(format!(
                        "`{method}` repeated its cursor"
                    )));
                }
                None => return Ok(items),
            }
        }
        Err(McpError::Protocol(format!(
            "`{method}` from server `{}` did not end within {MAX_PAGES} pages",
            self.name
        )))
    }
}

impl McpServer for Server {
    fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>> {
        Box::pin(async move {
            self.list_all("tools/list", "tools")
                .await?
                .iter()
                .map(wire::remote_tool)
                .collect()
        })
    }

    fn call(
        &self,
        raw_name: &str,
        args: Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
        let params = json!({ "name": raw_name, "arguments": args });
        Box::pin(async move {
            let result = self.channel.request("tools/call", params, cancel).await?;
            wire::call_result(&result)
        })
    }

    fn resources(&self, cursor: Option<String>) -> BoxFuture<'_, Result<ResourcePage, McpError>> {
        Box::pin(async move {
            let params = match cursor {
                Some(cursor) => json!({ "cursor": cursor }),
                None => json!({}),
            };
            let result = self
                .channel
                .request("resources/list", params, CancellationToken::new())
                .await?;
            let (page, next_cursor) = wire::page(&result, "resources")?;
            Ok(ResourcePage {
                resources: page
                    .iter()
                    .map(wire::remote_resource)
                    .collect::<Result<_, _>>()?,
                next_cursor,
            })
        })
    }

    fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>> {
        let params = json!({ "uri": uri });
        Box::pin(async move {
            let result = self
                .channel
                .request("resources/read", params, CancellationToken::new())
                .await?;
            wire::resource_contents(&result)
        })
    }
}
