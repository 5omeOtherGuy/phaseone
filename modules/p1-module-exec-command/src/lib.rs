//! The Codex `exec_command` tool as a component (`p1/exec-command`, ADR-0218): the face of
//! `p1-exec-command-guest` over the same process capability `p1/shell` runs on.
//!
//! The command runs with `bash -lc` under the host's sandbox for `yield_time_ms`; a command
//! still running then is handed over to a session job (ADR-0123), which the response names as
//! the session `write_stdin` polls. The host stores what the command printed (ADR-0109); this
//! face shows it as Codex shows it, truncated in the middle to the token budget.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::process::{self, ExitStatus, ProcessEvent};
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::{clock, process_jobs, random};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_exec_command_guest::{Response, exec_command as face, wire};

/// Codex collects at most this much of a command's output per response.
const OUTPUT_MAX_BYTES: usize = 1024 * 1024;

struct ExecCommand;

impl Guest for ExecCommand {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: face::NAME.to_owned(),
            description: face::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(face::input_schema().to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Executes
    }

    fn describe(call: ToolCall) -> CallDescription {
        let cmd = serde_json::from_str::<wire::Call>(&call)
            .ok()
            .and_then(|call| call.input.json().ok().map(str::to_owned))
            .and_then(|raw| face::parse(&raw).ok())
            .map(|input| input.cmd)
            .unwrap_or_default();
        wire::text(&p1_exec_command_guest::describe_command(&cmd))
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let (content, ok) = match serde_json::from_str::<wire::ResultItem>(&tool_result) {
            Ok(item) => (item.content, item.status == "ok"),
            Err(_) => (String::new(), false),
        };
        wire::text(&p1_exec_command_guest::describe_response(&content, ok))
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let (status, content) = run(&call);
        wire::outcome(status, &content)
    }
}

p1_bindings_tool::generated::export!(ExecCommand);

fn run(call: &str) -> (&'static str, String) {
    let call = match serde_json::from_str::<wire::Call>(call) {
        Ok(call) => call,
        Err(error) => return ("error", format!("Invalid input for exec_command: {error}")),
    };
    let input = match call.input.json().and_then(face::parse) {
        Ok(input) => input,
        Err(message) => return ("error", message),
    };
    if input.requests_escalation() {
        return ("error", face::ESCALATION_REFUSED.to_owned());
    }
    let started = clock::now();
    let yield_time_ms = input.yield_time_ms();
    let running = match process::spawn(&process::Command {
        script: input.script(),
        timeout_ms: yield_time_ms,
    }) {
        Ok(running) => running,
        Err(message) => return ("error", p1_exec_command_guest::failure(&message)),
    };
    let mut output: Vec<u8> = Vec::new();
    let mut omitted: usize = 0;
    let end = loop {
        match running.next() {
            Some(ProcessEvent::Output(chunk)) => {
                let room = OUTPUT_MAX_BYTES.saturating_sub(output.len());
                let kept = chunk.len().min(room);
                output.extend_from_slice(&chunk[..kept]);
                omitted += chunk.len() - kept;
            }
            Some(ProcessEvent::Exited(status)) => break status,
            None => {
                return (
                    "error",
                    p1_exec_command_guest::failure("the process ended without an exit status"),
                );
            }
        }
    };
    drop(running);
    let wall_time_ms = elapsed_ms(started);
    if omitted > 0 {
        let marker = format!("... {omitted} bytes omitted ...\n");
        output.splice(0..0, marker.into_bytes());
    }
    let chunk_id = chunk_id();
    let (exit_code, session_id) = match end {
        ExitStatus::Code(code) => (Some(code), None),
        ExitStatus::Signal(signal) => (Some(128 + signal), None),
        ExitStatus::UnknownSignal => (Some(1), None),
        ExitStatus::Cancelled => {
            return ("cancelled", p1_exec_command_guest::aborted(wall_time_ms));
        }
        ExitStatus::TimedOut => match process_jobs::handed_over() {
            Some(id) => match p1_exec_command_guest::session_id(&id) {
                Some(session) => (None, Some(session)),
                None => {
                    return (
                        "error",
                        p1_exec_command_guest::failure(&format!(
                            "the command still runs as job {id}, which is not a session id"
                        )),
                    );
                }
            },
            None => {
                return (
                    "error",
                    p1_exec_command_guest::failure(&format!(
                        "the command was killed after {yield_time_ms} ms"
                    )),
                );
            }
        },
    };
    let response = Response {
        chunk_id: &chunk_id,
        wall_time_ms,
        exit_code,
        session_id,
        raw_output: &output,
        max_output_tokens: input.max_output_tokens,
    };
    ("ok", response.render())
}

/// Codex's chunk id: six hex digits from the host's random source.
fn chunk_id() -> String {
    random::bytes(3)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn elapsed_ms(started: clock::Datetime) -> u64 {
    let now = clock::now();
    let start = u128::from(started.seconds) * 1_000 + u128::from(started.nanoseconds / 1_000_000);
    let end = u128::from(now.seconds) * 1_000 + u128::from(now.nanoseconds / 1_000_000);
    end.saturating_sub(start).min(u128::from(u64::MAX)) as u64
}
