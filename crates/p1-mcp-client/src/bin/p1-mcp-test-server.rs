//! A fake MCP server for this crate's tests, over stdio (default) or over HTTP on 127.0.0.1
//! (`--http`, which prints `PORT <n>` and serves until killed). No network beyond loopback,
//! no sleeps: every step answers the message that caused it.
//!
//! Tools: `echo` (read-only hint; answers its `text` argument), `env` (the names of its own
//! environment variables), `fail` (`isError`), `big` (`bytes` bytes of text), `wait` (never
//! answers; a later `cancelled` call lists the cancelled request ids), `ping_client` (stdio:
//! pings the client before answering), `break_listing` (every later `tools/list` fails),
//! `add_tool` (a later `tools/list` also lists `extra`), `headers` (HTTP: whether the request
//! carried the session id, the protocol version and an `authorization` header),
//! `process_group` (whether the server leads its own process group).
//! Flags: `--page-size <n>` splits `tools/list` into pages; `--tool <name>` adds a tool.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

const SESSION: &str = "fake-session-1";

struct State {
    page_size: usize,
    extra_tools: Vec<String>,
    listing_broken: bool,
    cancelled: Vec<Value>,
}

/// What one HTTP request carried that the `headers` tool reports.
#[derive(Default)]
struct Seen {
    session: bool,
    version: bool,
    authorization: bool,
}

impl State {
    fn from_args(args: &[String]) -> Self {
        let mut state = Self {
            page_size: usize::MAX,
            extra_tools: Vec::new(),
            listing_broken: false,
            cancelled: Vec::new(),
        };
        let mut args = args.iter();
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "--page-size" => {
                    state.page_size = args.next().and_then(|n| n.parse().ok()).unwrap_or(1);
                }
                "--tool" => state.extra_tools.extend(args.next().cloned()),
                _ => {}
            }
        }
        state
    }

    fn tools(&self) -> Vec<Value> {
        let schema = json!({"type": "object", "properties": {"text": {"type": "string"}}});
        let mut tools = vec![
            json!({"name": "echo", "description": "Echo text.", "inputSchema": schema,
                   "annotations": {"readOnlyHint": true}}),
            json!({"name": "env", "description": "List environment names.", "inputSchema": {"type": "object"}}),
            json!({"name": "fail", "description": "Always fails.", "inputSchema": {"type": "object"}}),
            json!({"name": "big", "description": "Large output.", "inputSchema": {"type": "object"}}),
            json!({"name": "wait", "description": "Never answers.", "inputSchema": {"type": "object"}}),
            json!({"name": "cancelled", "description": "Cancelled ids.", "inputSchema": {"type": "object"}}),
            json!({"name": "ping_client", "description": "Pings the client.", "inputSchema": {"type": "object"}}),
            json!({"name": "break_listing", "description": "Breaks tools/list.", "inputSchema": {"type": "object"}}),
            json!({"name": "add_tool", "description": "Adds `extra`.", "inputSchema": {"type": "object"}}),
            json!({"name": "headers", "description": "Reports headers.", "inputSchema": {"type": "object"}}),
        ];
        for name in &self.extra_tools {
            tools.push(
                json!({"name": name, "description": "Extra.", "inputSchema": {"type": "object"}}),
            );
        }
        tools
    }

    /// The answer to one message, `None` for a notification or a call that never answers.
    fn handle(&mut self, message: &Value, seen: &Seen) -> Option<Value> {
        let method = message.get("method")?.as_str()?;
        let Some(id) = message.get("id").cloned() else {
            if method == "notifications/cancelled" {
                self.cancelled.push(
                    message
                        .pointer("/params/requestId")
                        .cloned()
                        .unwrap_or(Value::Null),
                );
            }
            return None;
        };
        let result = match method {
            "initialize" => json!({
                "protocolVersion": message.pointer("/params/protocolVersion").cloned().unwrap_or(json!("2025-06-18")),
                "capabilities": {"tools": {"listChanged": true}},
                "serverInfo": {"name": "p1-mcp-test-server", "version": "0"},
            }),
            "tools/list" if self.listing_broken => {
                return Some(json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32603, "message": "listing is broken"}}));
            }
            "tools/list" => {
                let tools = self.tools();
                let start: usize = message
                    .pointer("/params/cursor")
                    .and_then(Value::as_str)
                    .and_then(|cursor| cursor.parse().ok())
                    .unwrap_or(0);
                let end = start.saturating_add(self.page_size).min(tools.len());
                let mut page = json!({"tools": tools[start..end]});
                if end < tools.len() {
                    page["nextCursor"] = json!(end.to_string());
                }
                page
            }
            "tools/call" => {
                let name = message
                    .pointer("/params/name")
                    .and_then(Value::as_str)
                    .unwrap_or("");
                let args = message
                    .pointer("/params/arguments")
                    .cloned()
                    .unwrap_or(json!({}));
                match name {
                    "echo" => text(args.get("text").and_then(Value::as_str).unwrap_or("")),
                    "env" => {
                        let mut names: Vec<String> = std::env::vars_os()
                            .map(|(name, _)| name.to_string_lossy().into_owned())
                            .collect();
                        names.sort();
                        text(&names.join(","))
                    }
                    "fail" => {
                        json!({"content": [{"type": "text", "text": "it failed"}], "isError": true})
                    }
                    "big" => {
                        let bytes = args.get("bytes").and_then(Value::as_u64).unwrap_or(0) as usize;
                        let body: String = (0..bytes)
                            .map(|i| if i % 100 == 99 { '\n' } else { 'x' })
                            .collect();
                        text(&body)
                    }
                    "wait" => return None,
                    "process_group" => {
                        // Linux: field 5 of /proc/self/stat is the process group.
                        let stat = std::fs::read_to_string("/proc/self/stat").unwrap_or_default();
                        let fields: Vec<&str> = stat
                            .rsplit_once(')')
                            .map(|(_, rest)| rest.split_whitespace().collect())
                            .unwrap_or_default();
                        let leader = fields
                            .get(2)
                            .is_some_and(|group| *group == std::process::id().to_string());
                        text(&format!("group leader: {leader}"))
                    }
                    "cancelled" => text(&Value::Array(self.cancelled.clone()).to_string()),
                    "break_listing" => {
                        self.listing_broken = true;
                        text("broken")
                    }
                    "add_tool" => {
                        self.extra_tools.push("extra".into());
                        text("added")
                    }
                    "headers" => text(&format!(
                        "session={} version={} authorization={}",
                        seen.session, seen.version, seen.authorization
                    )),
                    _ => {
                        return Some(json!({"jsonrpc": "2.0", "id": id,
                            "error": {"code": -32602, "message": "unknown tool"}}));
                    }
                }
            }
            "ping" => json!({}),
            _ => {
                return Some(json!({"jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": "method not found"}}));
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

