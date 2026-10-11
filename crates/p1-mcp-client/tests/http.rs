//! Streamable HTTP against the fake server on 127.0.0.1: the session id, the protocol version
//! header, configured headers, JSON and SSE answers.

mod common;

use common::{HttpServer, call_text, connect};
use p1_contracts::CancellationToken;
use p1_contracts::mcp::{McpClient, McpError, Transport};
use p1_mcp_client::NativeMcpClient;
use serde_json::json;

#[tokio::test]
async fn discovery_over_an_sse_answer_skips_the_servers_notifications() {
    let fake = HttpServer::start(&[]);
    let server = connect("web", fake.transport(&[])).await;
    let tools = server.tools().await.unwrap();
    assert_eq!(tools.len(), 10);
    assert!(tools[0].read_only);
}

#[tokio::test]
async fn a_call_carries_the_session_the_version_and_the_configured_headers() {
    let fake = HttpServer::start(&[]);
    let server = connect(
        "web",
        fake.transport(&[("Authorization", "Bearer placeholder")]),
    )
    .await;
    assert_eq!(
        call_text(server.as_ref(), "echo", json!({"text": "over http"})).await,
        "over http"
    );
    assert_eq!(
        call_text(server.as_ref(), "headers", json!({})).await,
        "session=true version=true authorization=true"
    );
}

#[tokio::test]
async fn is_error_over_http_is_a_tool_failure() {
    let fake = HttpServer::start(&[]);
    let server = connect("web", fake.transport(&[])).await;
    let error = server
        .call("fail", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::ToolFailed { .. }));
}

#[tokio::test]
async fn a_url_that_is_not_http_is_refused_before_any_connection() {
    let result = NativeMcpClient::new()
        .connect(
            "web",
            Transport::Http {
                url: "file:///etc/passwd".into(),
                headers: Default::default(),
            },
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(McpError::Connect(_))));
}
