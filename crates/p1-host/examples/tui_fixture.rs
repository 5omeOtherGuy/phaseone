//! The real host, provider adapter, tools and terminal driver with a deterministic
//! in-process HTTP transport. No network, credential file, or paid model required.
//!
//! P1_FIXTURE_KEY=fixture cargo run -p p1-host --example tui_fixture -- /tmp/p1-ui-check
//!
//! The scratch directory is the workspace and holds `session.jsonl`; starting again on
//! the same directory resumes that session. `P1_FIXTURE_ASK=1` runs under `--ask`, so
//! writes and commands park for approval.
//!
//! The reply is chosen by a keyword in the latest prompt; the step within a scenario
//! is the number of assistant responses since that prompt:
//!
//! | keyword  | behaviour                                                               |
//! |----------|-------------------------------------------------------------------------|
//! | (none)   | one real shell call printing 60 rows, then a prose answer               |
//! | `cancel` | a 30-second shell sleep, to exercise cancellation                       |
//! | `stream` | streamed reasoning, then ~60 rows of prose in small chunks              |
//! | `slow`   | 80 rows of prose over ~20 s, to cancel mid-stream                       |
//! | `md`     | one prose reply with code, lists, long words and wide characters        |
//! | `long`   | a shell call printing 4000 rows                                         |
//! | `fail`   | a shell call writing to stderr and exiting 101                          |
//! | `wide`   | tabs, ANSI colour, CJK and 300-column lines                             |
//! | `files`  | write, edit, then read `notes.txt`                                      |
//! | `many`   | three calls in one response (one fails: a missing file)                 |
//! | `flood N`| N sequential shell calls (default 50), for large histories              |
//! | `error`  | the provider answers HTTP 500                                           |
use futures_util::StreamExt;
use p1_contracts::BoxFuture;
use p1_provider_http::{HttpRequest, HttpResponse, Transport, TransportError};
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const PROSE: &[&str] = &[
    "Here is what changed and why.",
    "",
    "The renderer keeps settled rows cached per width, so a resize re-lays out only once.",
    "Streaming text is coalesced into frames rather than drawn per delta, which keeps the",
    "terminal responsive even when the provider sends hundreds of tiny chunks per second.",
    "",
    "```rust",
    "fn frame(budget: Duration) -> bool { budget > Duration::from_millis(16) }",
    "```",
    "",
    "- first point, short",
    "- second point, which is deliberately long enough that it must wrap at eighty columns without losing its hanging indent",
    "- a path: crates/p1-tui/src/render/block.rs and an identifier_that_is_extremely_long_and_cannot_break_anywhere_sensible_at_all",
    "",
    "Unicode check: 雪が降る — naïve café, emoji 🚀 (wide), tabs\tstay\tsane.",
];

enum Reply {
    Tools(Vec<Value>),
    Text {
        lines: Vec<String>,
        reasoning: bool,
        delay_ms: u64,
    },
    Error(u16),
}

fn call(id: &str, name: &str, args: Value) -> Value {
    json!({"id": id, "type": "function", "function": {"name": name, "arguments": args.to_string()}})
}

fn shell(id: &str, command: &str) -> Value {
    call(
        id,
        "shell",
        json!({"command": command, "timeout_seconds": 60}),
    )
}

fn text(lines: &[&str]) -> Reply {
    Reply::Text {
        lines: lines.iter().map(|s| s.to_string()).collect(),
        reasoning: false,
        delay_ms: 0,
    }
}

