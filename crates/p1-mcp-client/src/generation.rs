//! MCP tool generations (ADR-0181): the tools of every configured server, discovered once and
//! presented as ordinary [`Tool`]s.
//!
//! One served tool becomes one [`McpTool`] named `mcp__<server>__<raw>` with the identity
//! `mcp:<server>` / `<raw>`, so the journal names the server and the server's own tool name
//! with every call. Its [`Effect`] is [`Effect::Executes`] unless the server marked the tool
//! read-only; whether a call runs is the authorization policy's decision, as for any tool.
//!
//! A [`Generation`] is built whole and swapped whole: a name clash (with another served tool or
//! with a name the agent's assembly already holds) or a name no provider can carry fails the
//! build, and [`ToolGenerations::refresh`] keeps the previous generation when the build fails.
//! This is the one deliberate exception to ADR-0103's fixed tool snapshot: a generation reaches
//! an agent only as part of its assembled tool set, so the core's `Environment` record journals
//! every generation an agent runs with.

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex, MutexGuard};

use p1_contracts::history::{ToolCall, ToolStatus};
use p1_contracts::mcp::{ContentBlock, McpError, McpServer, RemoteTool};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolContext, ToolDeclaration,
    ToolIdentity, ToolOutcome,
};
use serde_json::Value;

/// The prefix of every generated tool name.
pub const NAME_PREFIX: &str = "mcp__";
/// The longest tool name every route carries (Anthropic and OpenAI both cap names at 64).
pub const MAX_NAME_LEN: usize = 64;
/// The most bytes of one result the model is shown; a longer result is cut to its head and
/// tail, and kept whole in the host's output store when there is one (ADR-0109). The same
/// 25 000-byte head and tail a shell command's output keeps.
pub const MAX_RESULT_BYTES: usize = 50_000;
const KEPT_BYTES: usize = MAX_RESULT_BYTES / 2;

/// The model-facing name of server `server`'s tool `raw`.
pub fn tool_name(server: &str, raw: &str) -> String {
    format!("{NAME_PREFIX}{server}__{raw}")
}

/// Whether every route can carry `name`: 1 to [`MAX_NAME_LEN`] ASCII letters, digits, `_`
/// and `-`.
fn carriable(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

/// Where a result too long to show whole is kept: the host's output store. `server` names
/// the configured server whose tool produced `text`, as its values mask only its own results
/// (ADR-0184).
pub trait ResultStore: Send + Sync {
    /// Mask `text`, the result of `call`, whole, before it is cut, so no credential is split
    /// across the cut and shown in part. The store counts what it masked against `call`:
    /// the tool's own masking finds it already masked.
    fn mask(&self, server: &str, call: &ToolCall, text: String) -> String;

    /// Keep the whole text, masked, and return the one line that tells the model how to page
    /// it. May block on the store's writer: it is called off the async runtime.
    fn store(&self, server: &str, text: &str) -> String;
}

/// One connected server under its configured name.
#[derive(Clone)]
pub struct ServerConnection {
    pub name: String,
    pub server: Arc<dyn McpServer>,
}

/// Why a generation could not be built.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum GenerationError {
    #[error("MCP server `{server}`: listing its tools failed: {error}")]
    Discovery { server: String, error: McpError },
    #[error(
        "MCP server `{server}` serves a tool `{raw}` whose name `{name}` no route can carry \
         (at most {MAX_NAME_LEN} letters, digits, `_` and `-`)"
    )]
    InvalidName {
        server: String,
        raw: String,
        name: String,
    },
    #[error("the MCP tool name `{name}` clashes with another tool of the same agent")]
    Clash { name: String },
}

/// One generated tool.
pub struct McpTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    server_name: String,
    raw: String,
    read_only: bool,
    server: Arc<dyn McpServer>,
    store: Option<Arc<dyn ResultStore>>,
}

impl McpTool {
    fn new(
        connection: &ServerConnection,
        remote: RemoteTool,
        store: Option<Arc<dyn ResultStore>>,
    ) -> Self {
        Self {
            declaration: ToolDeclaration {
                name: tool_name(&connection.name, &remote.name),
                description: remote.description,
                kind: DeclarationKind::Function {
                    input_schema: remote.input_schema,
                },
            },
            identity: ToolIdentity {
                implementation: format!("mcp:{}", connection.name),
                variant: remote.name.clone(),
            },
            server_name: connection.name.clone(),
            raw: remote.name,
            read_only: remote.read_only,
            server: connection.server.clone(),
            store,
        }
    }

    /// The configured server's name.
    pub fn server(&self) -> &str {
        &self.server_name
    }

