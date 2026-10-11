//! ADR-0181 end to end: an environment's `[[mcp_servers]]` entry is connected at session start,
//! its tool is presented as `mcp__<server>__<raw>`, a call reaches the server through the whole
//! host, the result is masked before the model sees it, and the journal names the generated
//! tool's identity. The server is a few lines of bash over stdio; the provider is scripted. No
//! network, no real credential: the token is a fake key built at run time.

mod common;

use std::ffi::OsString;

use common::{Harness, provider_hook, run_args, write_environment};
use p1_contracts::{DeclarationKind, Item, RecordBody};
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
        &["read"],
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
        tool_call_response(vec![
            json_call("c1", "mcp__sh__hello", "{}"),
            json_call("c2", "read", r#"{"file_path":"token.txt"}"#),
        ]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let key = fake_key();
    std::fs::write(workspace.path().join("token.txt"), format!("token {key}\n")).unwrap();
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

    let results: Vec<String> = requests
        .last()
        .unwrap()
        .history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.content.clone()),
            _ => None,
        })
        .collect();
    let result = results[0].clone();
    // ADR-0184: a credential passed by name from p1's own environment stays session-wide:
    // p1's shell and file tools can see that environment, so `read` masks it too.
    assert!(
        results[1].contains("token <redacted:") && !results[1].contains(&key),
        "{}",
        results[1]
    );
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

    // Masking is never silent: the turn's notice names the count and each tool, never a
    // value, and each call's record carries its own count.
    let stderr = harness.stderr.text();
    assert!(
        stderr.contains(
            "masked 2 possible credential value(s) in tool output: mcp__sh__hello 1, read 1"
        ),
        "stderr: {stderr}"
    );
    assert!(!stderr.contains(&key));
    let loaded = p1_journal::load(&session).unwrap();
    let counts: Vec<u64> = loaded
        .records
        .iter()
        .zip(&loaded.masked)
        .filter(|(record, _)| matches!(record.body, RecordBody::ToolFinished { .. }))
        .map(|(_, masked)| *masked)
        .collect();
    assert_eq!(counts, [1, 1]);
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

/// A 127.0.0.1 URL nothing listens on: the port was bound, then released.
fn closed_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{address}/mcp")
}

/// ADR-0186: a `required = false` server that fails to start (a command that does not exist,
/// a closed port) is skipped: the session starts with the other servers' tools, the operator
/// sees a notice naming each skipped server, and the session's assembly line records it. No
/// value the server was given reaches the notice or the record.
#[tokio::test]
async fn an_optional_server_that_fails_is_skipped_with_a_notice_and_a_record() {
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
    let missing = scratch.path().join("no-such-mcp-server");
    let file = environments.path().join("plain/environment.toml");
    let mut toml = std::fs::read_to_string(&file).unwrap();
    toml.push_str(&format!(
        "[[mcp_servers]]\nname = \"sh\"\ncommand = \"bash\"\nargs = [{:?}]\nenv_pass = [\"SH_TOKEN\"]\n\
         [[mcp_servers]]\nname = \"gone\"\nrequired = false\ncommand = {:?}\nenv_pass = [\"SH_TOKEN\"]\n\
         [[mcp_servers]]\nname = \"web\"\nrequired = false\nurl = {:?}\nbearer_token_env = \"SH_TOKEN\"\n",
        script.to_str().unwrap(),
        missing.to_str().unwrap(),
        closed_url(),
    ));
    std::fs::write(&file, toml).unwrap();

    let provider = ScriptedProvider::new(vec![text_response("done")]);
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
    let stderr = harness.stderr.text();
    assert_eq!(code, 0, "stderr: {stderr}");

    let declared: Vec<String> = handle.requests()[0]
        .tools
        .iter()
        .map(|tool| tool.name.clone())
        .collect();
    assert!(
        declared.contains(&"mcp__sh__hello".to_owned()),
        "{declared:?}"
    );
    assert!(
        !declared
            .iter()
            .any(|name| name.starts_with("mcp__gone__") || name.starts_with("mcp__web__")),
        "{declared:?}"
    );
    for server in ["gone", "web"] {
        assert!(
            stderr.contains(&format!(
                "· MCP server `{server}` skipped (optional), the session runs without its tools: \
                 could not connect: "
            )),
            "stderr: {stderr}"
        );
    }
    assert!(!stderr.contains(&key), "stderr: {stderr}");

    let journal = std::fs::read_to_string(&session).unwrap();
    assert!(!journal.contains(&key), "the journal never holds the token");
    let loaded = p1_journal::load(&session).unwrap();
    let skipped: Vec<&str> = loaded.assemblies[0]
        .identity
        .mcp_skipped
        .iter()
        .map(|server| server.name.as_str())
        .collect();
    assert_eq!(skipped, ["gone", "web"]);
    for server in &loaded.assemblies[0].identity.mcp_skipped {
        assert!(
            server.reason.starts_with("could not connect: "),
            "{server:?}"
        );
    }
}

/// ADR-0181 stands for a server the environment does not mark optional: one that does not
/// start fails the session start, naming it, before any request.
#[tokio::test]
async fn a_required_server_that_fails_fails_the_session_start() {
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
    let file = environments.path().join("plain/environment.toml");
    let mut toml = std::fs::read_to_string(&file).unwrap();
    toml.push_str(&format!(
        "[[mcp_servers]]\nname = \"gone\"\ncommand = {:?}\n",
        scratch.path().join("no-such-mcp-server").to_str().unwrap()
    ));
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
    let stderr = harness.stderr.text();
    assert!(stderr.contains("MCP server `gone`: "), "stderr: {stderr}");
    assert!(!stderr.contains("skipped"), "stderr: {stderr}");
    assert!(handle.requests().is_empty(), "no request was sent");
}