fn plan(prompt: &str, step: usize, turn: usize) -> Reply {
    let p = prompt.to_lowercase();
    // Call ids stay unique across the whole session: the journal replays them.
    let id = |n: usize| format!("t{turn}s{step}c{n}");
    if p.contains("error") {
        return Reply::Error(500);
    }
    if p.contains("cancel") {
        return if step == 0 {
            Reply::Tools(vec![shell(&id(0), "echo started; sleep 30; echo never")])
        } else {
            text(&["Cancelled work should not reach here."])
        };
    }
    if p.contains("slow") {
        return Reply::Text {
            lines: (0..80)
                .map(|i| format!("slow line {i}: the provider is still streaming this reply"))
                .collect(),
            reasoning: false,
            delay_ms: 30,
        };
    }
    if p.contains("stream") {
        return Reply::Text {
            lines: PROSE
                .iter()
                .cycle()
                .take(PROSE.len() * 4)
                .map(|s| s.to_string())
                .collect(),
            reasoning: true,
            delay_ms: 12,
        };
    }
    if p.contains("md") {
        return text(PROSE);
    }
    let then = |tools: Vec<Value>, done: &[&str]| {
        if step == 0 {
            Reply::Tools(tools)
        } else {
            text(done)
        }
    };
    if p.contains("long") {
        return then(
            vec![shell(&id(0), "seq -f 'long output line %05g' 1 4000")],
            &["4000 lines retained behind the handle."],
        );
    }
    if p.contains("fail") {
        return then(
            vec![shell(
                &id(0),
                "echo 'compiling p1-tui'; echo 'error[E0425]: cannot find value `frame` in this scope' >&2; exit 101",
            )],
            &["The command failed with exit 101; the error is above."],
        );
    }
    if p.contains("wide") {
        return then(
            vec![shell(
                &id(0),
                "printf 'col1\\tcol2\\tcol3\\n'; printf '\\033[31mred ansi\\033[0m and \\033[1mbold\\033[0m\\n'; \
                 echo '雪が降る日には 静かな音が 聞こえる — wide cells'; \
                 python3 -c \"print('x'*300)\"; python3 -c \"print(' '.join(f'w{i}' for i in range(120)))\"",
            )],
            &["Wide output: pan with Left/Right in the output pane."],
        );
    }
    if p.contains("files") {
        let content: String = (1..=12).map(|i| format!("note line {i}\n")).collect();
        return match step {
            0 => Reply::Tools(vec![call(
                &id(0),
                "write",
                json!({"file_path": "notes.txt", "content": content}),
            )]),
            1 => Reply::Tools(vec![call(
                &id(0),
                "edit",
                json!({"file_path": "notes.txt", "old_string": "note line 3\nnote line 4\n",
                       "new_string": "note line three\nnote line four\nnote line 4.5\n"}),
            )]),
            2 => Reply::Tools(vec![call(
                &id(0),
                "read",
                json!({"file_path": "notes.txt"}),
            )]),
            _ => text(&["Wrote, edited and read notes.txt."]),
        };
    }
    if p.contains("many") {
        return then(
            vec![
                shell(&id(0), "echo one"),
                call(&id(1), "read", json!({"file_path": "missing.txt"})),
                shell(&id(2), "printf 'a\\nb\\nc\\n'"),
            ],
            &["Three calls settled in order."],
        );
    }
    if p.contains("flood") {
        let n = p
            .split_whitespace()
            .find_map(|w| w.parse::<usize>().ok())
            .unwrap_or(50);
        return if step < n {
            Reply::Tools(vec![shell(
                &id(0),
                &format!(
                    "echo 'flood step {} of {n}'; printf 'detail a\\ndetail b\\n'",
                    step + 1
                ),
            )])
        } else {
            Reply::Text {
                lines: vec![format!("Flooded {n} calls.")],
                reasoning: false,
                delay_ms: 0,
            }
        };
    }
    then(
        vec![shell(
            &id(0),
            "python3 -c 'for i in range(60): print(f\"row {i:02}: actual shell output\")'",
        )],
        &[
            "The real shell completed. Output is retained behind its handle. Ctrl+O opens it; Escape returns to your draft.",
        ],
    )
}

