//! The stdio transport against the fake server: discovery, calls, the child-environment
//! scrub, server requests and cancellation.

mod common;

use common::{call_text, connect, stdio};
use p1_contracts::CancellationToken;
use p1_contracts::mcp::{McpClient, McpError};
use p1_mcp_client::NativeMcpClient;
use serde_json::json;

#[tokio::test]
async fn discovery_follows_every_page_and_reads_the_read_only_hint() {
    let server = connect("fake", stdio(&["--page-size", "3"], &[])).await;
    let tools = server.tools().await.unwrap();
    let names: Vec<&str> = tools.iter().map(|tool| tool.name.as_str()).collect();
    assert_eq!(
        names,
        [
            "echo",
            "env",
            "fail",
            "big",
            "wait",
            "cancelled",
            "ping_client",
            "break_listing",
            "add_tool",
            "headers"
        ]
    );
    assert!(tools[0].read_only, "echo carries readOnlyHint");
    assert!(!tools[1].read_only, "env carries no hint");
    assert_eq!(tools[0].description, "Echo text.");
    assert_eq!(tools[0].input_schema["type"], "object");
}

#[tokio::test]
async fn a_call_answers_with_the_tools_content() {
    let server = connect("fake", stdio(&[], &[])).await;
    assert_eq!(
        call_text(server.as_ref(), "echo", json!({"text": "hello"})).await,
        "hello"
    );
}

#[tokio::test]
async fn the_child_gets_only_the_environment_the_transport_names() {
    // This test process holds PATH, HOME, CARGO_* and whatever else the runner exported; the
    // child must see none of it, only the one variable the transport names.
    assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
    let server = connect("fake", stdio(&[], &[("P1_MCP_ONLY", "1")])).await;
    assert_eq!(
        call_text(server.as_ref(), "env", json!({})).await,
        "P1_MCP_ONLY"
    );
}

#[tokio::test]
async fn the_child_leads_its_own_process_group() {
    // A Ctrl-C reaches p1's foreground process group; the server must not be in it.
    let server = connect("fake", stdio(&[], &[])).await;
    assert_eq!(
        call_text(server.as_ref(), "process_group", json!({})).await,
        "group leader: true"
    );
}

#[tokio::test]
async fn is_error_is_a_tool_failure_with_the_tools_content() {
    let server = connect("fake", stdio(&[], &[])).await;
    let error = server
        .call("fail", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::ToolFailed { content } if content.len() == 1));
}

#[tokio::test]
async fn an_unknown_tool_is_a_server_error() {
    let server = connect("fake", stdio(&[], &[])).await;
    let error = server
        .call("nope", json!({}), CancellationToken::new())
        .await
        .unwrap_err();
    assert!(matches!(error, McpError::Server { code: -32602, .. }));
}

#[tokio::test]
async fn the_client_answers_a_server_ping_and_skips_its_notifications() {
    let server = connect("fake", stdio(&[], &[])).await;
    assert_eq!(
        call_text(server.as_ref(), "ping_client", json!({})).await,
        "client answered ping: true"
    );
}

#[tokio::test]
async fn cancelling_a_call_returns_cancelled_and_tells_the_server() {
    let server = connect("fake", stdio(&[], &[])).await;
    let cancel = CancellationToken::new();
    cancel.cancel();
    // `wait` never answers: only the cancellation ends the call.
    let error = server.call("wait", json!({}), cancel).await.unwrap_err();
    assert_eq!(error, McpError::Cancelled);
    // stdin is ordered, so the server read the cancellation before this call. Request 1 was
    // `initialize`, request 2 the cancelled `wait`.
    assert_eq!(
        call_text(server.as_ref(), "cancelled", json!({})).await,
        "[2]"
    );
}

#[tokio::test]
async fn a_command_that_cannot_start_is_a_connect_error() {
    let result = NativeMcpClient::new()
        .connect(
            "missing",
            p1_contracts::mcp::Transport::Stdio {
                command: "/nonexistent/p1-mcp-server".into(),
                args: Vec::new(),
                env: Default::default(),
                cwd: None,
            },
            CancellationToken::new(),
        )
        .await;
    assert!(matches!(result, Err(McpError::Connect(_))));
}

#[tokio::test]
async fn resources_are_a_server_error_when_the_server_serves_none() {
    let server = connect("fake", stdio(&[], &[])).await;
    assert!(matches!(
        server.resources(None).await,
        Err(McpError::Server { code: -32601, .. })
    ));
}
