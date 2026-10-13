//! E5: the `shell` tool's guest logic (`p1-shell-guest`, the same code `modules/p1-module-shell`
//! binds to the tool world) bound to the generic channel. `tool/execute` and `tool/describe`
//! are events; the process is `call("process", "spawn" | "next" | "close", ...)`.
//!
//! Left out of the spike, each of which would be one more capability or method in the host's
//! catalog and no WIT: background jobs and the one-build-per-session check (`process-jobs`),
//! stored outputs (`tool-outputs`) and the timed-out hand-over. A background call is refused.

use p1_bindings_channel::generated::Guest;
use p1_bindings_channel::generated::p1::channel::host;
use p1_shell_guest::{End, Outcome, RawInput, Status};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

struct Shell;

const MANIFEST: &str =
    r#"{"events":{"tool/execute":1,"tool/describe":1},"capabilities":["process"]}"#;

impl Guest for Shell {
    fn manifest() -> String {
        MANIFEST.to_owned()
    }

    fn handle(event: String, payload: String) -> Option<String> {
        match event.as_str() {
            "tool/execute" => Some(outcome(&run(&payload))),
            "tool/describe" => Some(describe(&payload)),
            _ => None,
        }
    }
}

p1_bindings_channel::generated::export!(Shell);

#[derive(Deserialize)]
struct Call {
    name: String,
    input: Input,
}

#[derive(Deserialize)]
#[serde(tag = "kind", content = "raw", rename_all = "snake_case")]
enum Input {
    Json(String),
    Text(String),
}

impl Input {
    fn raw(&self) -> RawInput<'_> {
        match self {
            Self::Json(raw) => RawInput::Json(raw),
            Self::Text(raw) => RawInput::Text(raw),
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Event {
    Output(Vec<u8>),
    Exited(Exit),
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum Exit {
    Code(i32),
    Signal(i32),
    UnknownSignal,
    TimedOut,
    Cancelled,
}

#[derive(Serialize)]
struct WireOutcome<'a> {
    status: &'static str,
    content: &'a str,
}

fn describe(call: &str) -> String {
    let summary = match serde_json::from_str::<Call>(call) {
        Ok(call) => p1_shell_guest::describe(call.input.raw(), None),
        Err(_) => p1_shell_guest::describe(RawInput::Json(""), None),
    };
    json!({
        "verb": summary.verb,
        "target": summary.target,
        "destructive": summary.destructive,
    })
    .to_string()
}

fn run(call: &str) -> Outcome {
    let call = match serde_json::from_str::<Call>(call) {
        Ok(call) => call,
        Err(error) => return Outcome::error(format!("Invalid input for shell: {error}")),
    };
    let input = match p1_shell_guest::parse_input(&call.name, call.input.raw()) {
        Ok(input) => input,
        Err(message) => return Outcome::error(message),
    };
    if input.background {
        return Outcome::error("background jobs are not part of the channel spike");
    }
    let timeout_seconds = input.timeout_seconds();
    let spawn = json!({
        "script": p1_shell_guest::script(&input),
        "timeout_ms": timeout_seconds.saturating_mul(1_000),
    });
    let process = match host::call("process", "spawn", &spawn.to_string()) {
        Ok(answer) => {
            serde_json::from_str::<Value>(&answer).unwrap_or(Value::Null)["process"].clone()
        }
        Err(message) => return Outcome::error(message),
    };
    let id = json!({ "process": process }).to_string();
    let mut output: Vec<u8> = Vec::new();
    let end = loop {
        let event = match host::call("process", "next", &id) {
            Ok(event) => event,
            Err(message) => return Outcome::error(message),
        };
        match serde_json::from_str::<Option<Event>>(&event) {
            Ok(Some(Event::Output(chunk))) => output.extend_from_slice(&chunk),
            Ok(Some(Event::Exited(exit))) => break end(exit),
            Ok(None) => return Outcome::error("the process ended without an exit status"),
            Err(error) => return Outcome::error(format!("unreadable process event: {error}")),
        }
    };
    let _ = host::call("process", "close", &id);
    p1_shell_guest::finished_with_store(
        &output,
        end,
        &input.command,
        timeout_seconds,
        input.raw,
        None,
    )
}

fn end(exit: Exit) -> End {
    match exit {
        Exit::Code(code) => End::Exited(code),
        Exit::Signal(signal) => End::TerminatedBySignal(signal),
        Exit::UnknownSignal => End::TerminatedByUnknownSignal,
        Exit::TimedOut => End::TimedOut,
        Exit::Cancelled => End::Cancelled,
    }
}

fn outcome(outcome: &Outcome) -> String {
    serde_json::to_string(&WireOutcome {
        status: match outcome.status {
            Status::Ok => "ok",
            Status::Error => "error",
            Status::Cancelled => "cancelled",
        },
        content: &outcome.content,
    })
    .expect("a wire outcome serializes")
}
