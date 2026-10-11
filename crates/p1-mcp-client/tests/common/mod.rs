//! Starting the fake MCP server (`src/bin/p1-mcp-test-server.rs`) over stdio or over HTTP on
//! 127.0.0.1, and reading call results.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use p1_contracts::CancellationToken;
use p1_contracts::mcp::{ContentBlock, McpClient, McpServer, Transport};
use p1_mcp_client::NativeMcpClient;

pub const SERVER: &str = env!("CARGO_BIN_EXE_p1-mcp-test-server");

/// The fake server over stdio, with exactly `env` as its environment.
pub fn stdio(args: &[&str], env: &[(&str, &str)]) -> Transport {
    Transport::Stdio {
        command: SERVER.to_owned(),
        args: args.iter().map(|arg| arg.to_string()).collect(),
        env: env
            .iter()
            .map(|(name, value)| (name.to_string(), value.to_string()))
            .collect(),
        cwd: None,
    }
}

pub async fn connect(name: &str, transport: Transport) -> Box<dyn McpServer> {
    NativeMcpClient::new()
        .connect(name, transport, CancellationToken::new())
        .await
        .expect("the fake server connects")
}

/// The text of a call's content blocks.
pub async fn call_text(server: &dyn McpServer, tool: &str, args: serde_json::Value) -> String {
    let blocks = server
        .call(tool, args, CancellationToken::new())
        .await
        .expect("the call answers");
    blocks
        .iter()
        .map(|block| match block {
            ContentBlock::Text { text } => text.clone(),
            other => panic!("not text: {other:?}"),
        })
        .collect()
}

/// The fake server over HTTP; killed when dropped.
pub struct HttpServer {
    child: Child,
    pub url: String,
}

impl HttpServer {
    pub fn start(args: &[&str]) -> Self {
        let mut child = Command::new(SERVER)
            .arg("--http")
            .args(args)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("the fake HTTP server starts");
        let mut line = String::new();
        BufReader::new(child.stdout.take().expect("piped stdout"))
            .read_line(&mut line)
            .expect("the server prints its port");
        let port = line
            .trim()
            .strip_prefix("PORT ")
            .expect("a PORT line")
            .to_owned();
        Self {
            child,
            url: format!("http://127.0.0.1:{port}/mcp"),
        }
    }

    pub fn transport(&self, headers: &[(&str, &str)]) -> Transport {
        Transport::Http {
            url: self.url.clone(),
            headers: headers
                .iter()
                .map(|(name, value)| (name.to_string(), value.to_string()))
                .collect::<BTreeMap<_, _>>(),
        }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}
