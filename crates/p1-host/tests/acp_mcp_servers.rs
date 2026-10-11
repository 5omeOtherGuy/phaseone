//! #695 end to end: the `mcpServers` of an ACP `session/new` become that session's MCP
//! servers. The real host runs behind the ACP adapter over an in-memory pipe, on a scripted
//! provider: a stdio server (a few lines of bash) and a streamable HTTP server (a thread on
//! 127.0.0.1) named by the client serve tools the model calls; the values the client sends
//! are masked everywhere and journalled by name only; a name an environment server already
//! uses fails `session/new` with an error naming the server. No network beyond loopback, no
//! real credential (fake keys built at run time), no sleeps.

mod common;

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use common::{Harness, provider_hook, write_environment};
use p1_contracts::{CancellationToken, Item};
use p1_host::frontend::FrontEnd;
use p1_host::frontend_port::PortFrontEnd;
use p1_host::run::{RunError, run_with_front_end};
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use serde_json::{Value, json};
use tempfile::tempdir;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

/// A value the client sends, built at run time. It has no credential shape, so only the
/// host's registering it as a secret masks it.
fn fake_key(seed: &str) -> String {
    format!("client-value-{seed}")
}

/// A stdio MCP server in bash: one read-only tool, `hello`, which answers with the variables
/// it was given. Every request id the client sends is numeric.
const SERVER: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  case "$line" in
    *'"method":"initialize"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"protocolVersion":"2025-06-18","capabilities":{},"serverInfo":{"name":"sh","version":"0"}}}\n' "$id" ;;
    *'"method":"tools/list"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"tools":[{"name":"hello","description":"Say hello.","inputSchema":{"type":"object"},"annotations":{"readOnlyHint":true}}]}}\n' "$id" ;;
    *'"method":"tools/call"'*)
      printf '{"jsonrpc":"2.0","id":%s,"result":{"content":[{"type":"text","text":"hello token=%s canary=%s path=%s dir=%s"}]}}\n' "$id" "$SH_TOKEN" "${CANARY_TOKEN:-absent}" "${PATH:+set}" "$(pwd)" ;;
  esac
done
"#;

/// A streamable HTTP MCP server on 127.0.0.1: one read-only tool, `fetch`, which answers
/// whether the request carried `authorization` and echoes its bare token, so the test sees
/// the token reached the server and was masked on the way back. Serves until the test ends.
fn http_server(expected: String) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://{}/mcp", listener.local_addr().unwrap());
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(stream) = stream else { continue };
            let expected = expected.clone();
            std::thread::spawn(move || serve_connection(stream, &expected));
        }
    });
    url
}

fn serve_connection(stream: TcpStream, expected: &str) -> std::io::Result<()> {
    let mut reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;
    loop {
        let mut request_line = String::new();
        if reader.read_line(&mut request_line)? == 0 {
            return Ok(());
        }
        let mut length = 0;
        let mut token = None;
        loop {
            let mut line = String::new();
            reader.read_line(&mut line)?;
            let line = line.trim_end();
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':').unwrap_or((line, ""));
            match name.to_ascii_lowercase().as_str() {
                "content-length" => length = value.trim().parse().unwrap_or(0),
                "authorization" => token = Some(value.trim().to_string()),
                _ => {}
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body)?;
        let message: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
        let id = message["id"].clone();
        let result = match message["method"].as_str().unwrap_or("") {
            "initialize" => json!({"protocolVersion": "2025-06-18", "capabilities": {},
                "serverInfo": {"name": "web", "version": "0"}}),
            "tools/list" => json!({"tools": [{"name": "fetch", "description": "Fetch.",
                "inputSchema": {"type": "object"}, "annotations": {"readOnlyHint": true}}]}),
            "tools/call" => {
                let authorized = token.as_deref() == Some(expected);
                let bare = token
                    .as_deref()
                    .and_then(|token| token.strip_prefix("Bearer "))
                    .unwrap_or("none");
                json!({"content": [{"type": "text",
                    "text": format!("web authorized={authorized} token={bare}")}]})
            }
            _ => {
                // A notification (`notifications/initialized`): accepted, no body.
                write!(writer, "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\n\r\n")?;
                writer.flush()?;
                continue;
            }
        };
        let answer = json!({"jsonrpc": "2.0", "id": id, "result": result}).to_string();
        write!(
            writer,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nMcp-Session-Id: s1\r\nContent-Length: {}\r\n\r\n{answer}",
            answer.len()
        )?;
        writer.flush()?;
    }
}