    /// Cut a result longer than [`MAX_RESULT_BYTES`] to its head and tail; the whole text goes
    /// to the store.
    async fn bound(&self, call: &ToolCall, text: String) -> String {
        if text.len() <= MAX_RESULT_BYTES {
            return text;
        }
        let Some(store) = &self.store else {
            return cut(&text, "[the whole result was not stored]");
        };
        let text = store.mask(&self.server_name, call, text);
        if text.len() <= MAX_RESULT_BYTES {
            return text;
        }
        let store = store.clone();
        let server = self.server_name.clone();
        // The store waits for its writer thread; keep that off the runtime's thread.
        match tokio::task::spawn_blocking(move || {
            let notice = store.store(&server, &text);
            cut(&text, &notice)
        })
        .await
        {
            Ok(shown) => shown,
            Err(_) => "[the result could not be stored or shown]".to_owned(),
        }
    }
}

/// `text`, longer than [`MAX_RESULT_BYTES`], as its head and tail with `notice` after them.
fn cut(text: &str, notice: &str) -> String {
    let mut head = KEPT_BYTES;
    while !text.is_char_boundary(head) {
        head -= 1;
    }
    let mut tail = text.len() - KEPT_BYTES;
    while !text.is_char_boundary(tail) {
        tail += 1;
    }
    format!(
        "{}\n[… {} bytes omitted …]\n{}\n{notice}",
        &text[..head],
        tail - head,
        &text[tail..]
    )
}

impl Tool for McpTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        if self.read_only {
            Effect::ReadOnly
        } else {
            Effect::Executes
        }
    }

    fn describe(&self, _call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "mcp",
            target: Some(format!("{}/{}", self.server_name, self.raw)),
            edit: None,
            // What a tool does is the server's to say; only a read-only hint rules out harm.
            destructive: !self.read_only,
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let raw = call.input.raw();
            let args = if raw.trim().is_empty() {
                Value::Object(Default::default())
            } else {
                match serde_json::from_str::<Value>(raw) {
                    Ok(args @ Value::Object(_)) => args,
                    Ok(_) => return ToolOutcome::error("the arguments must be a JSON object"),
                    Err(error) => {
                        return ToolOutcome::error(format!("the arguments are not JSON: {error}"));
                    }
                }
            };
            match self.server.call(&self.raw, args, context.cancel).await {
                Ok(blocks) => ToolOutcome::ok(self.bound(call, render(&blocks)).await),
                Err(McpError::ToolFailed { content }) if content.is_empty() => {
                    ToolOutcome::error("the tool reported an error")
                }
                Err(McpError::ToolFailed { content }) => {
                    ToolOutcome::error(self.bound(call, render(&content)).await)
                }
                Err(McpError::Cancelled) => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: "cancelled".into(),
                },
                Err(error) => ToolOutcome::error(
                    self.bound(call, format!("MCP server `{}`: {error}", self.server_name))
                        .await,
                ),
            }
        })
    }
}

/// The text the model is shown for a result. Binary data is named, never inlined: base64
/// would only spend the context window.
pub fn render(blocks: &[ContentBlock]) -> String {
    let parts: Vec<String> = blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            ContentBlock::Image { mime_type, data } => {
                format!(
                    "[image: {mime_type}, {} bytes of base64 not shown]",
                    data.len()
                )
            }
            ContentBlock::Audio { mime_type, data } => {
                format!(
                    "[audio: {mime_type}, {} bytes of base64 not shown]",
                    data.len()
                )
            }
            ContentBlock::Resource {
                uri,
                text: Some(text),
                ..
            } => format!("[resource {uri}]\n{text}"),
            ContentBlock::Resource {
                uri,
                mime_type,
                blob,
                ..
            } => format!(
                "[resource {uri}: {}, {} bytes of base64 not shown]",
                mime_type.as_deref().unwrap_or("binary"),
                blob.as_ref().map_or(0, String::len)
            ),
            ContentBlock::ResourceLink { uri, name, .. } => match name {
                Some(name) => format!("[resource link: {name} {uri}]"),
                None => format!("[resource link: {uri}]"),
            },
            ContentBlock::Unknown { kind } => format!("[unsupported content block: {kind}]"),
        })
        .collect();
    parts.join("\n")
}

/// The tools of every configured server at one point in time.
pub struct Generation {
    number: u64,
    tools: Vec<Arc<McpTool>>,
}

impl std::fmt::Debug for Generation {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Generation")
            .field("number", &self.number)
            .field(
                "tools",
                &self
                    .tools
                    .iter()
                    .map(|tool| tool.declaration.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .finish()
    }
}

impl Generation {
    /// 1 for the session start's generation, one more for every successful refresh.
    pub fn number(&self) -> u64 {
        self.number
    }

