//! MCP client port (seam 10, ADR-0181): the host-side contract between the tool generation
//! that presents an external MCP server's tools and the native adapter that talks to it.
//!
//! A WebAssembly module can open neither a process nor a socket, so the one adapter
//! (`p1-mcp-client`, stdio and streamable HTTP) is a host OS service (ADR-0081). Nothing here
//! names a wire format: JSON-RPC framing, the MCP method names and protocol versions are the
//! adapter's. Every served tool reaches an agent as an ordinary [`Tool`](crate::Tool), so
//! authorization, redaction and the journal treat it as any other tool.

use std::collections::BTreeMap;
use std::path::PathBuf;

use crate::{BoxFuture, CancellationToken};

/// How the host reaches one server. The host builds it from the environment file's
/// `[[mcp_servers]]` entry; a guest never names a server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Transport {
    /// A child process speaking newline-delimited JSON-RPC on stdin and stdout. `env` is the
    /// child's WHOLE environment: the adapter clears everything else, so the host decides
    /// what a server inherits (no credential reaches it by accident).
    Stdio {
        command: String,
        args: Vec<String>,
        env: BTreeMap<String, String>,
        cwd: Option<PathBuf>,
    },
    /// Streamable HTTP: JSON-RPC POSTed to one endpoint, answered as JSON or as an SSE
    /// stream. `url` is an absolute `http` or `https` URL; `headers` are sent with every
    /// request.
    Http {
        url: String,
        headers: BTreeMap<String, String>,
    },
}

/// One tool a server serves, as its `tools/list` answer describes it.
#[derive(Debug, Clone, PartialEq)]
pub struct RemoteTool {
    /// The server's own name for the tool (the `variant` of the generated tool's identity).
    pub name: String,
    pub description: String,
    /// The JSON Schema of the tool's arguments.
    pub input_schema: serde_json::Value,
    /// The server marked the tool read-only (`annotations.readOnlyHint`). Only a hint: the
    /// host trusts it as far as the environment trusts the server it named.
    pub read_only: bool,
}

/// One resource a server lists.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteResource {
    pub uri: String,
    pub name: String,
    pub description: Option<String>,
    pub mime_type: Option<String>,
}

/// One page of `resources/list`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResourcePage {
    pub resources: Vec<RemoteResource>,
    /// Pass back to [`McpServer::resources`] for the next page; `None` on the last page.
    pub next_cursor: Option<String>,
}

/// One block of a tool result or a resource's contents.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ContentBlock {
    Text {
        text: String,
    },
    /// Base64 image data.
    Image {
        mime_type: String,
        data: String,
    },
    /// Base64 audio data.
    Audio {
        mime_type: String,
        data: String,
    },
    /// An embedded resource: text, or base64 `blob`.
    Resource {
        uri: String,
        mime_type: Option<String>,
        text: Option<String>,
        blob: Option<String>,
    },
    /// A link to a resource the server can read.
    ResourceLink {
        uri: String,
        name: Option<String>,
        mime_type: Option<String>,
    },
    /// A block type this port does not know; `kind` is the wire's type tag.
    Unknown {
        kind: String,
    },
}

/// Why an MCP operation failed. Messages name the server and the operation, never a
/// credential: the host builds them from the transport's error, not from its headers.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum McpError {
    /// The server could not be started or reached.
    #[error("could not connect: {0}")]
    Connect(String),
    /// The server broke the protocol: malformed JSON-RPC, a missing result, an unsupported
    /// protocol version.
    #[error("protocol error: {0}")]
    Protocol(String),
    /// The server answered a request with a JSON-RPC error.
    #[error("server error {code}: {message}")]
    Server { code: i64, message: String },
    /// The tool ran and reported failure (`isError`); `content` is what it said.
    #[error("the tool reported an error")]
    ToolFailed { content: Vec<ContentBlock> },
    /// The connection ended (the child exited, the stream closed).
    #[error("connection closed: {0}")]
    Closed(String),
    /// The operation was cancelled.
    #[error("cancelled")]
    Cancelled,
}

/// The native adapter: opens one connection per configured server.
pub trait McpClient: Send + Sync {
    /// Start or reach the server `name` over `transport` and complete the MCP handshake.
    /// Cancelling `cancel` abandons the attempt with [`McpError::Cancelled`].
    fn connect(
        &self,
        name: &str,
        transport: Transport,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Box<dyn McpServer>, McpError>>;
}

/// One initialized connection. Dropping it ends the connection (a stdio child is killed).
pub trait McpServer: Send + Sync {
    /// Every tool the server serves (all `tools/list` pages).
    fn tools(&self) -> BoxFuture<'_, Result<Vec<RemoteTool>, McpError>>;

    /// Call the tool the server names `raw_name`. Cancelling `cancel` tells the server and
    /// returns [`McpError::Cancelled`].
    fn call(
        &self,
        raw_name: &str,
        args: serde_json::Value,
        cancel: CancellationToken,
    ) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>>;

    /// One page of the server's resources.
    fn resources(&self, cursor: Option<String>) -> BoxFuture<'_, Result<ResourcePage, McpError>>;

    /// The contents of one resource.
    fn read_resource(&self, uri: &str) -> BoxFuture<'_, Result<Vec<ContentBlock>, McpError>>;
}