fn shell_env() -> Vec<(OsString, OsString)> {
    vec![
        (
            OsString::from("PATH"),
            std::env::var_os("PATH").unwrap_or_else(|| OsString::from("/usr/bin:/bin")),
        ),
        (
            OsString::from("CANARY_TOKEN"),
            OsString::from("canary-value"),
        ),
    ]
}

fn options(workspace: &Path, journal: &Path) -> p1_host::cli::Options {
    let mut options = p1_host::cli::parse(&[
        "acp".to_string(),
        p1_host::cli::SERVE_SESSION.to_string(),
        "--env".to_string(),
        "plain".to_string(),
        "--workspace".to_string(),
        workspace.to_str().unwrap().to_string(),
    ])
    .unwrap();
    options.session = Some(journal.to_path_buf());
    options
}

/// Drive one session process: send each request of `script` and read until it is answered,
/// allowing every permission request; then close the input and read to the end. The host's
/// result and every line the agent wrote.
async fn drive(
    harness: &mut Harness,
    options: &p1_host::cli::Options,
    workspace: &Path,
    script: Vec<Value>,
) -> (Result<i32, RunError>, Vec<Value>) {
    let (agent, user) = tokio::io::duplex(1 << 16);
    let (agent_read, agent_write) = tokio::io::split(agent);
    let (user_read, mut user_write) = tokio::io::split(user);
    let mut lines = tokio::io::BufReader::new(user_read).lines();
    let front_end = Arc::new(PortFrontEnd::new(Arc::new(
        p1_acp::driver::AcpFrontEnd::new(
            Box::new(agent_read),
            Box::new(agent_write),
            workspace.to_path_buf(),
        ),
    )));
    let client = async {
        let mut received = Vec::new();
        let mut session = String::new();
        for mut message in script {
            if message["params"]["sessionId"] == "<session>" {
                message["params"]["sessionId"] = json!(session);
            }
            let id = message["id"].clone();
            let mut line = serde_json::to_vec(&message).unwrap();
            line.push(b'\n');
            user_write.write_all(&line).await.unwrap();
            loop {
                let Some(line) = lines.next_line().await.unwrap() else {
                    return received;
                };
                let message: Value = serde_json::from_str(&line).unwrap();
                received.push(message.clone());
                if message["method"] == "session/request_permission" {
                    let answer = json!({"jsonrpc": "2.0", "id": message["id"],
                        "result": {"outcome": {"outcome": "selected", "optionId": "allow_once"}}});
                    let mut line = serde_json::to_vec(&answer).unwrap();
                    line.push(b'\n');
                    user_write.write_all(&line).await.unwrap();
                } else if message.get("method").is_none() && message["id"] == id {
                    if let Some(id) = message["result"]["sessionId"].as_str() {
                        session = id.to_string();
                    }
                    break;
                }
            }
        }
        user_write.shutdown().await.unwrap();
        while let Some(line) = lines.next_line().await.unwrap() {
            received.push(serde_json::from_str(&line).unwrap());
        }
        received
    };
    let stderr = &harness.stderr;
    let host = run_with_front_end(
        &mut harness.deps,
        options,
        CancellationToken::new(),
        front_end as Arc<dyn FrontEnd>,
    );
    tokio::time::timeout(Duration::from_secs(60), async {
        tokio::join!(host, client)
    })
    .await
    .unwrap_or_else(|_| panic!("p1 acp hung; stderr: {}", stderr.text()))
}

fn initialize() -> Value {
    json!({"jsonrpc": "2.0", "id": 0, "method": "initialize",
        "params": {"protocolVersion": 1, "clientCapabilities": {}}})
}

