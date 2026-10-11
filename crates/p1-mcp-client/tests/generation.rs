//! Tool generations over the fake server: names, identities, effects, whole swaps, clash
//! refusal, a failed refresh keeping the previous generation, and bounded results.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use common::{HttpServer, connect, stdio};
use p1_contracts::history::{ToolCall, ToolStatus};
use p1_contracts::{CancellationToken, Effect, Tool, ToolContext, ToolIdentity, ToolInput};
use p1_mcp_client::generation::MAX_RESULT_BYTES;
use p1_mcp_client::{GenerationError, ResultStore, ServerConnection, ToolGenerations};

async fn server(name: &str, args: &[&str]) -> ServerConnection {
    ServerConnection {
        name: name.to_owned(),
        server: Arc::from(connect(name, stdio(args, &[])).await),
    }
}

fn call(name: &str, input: &str) -> ToolCall {
    ToolCall {
        call_id: "c1".into(),
        name: name.into(),
        input: ToolInput::Json(input.into()),
    }
}

fn find(tools: &[Arc<dyn Tool>], name: &str) -> Arc<dyn Tool> {
    tools
        .iter()
        .find(|tool| tool.declaration().name == name)
        .unwrap_or_else(|| panic!("no tool {name}"))
        .clone()
}

async fn run(tool: &dyn Tool, input: &str) -> p1_contracts::ToolOutcome {
    tool.execute(
        &call(&tool.declaration().name, input),
        ToolContext {
            cancel: CancellationToken::new(),
        },
    )
    .await
}