    /// Every generated tool, in server order and then in the server's own order.
    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        self.tools
            .iter()
            .map(|tool| tool.clone() as Arc<dyn Tool>)
            .collect()
    }

    /// The generated tools themselves.
    pub fn mcp_tools(&self) -> &[Arc<McpTool>] {
        &self.tools
    }
}

/// List every server's tools, in order; the first failure ends the listing.
async fn list(
    servers: &[ServerConnection],
) -> Result<Vec<(ServerConnection, Vec<RemoteTool>)>, GenerationError> {
    let mut listed = Vec::with_capacity(servers.len());
    for connection in servers {
        let tools =
            connection
                .server
                .tools()
                .await
                .map_err(|error| GenerationError::Discovery {
                    server: connection.name.clone(),
                    error,
                })?;
        listed.push((connection.clone(), tools));
    }
    Ok(listed)
}

/// Build one generation from every server's listed tools, in order. `reserved` holds the
/// names the agent's assembly already uses.
fn build(
    number: u64,
    listed: Vec<(ServerConnection, Vec<RemoteTool>)>,
    reserved: &BTreeSet<String>,
    store: &Option<Arc<dyn ResultStore>>,
) -> Result<Generation, GenerationError> {
    let mut names = reserved.clone();
    let mut tools = Vec::new();
    for (connection, remotes) in listed {
        for remote in remotes {
            let name = tool_name(&connection.name, &remote.name);
            if !carriable(&name) {
                return Err(GenerationError::InvalidName {
                    server: connection.name.clone(),
                    raw: remote.name,
                    name,
                });
            }
            if !names.insert(name.clone()) {
                return Err(GenerationError::Clash { name });
            }
            tools.push(Arc::new(McpTool::new(&connection, remote, store.clone())));
        }
    }
    Ok(Generation { number, tools })
}

/// The connected servers of one agent and their current generation.
pub struct ToolGenerations {
    servers: Vec<ServerConnection>,
    reserved: BTreeSet<String>,
    store: Option<Arc<dyn ResultStore>>,
    current: Mutex<Arc<Generation>>,
}

impl ToolGenerations {
    /// Discover every server's tools once and build generation 1.
    pub async fn discover(
        servers: Vec<ServerConnection>,
        reserved: BTreeSet<String>,
        store: Option<Arc<dyn ResultStore>>,
    ) -> Result<Self, GenerationError> {
        Self::with_tools(list(&servers).await?, reserved, store)
    }

    /// Build generation 1 from tools the caller already listed, one entry per server: the
    /// host lists each server itself, so a server it may skip cannot fail the others
    /// (ADR-0186). A refresh lists every server again.
    pub fn with_tools(
        listed: Vec<(ServerConnection, Vec<RemoteTool>)>,
        reserved: BTreeSet<String>,
        store: Option<Arc<dyn ResultStore>>,
    ) -> Result<Self, GenerationError> {
        let servers = listed
            .iter()
            .map(|(connection, _)| connection.clone())
            .collect();
        let first = build(1, listed, &reserved, &store)?;
        Ok(Self {
            servers,
            reserved,
            store,
            current: Mutex::new(Arc::new(first)),
        })
    }

    fn slot(&self) -> MutexGuard<'_, Arc<Generation>> {
        self.current
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// The generation in use.
    pub fn current(&self) -> Arc<Generation> {
        self.slot().clone()
    }

    /// Discover again and swap the whole generation; on any failure the previous generation
    /// stays current and the error says why.
    pub async fn refresh(&self) -> Result<Arc<Generation>, GenerationError> {
        let number = self.current().number + 1;
        let listed = list(&self.servers).await?;
        let next = Arc::new(build(number, listed, &self.reserved, &self.store)?);
        *self.slot() = next.clone();
        Ok(next)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_checked_against_what_every_route_carries() {
        assert!(carriable(&tool_name("github", "search_issues")));
        assert!(!carriable(&tool_name("fs", "read.file")));
        assert!(!carriable(&tool_name("s", &"x".repeat(60))));
    }

    #[test]
    fn rendering_names_binary_data_instead_of_inlining_it() {
        let text = render(&[
            ContentBlock::Text { text: "a".into() },
            ContentBlock::Image {
                mime_type: "image/png".into(),
                data: "QUJD".into(),
            },
            ContentBlock::Resource {
                uri: "file:///n".into(),
                mime_type: None,
                text: Some("note".into()),
                blob: None,
            },
        ]);
        assert_eq!(
            text,
            "a\n[image: image/png, 4 bytes of base64 not shown]\n[resource file:///n]\nnote"
        );
    }
}