fn session_new(id: u64, workspace: &Path, servers: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "method": "session/new",
        "params": {"cwd": workspace, "mcpServers": servers}})
}

fn answer(received: &[Value], id: u64) -> &Value {
    received
        .iter()
        .find(|message| message.get("method").is_none() && message["id"] == id)
        .unwrap_or_else(|| panic!("no answer to {id}: {received:?}"))
}

#[tokio::test]
async fn client_stdio_and_http_servers_serve_the_session_masked_and_journalled_by_name() {
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
    let stdio_key = fake_key("Hj3kL9mN2p");
    let http_key = fake_key("Zx8cV5bN1q");
    let url = http_server(format!("Bearer {http_key}"));

    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("c1", "mcp__sh__hello", "{}"),
            json_call("c2", "mcp__web__fetch", "{}"),
        ]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.shell_env = Some(shell_env());
    let journal = scratch.path().join("session.jsonl");
    let options = options(workspace.path(), &journal);

    let servers = json!([
        {"name": "sh", "command": "bash", "args": [script],
         "env": [{"name": "SH_TOKEN", "value": stdio_key}]},
        {"type": "http", "name": "web", "url": url,
         "headers": [{"name": "Authorization", "value": format!("Bearer {http_key}")}]},
    ]);
    let (code, received) = drive(
        &mut harness,
        &options,
        workspace.path(),
        vec![
            initialize(),
            session_new(1, workspace.path(), servers),
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": "<session>", "prompt": [{"type": "text", "text": "go"}]}}),
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code.ok(), Some(0), "stderr: {stderr}");
    assert_eq!(
        answer(&received, 0)["result"]["agentCapabilities"]["mcpCapabilities"],
        json!({"http": true, "sse": false}),
        "streamable HTTP is advertised, legacy SSE is not"
    );
    assert!(answer(&received, 1)["result"]["sessionId"].is_string());
    assert_eq!(answer(&received, 2)["result"]["stopReason"], "end_turn");

    let requests = handle.requests();
    let declared: Vec<_> = requests[0]
        .tools
        .iter()
        .map(|tool| tool.name.as_str())
        .collect();
    assert!(
        declared.contains(&"mcp__sh__hello") && declared.contains(&"mcp__web__fetch"),
        "{declared:?}"
    );
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
    let hello = results
        .iter()
        .find(|result| result.starts_with("hello "))
        .unwrap_or_else(|| panic!("{results:?}"));
    assert!(!hello.contains(&stdio_key), "masked: {hello}");
    assert!(
        hello.contains("token=") && !hello.contains("token= "),
        "{hello}"
    );
    assert!(
        hello.contains("canary=absent"),
        "nothing outside the baseline and the client's list is inherited: {hello}"
    );
    assert!(
        hello.contains("path=set"),
        "the baseline keeps PATH: {hello}"
    );
    let folder = workspace.path().canonicalize().unwrap();
    assert!(
        hello.contains(&format!("dir={}", folder.display()))
            || hello.contains(&format!("dir={}", workspace.path().display())),
        "the child runs in the session's folder: {hello}"
    );
    let web = results
        .iter()
        .find(|result| result.starts_with("web "))
        .unwrap_or_else(|| panic!("{results:?}"));
    assert!(
        web.starts_with("web authorized=true"),
        "the header reached the server: {web}"
    );
    assert!(!web.contains(&http_key), "masked: {web}");

    let journal = std::fs::read_to_string(&journal).unwrap();
    for key in [&stdio_key, &http_key] {
        assert!(
            !journal.contains(key.as_str()),
            "the journal holds no value"
        );
        assert!(!stderr.contains(key.as_str()), "stderr holds no value");
        for message in &received {
            assert!(
                !message.to_string().contains(key.as_str()),
                "the wire carries no value back: {message}"
            );
        }
    }
    let environment = journal
        .lines()
        .find(|line| line.contains(r#""record":"environment""#))
        .expect("an Environment record");
    for (implementation, name) in [("mcp:sh", "mcp__sh__hello"), ("mcp:web", "mcp__web__fetch")] {
        assert!(
            environment.contains(&format!(r#""implementation":"{implementation}""#))
                && environment.contains(&format!(r#""name":"{name}""#)),
            "the generation is journalled by name: {environment}"
        );
    }
}

#[tokio::test]
async fn a_client_server_named_like_an_environment_server_fails_session_new_naming_it() {
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
        "[[mcp_servers]]\nname = \"sh\"\ncommand = \"bash\"\nargs = [{:?}]\n",
        script.to_str().unwrap()
    ));
    std::fs::write(&file, toml).unwrap();
    let key = fake_key("Qw4eR6tY8u");
    let provider = ScriptedProvider::new(vec![text_response("unused")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.shell_env = Some(shell_env());
    let journal = scratch.path().join("session.jsonl");
    let options = options(workspace.path(), &journal);

    let (code, received) = drive(
        &mut harness,
        &options,
        workspace.path(),
        vec![
            initialize(),
            session_new(
                1,
                workspace.path(),
                json!([{"name": "sh", "command": "bash", "args": [script],
                        "env": [{"name": "SH_TOKEN", "value": key}]}]),
            ),
        ],
    )
    .await;
    let error = &answer(&received, 1)["error"];
    let message = error["message"].as_str().unwrap_or_default();
    assert!(
        message.contains("MCP server `sh`") && message.contains("the environment already names"),
        "{error}"
    );
    assert!(!message.contains(&key), "{message}");
    assert!(code.is_err(), "the session does not start: fail closed");
    assert!(handle.requests().is_empty(), "no turn ran");
    assert!(!harness.stderr.text().contains(&key));
}

#[tokio::test]
async fn an_sse_server_is_invalid_params_and_the_session_can_still_open() {
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
    let provider = ScriptedProvider::new(vec![text_response("unused")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    let journal = scratch.path().join("session.jsonl");
    let options = options(workspace.path(), &journal);

    let (code, received) = drive(
        &mut harness,
        &options,
        workspace.path(),
        vec![
            initialize(),
            session_new(
                1,
                workspace.path(),
                json!([{"type": "sse", "name": "old", "url": "http://127.0.0.1:9/sse",
                        "headers": []}]),
            ),
            session_new(2, workspace.path(), json!([])),
        ],
    )
    .await;
    assert_eq!(code.ok(), Some(0), "stderr: {}", harness.stderr.text());
    let error = &answer(&received, 1)["error"];
    assert_eq!(error["code"], -32602, "{error}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|message| message.contains("MCP server `old`") && message.contains("sse")),
        "{error}"
    );
    assert!(answer(&received, 2)["result"]["sessionId"].is_string());
}

/// ADR-0184: a value the client sent for `sh` masks only `sh`'s tools. The same string in a
/// workspace file reaches the model through `read` as it is: the client's values go only into
/// `sh`'s process, so p1's own tools cannot see them. The masked call's record counts what was
/// masked; the `read` record counts nothing.
#[tokio::test]
async fn a_client_value_masks_its_own_servers_output_and_not_another_tools() {
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
    let value = fake_key("Mode7production");
    std::fs::write(workspace.path().join("note.txt"), format!("mode {value}\n")).unwrap();
    let provider = ScriptedProvider::new(vec![
        tool_call_response(vec![
            json_call("c1", "mcp__sh__hello", "{}"),
            json_call("c2", "read", r#"{"file_path":"note.txt"}"#),
        ]),
        text_response("done"),
    ]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.shell_env = Some(shell_env());
    let journal = scratch.path().join("session.jsonl");
    let options = options(workspace.path(), &journal);

    let (code, _) = drive(
        &mut harness,
        &options,
        workspace.path(),
        vec![
            initialize(),
            session_new(
                1,
                workspace.path(),
                json!([{"name": "sh", "command": "bash", "args": [script],
                        "env": [{"name": "SH_TOKEN", "value": value}]}]),
            ),
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": "<session>", "prompt": [{"type": "text", "text": "go"}]}}),
        ],
    )
    .await;
    assert_eq!(code.ok(), Some(0), "stderr: {}", harness.stderr.text());
    let results: Vec<(String, String)> = handle
        .requests()
        .last()
        .unwrap()
        .history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some((result.call_id.clone(), result.content.clone())),
            _ => None,
        })
        .collect();
    let result = |id: &str| {
        results
            .iter()
            .find(|(call, _)| call == id)
            .map(|(_, content)| content.as_str())
            .unwrap_or_else(|| panic!("{results:?}"))
    };
    assert!(
        result("c1").starts_with("hello token=<redacted:secret:") && !result("c1").contains(&value),
        "the server's own output is masked: {}",
        result("c1")
    );
    assert!(
        result("c2").contains(&format!("mode {value}")),
        "another tool's output is not: {}",
        result("c2")
    );

    let loaded = p1_journal::load(&journal).unwrap();
    let masked: Vec<(String, u64)> = loaded
        .records
        .iter()
        .zip(&loaded.masked)
        .filter_map(|(record, masked)| match &record.body {
            p1_contracts::RecordBody::ToolFinished { result, .. } => {
                Some((result.call_id.clone(), *masked))
            }
            _ => None,
        })
        .collect();
    assert_eq!(
        masked,
        [("c1".to_owned(), 1), ("c2".to_owned(), 0)],
        "each call's record counts what was masked in it"
    );
}

/// A stdio server that refuses `initialize` with an error echoing the variable it was given.
const ECHOING_FAILURE: &str = r#"
while IFS= read -r line; do
  id=$(printf '%s' "$line" | sed -n 's/.*"id":\([0-9][0-9]*\).*/\1/p')
  printf '{"jsonrpc":"2.0","id":%s,"error":{"code":-32000,"message":"bad token %s"}}\n' "$id" "$SH_TOKEN"
done
"#;

/// ADR-0186: a client server is optional. One that fails to start does not fail
/// `session/new`: the session opens and runs without its tools, and the session's assembly line
/// records the server with its reason. The reason echoes a client value, which is not in the
/// session's set, yet it is masked in the record, on stderr and on the wire (ADR-0184).
#[tokio::test]
async fn a_failing_client_server_is_skipped_and_its_reason_recorded_masked() {
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
    std::fs::write(&script, ECHOING_FAILURE).unwrap();
    let key = fake_key("Rt5yU7iO9p");
    let provider = ScriptedProvider::new(vec![text_response("done")]);
    let handle = provider.clone();
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![("fake", provider)]));
    harness.deps.shell_env = Some(shell_env());
    let journal = scratch.path().join("session.jsonl");
    let options = options(workspace.path(), &journal);

    let (code, received) = drive(
        &mut harness,
        &options,
        workspace.path(),
        vec![
            initialize(),
            session_new(
                1,
                workspace.path(),
                json!([{"name": "sh", "command": "bash", "args": [script],
                        "env": [{"name": "SH_TOKEN", "value": key}]}]),
            ),
            json!({"jsonrpc": "2.0", "id": 2, "method": "session/prompt",
                "params": {"sessionId": "<session>", "prompt": [{"type": "text", "text": "go"}]}}),
        ],
    )
    .await;
    let stderr = harness.stderr.text();
    assert_eq!(code.ok(), Some(0), "stderr: {stderr}");
    assert!(answer(&received, 1)["result"]["sessionId"].is_string());
    assert_eq!(answer(&received, 2)["result"]["stopReason"], "end_turn");
    let requests = handle.requests();
    assert!(
        !requests[0]
            .tools
            .iter()
            .any(|tool| tool.name.starts_with("mcp__sh__")),
        "the skipped server serves no tool"
    );

    let loaded = p1_journal::load(&journal).unwrap();
    let skipped = &loaded.assemblies[0].identity.mcp_skipped;
    assert_eq!(skipped.len(), 1, "{skipped:?}");
    assert_eq!(skipped[0].name, "sh");
    assert!(
        skipped[0].reason.contains("bad token <redacted:secret:"),
        "{:?}",
        skipped[0]
    );
    assert!(!std::fs::read_to_string(&journal).unwrap().contains(&key));
    assert!(!stderr.contains(&key));
    for message in &received {
        assert!(!message.to_string().contains(&key), "{message}");
    }
}
