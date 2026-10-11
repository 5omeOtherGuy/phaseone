//! ADR-0181 end to end: an environment's `[[mcp_servers]]` entry is connected at session start,
//! its tool is presented as `mcp__<server>__<raw>`, a call reaches the server through the whole
//! host, the result is masked before the model sees it, and the journal names the generated
//! tool's identity. The server is a few lines of bash over stdio; the provider is scripted. No
//! network, no real credential: the token is a fake key built at run time.

mod common;

use std::ffi::OsString;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{DeclarationKind, Item};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::tempdir;

/// A fake key of the `sk-` shape, built at run time: no credential-shaped literal.
fn fake_key() -> String {
    format!("sk-{}", "Pq7wEr2tYu".repeat(3))
}

/// A stdio MCP server in bash: it answers `initialize`, lists one tool and answers every call
/// with the variables it was given. Every request id the client sends is numeric.
const SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"sh","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"hello","description":"Say hello.","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hello token=%s canary=%s"}]}}\n' "$id" "$SH_TOKEN" "${CANARY_TOKEN:-absent}" ;;
  esac
done
"#;

#[tokio::test]
async fn a_configured_server_is_connected_called_masked_and_journalled() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    let scratch = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &[],
        "test",
    );
    let script = scratch.path().join("server.sh");
    std::fs::write(&script, SERVER).unwrap();
    let file = environments.path().join("plain/environment.toml");
    let mut toml = std::fs::read_to_string(&file).unwrap();
    toml.push_str(&format!(
        "[[mcp_servers]]\nname = \"sh\"\ncommand = \"bash\"\nargs = [{:?}]\nenv_pass = [\"SH_TOKEN\"]\n",
        script.to_str().unwrap()
    ));
    std::fs::write(&file, toml).unwrap();

    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("c1", "mcp__sh__hello", "{}")]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let key = fake_key();
    harness.deps.shell_env = Some(vec![
        (
            OsString::from("PATH"),
            std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        ),
        (OsString::from("SH_TOKEN"), OsString::from(&key)),
        (
            OsString::from("CANARY_TOKEN"),
            OsString::from("canary-value"),
        ),
    ]);
    let session = scratch.path().join("session.jsonl");
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());

    let requests = handle.requests();
    let declaration = requests[0]
        .tools
        .iter()
        .find(|tool| tool.name == "mcp__sh__hello")
        .expect("the generated tool is declared to the model");
    assert_eq!(declaration.description, "Say hello.");
    assert!(matches!(declaration.kind, DeclarationKind::Function { .. }));

    let result = requests
        .last()
        .unwrap()
        .history
        .iter()
        .find_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .expect("the call has a result");
    assert!(result.starts_with("hello token="), "{result}");
    assert!(
        !result.contains(&key),
        "the passed token is masked: {result}"
    );
    assert!(
        result.ends_with("canary=absent"),
        "a variable not passed stays out: {result}"
    );

    let journal = std::fs::read_to_string(&session).unwrap();
    assert!(!journal.contains(&key), "the journal never holds the token");
    let environment = journal
        .lines()
        .find(|line| line.contains(r#""record":"environment""#))
        .expect("an Environment record");
    assert!(
        environment.contains(r#""implementation":"mcp:sh""#)
            && environment.contains(r#""name":"mcp__sh__hello""#),
        "the generation is journalled: {environment}"
    );
    assert!(
        journal
            .lines()
            .any(|line| line.contains(r#""record":"tool_started""#)
                && line.contains(r#""implementation":"mcp:sh""#)
                && line.contains(r#""variant":"hello""#)),
        "the call journals the server's identity"
    );
}

#[tokio::test]
async fn an_invalid_mcp_servers_entry_fails_before_the_session_starts() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "fake",
        "fake-model",
        &[],
        "test",
    );
    let file = environments.path().join("plain/environment.toml");
    let mut toml = std::fs::read_to_string(&file).unwrap();
    toml.push_str("[[mcp_servers]]\nname = \"Bad Name\"\ncommand = \"bash\"\n");
    std::fs::write(&file, toml).unwrap();

    let provider = ScriptedProvider::new(vec![text_response("never sent")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "go",
        ],
    )
    .await;
    assert_ne!(code, 0);
    assert!(
        harness.stderr.text().contains("[[mcp_servers]] `Bad Name`"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(handle.requests().is_empty(), "no request was sent");
}