struct FixtureTransport;
impl Transport for FixtureTransport {
    fn post<'a>(
        &'a self,
        request: HttpRequest,
    ) -> BoxFuture<'a, Result<HttpResponse, TransportError>> {
        Box::pin(async move {
            let body: Value =
                serde_json::from_slice(&request.body).map_err(|e| TransportError(e.to_string()))?;
            let messages = body["messages"]
                .as_array()
                .ok_or_else(|| TransportError("missing messages".into()))?;
            let users: Vec<usize> = messages
                .iter()
                .enumerate()
                .filter(|(_, m)| m["role"] == "user")
                .map(|(i, _)| i)
                .collect();
            let last_user = users.last().copied().unwrap_or(0);
            let prompt = messages
                .get(last_user)
                .and_then(|m| m["content"].as_str())
                .unwrap_or("");
            let step = messages[last_user..]
                .iter()
                .filter(|m| m["role"] == "assistant")
                .count();
            let reply = plan(prompt, step, users.len());
            let chunk = |delta: Value, finish: Option<&str>| {
                let mut event = json!({"id":"fixture","model":"deepseek-v4.1-flash",
                    "choices":[{"index":0,"delta":delta,"finish_reason":finish}]});
                if finish.is_some() {
                    event["usage"] = json!({"prompt_tokens": 1200 + 100 * step,
                        "completion_tokens": 80, "total_tokens": 1280 + 100 * step});
                }
                format!("data: {event}\n\n").into_bytes()
            };
            let (chunks, delay_ms) = match reply {
                Reply::Error(status) => {
                    let body = br#"{"error":{"message":"fixture upstream exploded","type":"server_error"}}"#;
                    return Ok(HttpResponse {
                        status,
                        headers: vec![("content-type".into(), "application/json".into())],
                        body: Box::pin(futures_util::stream::iter(vec![Ok(body.to_vec())])),
                    });
                }
                Reply::Tools(calls) => {
                    let calls: Vec<Value> = calls
                        .into_iter()
                        .enumerate()
                        .map(|(i, mut c)| {
                            c["index"] = json!(i);
                            c
                        })
                        .collect();
                    (
                        vec![
                            chunk(json!({"tool_calls": calls}), None),
                            chunk(json!({}), Some("tool_calls")),
                        ],
                        0,
                    )
                }
                Reply::Text {
                    lines,
                    reasoning,
                    delay_ms,
                } => {
                    let mut chunks = vec![];
                    if reasoning {
                        for part in [
                            "Considering the renderer. ",
                            "Checking the cache invalidation. ",
                            "Planning the reply.\n",
                        ] {
                            chunks.push(chunk(json!({"reasoning_content": part}), None));
                        }
                    }
                    let text: Vec<char> = lines.join("\n").chars().collect();
                    for piece in text.chunks(7) {
                        chunks.push(chunk(
                            json!({"content": piece.iter().collect::<String>()}),
                            None,
                        ));
                    }
                    chunks.push(chunk(json!({}), Some("stop")));
                    (chunks, delay_ms)
                }
            };
            let mut chunks = chunks;
            chunks.push(b"data: [DONE]\n\n".to_vec());
            let body = futures_util::stream::iter(chunks).then(move |bytes| async move {
                if delay_ms > 0 {
                    tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                }
                Ok(bytes)
            });
            Ok(HttpResponse {
                status: 200,
                headers: vec![("content-type".into(), "text/event-stream".into())],
                body: Box::pin(body),
            })
        })
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let workspace = std::env::args()
        .nth(1)
        .map(std::path::PathBuf::from)
        .expect("provide a dedicated scratch directory");
    std::fs::create_dir_all(&workspace).unwrap();
    let config = tempfile::tempdir().unwrap();
    let env = config.path().join("environments/fixture");
    std::fs::create_dir_all(&env).unwrap();
    std::fs::create_dir(config.path().join("routes")).unwrap();
    std::fs::create_dir(config.path().join("profiles")).unwrap();
    std::fs::write(
        env.join("environment.toml"),
        "route = \"fixture\"\nprofile = \"deepseek-v4.1-flash\"\n\
         [[tools]]\nmodule = \"shell\"\n[[tools]]\nmodule = \"read\"\n\
         [[tools]]\nmodule = \"write\"\n[[tools]]\nmodule = \"edit\"\n",
    )
    .unwrap();
    std::fs::write(env.join("prompt.md"), "Local synthetic UI verification.").unwrap();
    std::fs::write(
        config.path().join("profiles/deepseek-v4.1-flash.toml"),
        include_str!("../../../profiles/deepseek-v4.1-flash.toml"),
    )
    .unwrap();
    std::fs::write(config.path().join("routes/fixture.toml"),"id = \"fixture\"\norigin_route = \"openai-chat/fixture\"\nadapter = \"openai-chat\"\nendpoint = \"https://fixture.invalid/chat/completions\"\n[credential]\nkind = \"api-key\"\nenv = \"P1_FIXTURE_KEY\"\n[adapter_settings]\ndialect = \"thinking-with-reasoning-alias\"\n[models.\"deepseek-v4.1-flash\"]\nwire_model = \"deepseek-v4.1-flash\"\n").unwrap();
    let mut deps = p1_host::HostDeps::new(
        Arc::new(Mutex::new(Box::new(std::io::stdout()))),
        Arc::new(Mutex::new(Box::new(std::io::stderr()))),
        Arc::new(p1_host::StdinLines::new()),
        Arc::new(FixtureTransport),
        "2026-09-21".into(),
        Arc::new(p1_host::SignalInterrupt),
        vec![config.path().join("environments")],
        true,
    );
    deps.home = None;
    deps.runtime_dir = None;
    let session = workspace.join("session.jsonl");
    let mut args: Vec<String> = vec![
        "--tui".into(),
        "--env".into(),
        "fixture".into(),
        "--workspace".into(),
        workspace.display().to_string(),
        "--session".into(),
        session.display().to_string(),
    ];
    if session.exists() {
        args.push("--resume".into());
    }
    if std::env::var_os("P1_FIXTURE_ASK").is_some() {
        args.push("--ask".into());
    }
    let options = p1_host::cli::parse(&args).unwrap();
    let code = p1_host::run::run(&mut deps, options).await;
    std::process::exit(code);
}