#[tokio::test]
async fn every_served_tool_becomes_one_named_tool_with_the_servers_identity() {
    let generations = ToolGenerations::discover(
        vec![server("alpha", &[]).await, server("beta", &[]).await],
        BTreeSet::new(),
        None,
    )
    .await
    .unwrap();
    let generation = generations.current();
    assert_eq!(generation.number(), 1);
    let tools = generation.tools();
    assert_eq!(tools.len(), 20);
    assert_eq!(tools[0].declaration().name, "mcp__alpha__echo");
    assert_eq!(tools[10].declaration().name, "mcp__beta__echo");
    assert_eq!(
        tools[10].identity(),
        &ToolIdentity {
            implementation: "mcp:beta".into(),
            variant: "echo".into()
        }
    );
    assert_eq!(tools[0].declaration().description, "Echo text.");

    let echo = find(&tools, "mcp__alpha__echo");
    let outcome = run(echo.as_ref(), r#"{"text": "through the tool"}"#).await;
    assert_eq!(outcome.status, ToolStatus::Ok);
    assert_eq!(outcome.content, "through the tool");
}

#[tokio::test]
async fn the_read_only_hint_decides_the_effect() {
    let generations =
        ToolGenerations::discover(vec![server("s", &[]).await], BTreeSet::new(), None)
            .await
            .unwrap();
    let tools = generations.current().tools();
    let echo = find(&tools, "mcp__s__echo");
    let env = find(&tools, "mcp__s__env");
    assert_eq!(echo.effect(&call("mcp__s__echo", "{}")), Effect::ReadOnly);
    assert_eq!(env.effect(&call("mcp__s__env", "{}")), Effect::Executes);
}

#[tokio::test]
async fn a_clash_with_the_assembled_tools_fails_the_generation() {
    let reserved = BTreeSet::from(["mcp__s__echo".to_owned()]);
    let result = ToolGenerations::discover(vec![server("s", &[]).await], reserved, None).await;
    assert_eq!(
        result.err(),
        Some(GenerationError::Clash {
            name: "mcp__s__echo".into()
        })
    );
}

#[tokio::test]
async fn a_clash_between_two_servers_fails_the_generation() {
    // `a` serving `b__echo` and `a__b` serving `echo` would both be `mcp__a__b__echo`.
    let result = ToolGenerations::discover(
        vec![
            server("a", &["--tool", "b__echo"]).await,
            server("a__b", &[]).await,
        ],
        BTreeSet::new(),
        None,
    )
    .await;
    assert_eq!(
        result.err(),
        Some(GenerationError::Clash {
            name: "mcp__a__b__echo".into()
        })
    );
}

#[tokio::test]
async fn a_name_no_route_can_carry_fails_the_generation() {
    let result = ToolGenerations::discover(
        vec![server("s", &["--tool", "dotted.name"]).await],
        BTreeSet::new(),
        None,
    )
    .await;
    assert!(matches!(
        result.err(),
        Some(GenerationError::InvalidName { raw, .. }) if raw == "dotted.name"
    ));
}

#[tokio::test]
async fn a_failed_refresh_keeps_the_previous_generation() {
    let generations =
        ToolGenerations::discover(vec![server("s", &[]).await], BTreeSet::new(), None)
            .await
            .unwrap();
    let before = generations.current();
    let broken = run(
        find(&before.tools(), "mcp__s__break_listing").as_ref(),
        "{}",
    )
    .await;
    assert_eq!(broken.content, "broken");

    let error = generations.refresh().await.unwrap_err();
    assert!(matches!(error, GenerationError::Discovery { ref server, .. } if server == "s"));
    let after = generations.current();
    assert!(
        Arc::ptr_eq(&before, &after),
        "the previous generation stays"
    );
    assert_eq!(after.number(), 1);
}

#[tokio::test]
async fn a_refresh_swaps_the_whole_generation() {
    let generations =
        ToolGenerations::discover(vec![server("s", &[]).await], BTreeSet::new(), None)
            .await
            .unwrap();
    let first = generations.current();
    run(find(&first.tools(), "mcp__s__add_tool").as_ref(), "{}").await;

    let second = generations.refresh().await.unwrap();
    assert_eq!(second.number(), 2);
    assert!(Arc::ptr_eq(&second, &generations.current()));
    assert_eq!(second.tools().len(), first.tools().len() + 1);
    find(&second.tools(), "mcp__s__extra");
    // The old generation is untouched: an agent still holding it keeps its tools.
    assert!(
        first
            .tools()
            .iter()
            .all(|tool| tool.declaration().name != "mcp__s__extra")
    );
}

#[derive(Default)]
struct RecordingStore {
    stored: Mutex<Vec<usize>>,
}

impl ResultStore for RecordingStore {
    fn mask(&self, server: &str, _call: &ToolCall, text: String) -> String {
        assert_eq!(server, "s", "the store is told whose result it masks");
        text
    }

    fn store(&self, server: &str, text: &str) -> String {
        assert_eq!(server, "s");
        self.stored.lock().unwrap().push(text.len());
        "[stored output: handle_id out-test]".into()
    }
}

#[tokio::test]
async fn a_long_result_is_cut_to_head_and_tail_and_stored_whole() {
    let store = Arc::new(RecordingStore::default());
    let generations = ToolGenerations::discover(
        vec![server("s", &[]).await],
        BTreeSet::new(),
        Some(store.clone()),
    )
    .await
    .unwrap();
    let big = find(&generations.current().tools(), "mcp__s__big");

    let short = run(big.as_ref(), r#"{"bytes": 1000}"#).await;
    assert_eq!(short.content.len(), 1000);
    assert!(
        store.stored.lock().unwrap().is_empty(),
        "a short result is not stored"
    );

    let long = run(big.as_ref(), r#"{"bytes": 120000}"#).await;
    assert_eq!(long.status, ToolStatus::Ok);
    assert!(long.content.len() < MAX_RESULT_BYTES + 200);
    assert!(long.content.contains("[… 70000 bytes omitted …]"));
    assert!(
        long.content
            .ends_with("\n[stored output: handle_id out-test]")
    );
    assert_eq!(*store.stored.lock().unwrap(), [120000]);
}

#[tokio::test]
async fn failures_cancellation_and_bad_arguments_are_tool_outcomes() {
    let generations =
        ToolGenerations::discover(vec![server("s", &[]).await], BTreeSet::new(), None)
            .await
            .unwrap();
    let tools = generations.current().tools();

    let failed = run(find(&tools, "mcp__s__fail").as_ref(), "{}").await;
    assert_eq!(failed.status, ToolStatus::Error);
    assert_eq!(failed.content, "it failed");

    let not_object = run(find(&tools, "mcp__s__echo").as_ref(), "[1]").await;
    assert_eq!(not_object.status, ToolStatus::Error);

    let wait = find(&tools, "mcp__s__wait");
    let cancel = CancellationToken::new();
    cancel.cancel();
    let cancelled = wait
        .execute(&call("mcp__s__wait", "{}"), ToolContext { cancel })
        .await;
    assert_eq!(cancelled.status, ToolStatus::Cancelled);
}

#[tokio::test]
async fn an_http_server_generates_tools_like_a_stdio_one() {
    let fake = HttpServer::start(&[]);
    let connection = ServerConnection {
        name: "web".into(),
        server: Arc::from(connect("web", fake.transport(&[])).await),
    };
    let generations = ToolGenerations::discover(vec![connection], BTreeSet::new(), None)
        .await
        .unwrap();
    let tools = generations.current().tools();
    let echo = find(&tools, "mcp__web__echo");
    assert_eq!(echo.identity().implementation, "mcp:web");
    assert_eq!(echo.effect(&call("mcp__web__echo", "{}")), Effect::ReadOnly);
    assert_eq!(run(echo.as_ref(), r#"{"text": "hi"}"#).await.content, "hi");
}