fn text(text: &str) -> Value {
    json!({"content": [{"type": "text", "text": text}]})
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let state = State::from_args(&args);
    if args.iter().any(|arg| arg == "--http") {
        serve_http(state);
    } else {
        serve_stdio(state);
    }
}

fn serve_stdio(mut state: State) {
    let stdin = std::io::stdin();
    let mut lines = stdin.lock().lines();
    let mut stdout = std::io::stdout();
    let seen = Seen::default();
    while let Some(Ok(line)) = lines.next() {
        let Ok(message) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if message.pointer("/params/name").and_then(Value::as_str) == Some("ping_client") {
            // Ask the client first; answer the call only once the client answered.
            writeln!(
                stdout,
                "{}",
                json!({"jsonrpc": "2.0", "id": "server-ping", "method": "ping"})
            )
            .unwrap();
            writeln!(
                stdout,
                "{}",
                json!({"jsonrpc": "2.0", "method": "notifications/message",
                "params": {"level": "info", "data": "noise"}})
            )
            .unwrap();
            stdout.flush().unwrap();
            let answered = lines
                .next()
                .and_then(Result::ok)
                .and_then(|line| serde_json::from_str::<Value>(&line).ok())
                .is_some_and(|answer| {
                    answer["id"] == "server-ping" && answer["result"] == json!({})
                });
            let id = message["id"].clone();
            let reply = json!({"jsonrpc": "2.0", "id": id, "result": text(&format!("client answered ping: {answered}"))});
            writeln!(stdout, "{reply}").unwrap();
            stdout.flush().unwrap();
            continue;
        }
        if let Some(answer) = state.handle(&message, &seen) {
            writeln!(stdout, "{answer}").unwrap();
            stdout.flush().unwrap();
        }
    }
}

fn serve_http(state: State) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    println!("PORT {}", listener.local_addr().unwrap().port());
    std::io::stdout().flush().unwrap();
    let state = Arc::new(Mutex::new(state));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let state = state.clone();
        std::thread::spawn(move || serve_connection(stream, &state));
    }
}

fn serve_connection(stream: TcpStream, state: &Mutex<State>) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut length = 0usize;
        let mut seen = Seen::default();
        let mut session_ok = false;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            let value = value.trim();
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.parse().unwrap_or(0),
                "mcp-session-id" => {
                    seen.session = true;
                    session_ok = value == SESSION;
                }
                "mcp-protocol-version" => seen.version = true,
                "authorization" => seen.authorization = true,
                _ => {}
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let Ok(message) = serde_json::from_slice::<Value>(&body) else {
            respond(&mut writer, "400 Bad Request", &[], "")?;
            continue;
        };
        let method = message.get("method").and_then(Value::as_str).unwrap_or("");
        if method != "initialize" && !session_ok {
            respond(&mut writer, "400 Bad Request", &[], "")?;
            continue;
        }
        let answer = state.lock().unwrap().handle(&message, &seen);
        match answer {
            None => respond(&mut writer, "202 Accepted", &[], "")?,
            Some(answer) if method == "initialize" => respond(
                &mut writer,
                "200 OK",
                &[
                    ("Content-Type", "application/json"),
                    ("Mcp-Session-Id", SESSION),
                ],
                &answer.to_string(),
            )?,
            Some(answer) if method == "tools/list" => {
                // A stream: a notification first, then the answer, split across events.
                let note = json!({"jsonrpc": "2.0", "method": "notifications/message",
                    "params": {"level": "info", "data": "listing"}});
                let body = format!(
                    ": comment\n\nevent: message\ndata: {note}\n\nevent: message\ndata: {answer}\n\n"
                );
                respond(
                    &mut writer,
                    "200 OK",
                    &[("Content-Type", "text/event-stream")],
                    &body,
                )?;
            }
            Some(answer) => respond(
                &mut writer,
                "200 OK",
                &[("Content-Type", "application/json")],
                &answer.to_string(),
            )?,
        }
    }
}

fn respond(
    writer: &mut TcpStream,
    status: &str,
    headers: &[(&str, &str)],
    body: &str,
) -> std::io::Result<()> {
    let mut head = format!("HTTP/1.1 {status}\r\nContent-Length: {}\r\n", body.len());
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    head.push_str("\r\n");
    writer.write_all(head.as_bytes())?;
    writer.write_all(body.as_bytes())?;
    writer.flush()
}
