//! The Codex `write_stdin` tool as a component (`p1/write-stdin`, ADR-0218).
//!
//! A Codex session is a p1 job (`j<N>`) that `exec_command` left running. A p1 job has no
//! stdin, so writing fails with Codex's own sentence; an empty write polls: a running job
//! answers with its session id and no new text (a job's output is stored when it ends,
//! ADR-0117), an ended job with its exit code and its stored output.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::process::ExitStatus;
use p1_bindings_tool::generated::p1::module::process_jobs::{self, JobError, JobState};
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::{clock, random, tool_outputs};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_exec_command_guest::{Response, wire, write_stdin as face};

/// Codex collects at most this much of a session's output per response.
const OUTPUT_MAX_BYTES: usize = 1024 * 1024;
const PAGE_BYTES: u32 = 65_536;

struct WriteStdin;

impl Guest for WriteStdin {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: face::NAME.to_owned(),
            description: face::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(face::input_schema().to_string()),
        }
    }

    fn effect(call: ToolCall) -> CallEffect {
        // A poll reads; a write would reach a process. Unreadable input is the worst case.
        match input(&call) {
            Ok(input) if input.chars.is_empty() => CallEffect::ReadOnly,
            _ => CallEffect::Executes,
        }
    }

    fn describe(call: ToolCall) -> CallDescription {
        let (verb, target) = match input(&call) {
            Ok(input) if input.chars.is_empty() => {
                ("poll", Some(format!("session {}", input.session_id)))
            }
            Ok(input) => ("write", Some(format!("session {}", input.session_id))),
            Err(_) => ("write", None),
        };
        wire::text(&wire::CallDescription {
            verb,
            target,
            destructive: false,
            shared: verb == "poll",
        })
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

p1_bindings_tool::generated::export!(WriteStdin);

fn input(call: &str) -> Result<face::Input, String> {
    let call = serde_json::from_str::<wire::Call>(call)
        .map_err(|error| format!("Invalid input for write_stdin: {error}"))?;
    call.input.json().and_then(face::parse)
}

fn run(call: &str) -> (&'static str, String) {
    let input = match input(call) {
        Ok(input) => input,
        Err(message) => return ("error", message),
    };
    if !input.chars.is_empty() {
        return ("error", face::STDIN_CLOSED.to_owned());
    }
    let started = clock::now();
    let job_id = p1_exec_command_guest::job_id(input.session_id);
    let chunk_id = chunk_id();
    match process_jobs::status(&job_id) {
        Err(JobError::UnknownJob) => ("error", face::unknown_session(input.session_id)),
        Err(JobError::StartFailed(message)) => ("error", format!("write_stdin failed: {message}")),
        Ok(JobState::Running(_)) => {
            let response = Response {
                chunk_id: &chunk_id,
                wall_time_ms: elapsed_ms(started),
                exit_code: None,
                session_id: Some(input.session_id),
                raw_output: &[],
                max_output_tokens: input.max_output_tokens,
            };
            ("ok", response.render())
        }
        Ok(JobState::Ended(end)) => {
            let output = stored_output(&end.output);
            let exit_code = match end.status {
                ExitStatus::Code(code) => code,
                ExitStatus::Signal(signal) => 128 + signal,
                ExitStatus::UnknownSignal | ExitStatus::TimedOut | ExitStatus::Cancelled => 1,
            };
            let response = Response {
                chunk_id: &chunk_id,
                wall_time_ms: elapsed_ms(started),
                exit_code: Some(exit_code),
                session_id: None,
                raw_output: output.as_bytes(),
                max_output_tokens: input.max_output_tokens,
            };
            ("ok", response.render())
        }
    }
}

/// An ended job's output from the store, up to Codex's collection cap.
fn stored_output(handle: &str) -> String {
    let mut text = String::new();
    let mut offset = 0u64;
    while text.len() < OUTPUT_MAX_BYTES {
        match tool_outputs::page(handle, offset, PAGE_BYTES) {
            Ok(page) => {
                let at_end = page.at_end;
                text.push_str(&page.text);
                offset = page.next_offset;
                if at_end || page.text.is_empty() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
    text
}

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
