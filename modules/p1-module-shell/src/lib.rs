//! The `shell` tool as a component (`p1/shell`, ADR-0071).
//!
//! Everything the tool decides is `p1-shell-guest`, the same code the native adapter
//! (`ShellTool` in `p1-tool-shell`) runs; this crate only binds it to the `tool` world. How a
//! command runs is the host's: `process.spawn` takes the command text and its time limit and
//! nothing else, and the native process service chooses the sandbox, the environment, the
//! working directory and the output bounds, and kills the process group on timeout,
//! cancellation or drop (`modules/wit/process.wit`).
//!
//! What this component cannot know, it does not claim: whether the sandbox is on (the host
//! appends the sandbox paragraph and the `+sandbox` variant when it presents the tool) and
//! the workspace root (so `describe` classifies paths without one, on the cautious side).
#![forbid(unsafe_code)]

mod wire;

use p1_bindings_tool::generated::p1::module::process::{self, ExitStatus, ProcessEvent};
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_shell_guest::{End, Outcome};

struct Shell;

impl Guest for Shell {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: p1_shell_guest::NAME.to_owned(),
            description: p1_shell_guest::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(p1_shell_guest::input_schema().to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::Executes
    }

    fn describe(call: ToolCall) -> CallDescription {
        // Restricted path: from the input alone. An unreadable call is described as invalid
        // input, which the guest classifies at the worst case.
        let summary = match serde_json::from_str::<wire::Call>(&call) {
            Ok(call) => p1_shell_guest::describe(call.input.raw(), None),
            Err(_) => p1_shell_guest::describe(p1_shell_guest::RawInput::Json(""), None),
        };
        wire::text(&wire::CallDescription {
            verb: summary.verb,
            target: summary.target,
            destructive: summary.destructive,
        })
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let (content, ok) = match serde_json::from_str::<wire::ResultItem>(&tool_result) {
            Ok(item) => (item.content, item.status == "ok"),
            Err(_) => (String::new(), false),
        };
        let summary = p1_shell_guest::describe_result(&content, ok);
        wire::text(&wire::ResultDescription {
            summary: summary.summary,
            detail: wire::CommandDetail {
                exit_code: summary.exit_code,
                tail: summary.tail,
            },
        })
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        wire::outcome(&run(&call))
    }
}

p1_bindings_tool::generated::export!(Shell);

fn run(call: &str) -> Outcome {
    let call = match serde_json::from_str::<wire::Call>(call) {
        Ok(call) => call,
        Err(error) => return Outcome::error(format!("Invalid input for shell: {error}")),
    };
    let input = match p1_shell_guest::parse_input(&call.name, call.input.raw()) {
        Ok(input) => input,
        Err(message) => return Outcome::error(message),
    };
    let timeout_seconds = input.timeout_seconds();
    let running = match process::spawn(&process::Command {
        script: input.command.clone(),
        timeout_ms: timeout_seconds.saturating_mul(1_000),
    }) {
        Ok(running) => running,
        // The host words why nothing started, as the native service always has.
        Err(message) => return Outcome::error(message),
    };
    let mut output: Vec<u8> = Vec::new();
    let end = loop {
        match running.next() {
            Some(ProcessEvent::Output(chunk)) => output.extend_from_slice(&chunk),
            Some(ProcessEvent::Exited(status)) => break end(status),
            // `none` before `exited` breaks the resource's contract; report it rather than
            // invent an exit.
            None => return Outcome::error("the process ended without an exit status"),
        }
    };
    // Dropped before returning, so the host never has to reclaim it after the call.
    drop(running);
    p1_shell_guest::finished(&output, end, &input.command, timeout_seconds, input.raw)
}

fn end(status: ExitStatus) -> End {
    match status {
        ExitStatus::Code(code) => End::Exited(code),
        ExitStatus::Signal(signal) => End::TerminatedBySignal(signal),
        ExitStatus::UnknownSignal => End::TerminatedByUnknownSignal,
        ExitStatus::TimedOut => End::TimedOut,
        ExitStatus::Cancelled => End::Cancelled,
    }
}
