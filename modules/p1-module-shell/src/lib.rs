//! The `shell` tool as a component (`p1/shell`, ADR-0071).
//!
//! Everything the tool decides is `p1-shell-guest`, the same code the native adapter
//! (`ShellTool` in `p1-tool-shell`) runs; this crate only binds it to the `tool` world. How a
//! command runs is the host's: `process.spawn` takes the command text and its time limit and
//! nothing else, and the native process service chooses the sandbox, the environment, the
//! working directory and the output bounds, and kills the process group on cancellation or
//! drop (`modules/wit/process.wit`). A command that reaches its time limit while it runs is
//! handed over to the session's job registry instead (ADR-0123).
//!
//! The host also stores what each command printed, masked and before it is cut (ADR-0109):
//! when the result shows less than the command printed, the guest names the stored output's
//! handle, which `read_output` pages, or says the output could not be stored.
//!
//! What this component cannot know, it does not claim: whether the sandbox is on (the host
//! appends the sandbox paragraph and the `+sandbox` variant when it presents the tool) and
//! the workspace root (so `describe` classifies paths without one, on the cautious side).
#![forbid(unsafe_code)]

mod wire;

use p1_bindings_tool::generated::p1::module::process::{self, ExitStatus, ProcessEvent};
use p1_bindings_tool::generated::p1::module::process_jobs;
use p1_bindings_tool::generated::p1::module::tool_outputs::{self, Capture};
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_shell_guest::{End, Outcome, StoredOutput};

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
            shared: summary.shared,
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
    if input.background {
        return match p1_bindings_tool::generated::p1::module::process_jobs::start(
            &input.command,
            input.background_timeout_ms(),
        ) {
            Ok(id) => Outcome {
                status: p1_shell_guest::Status::Ok,
                content: format!(
                    "Background job {id} started. Completion arrives as a notification; check with shell_job {{job_id: \"{id}\", action: \"status\"}}; read its output with read_output once a handle is available."
                ),
            },
            Err(error) => Outcome::error(format!("cannot start background job: {error:?}")),
        };
    }
    let timeout_seconds = input.timeout_seconds();
    // ADR-0118: a Shared call runs with `GIT_OPTIONAL_LOCKS=0`; every other call runs its
    // command unchanged.
    let running = match process::spawn(&process::Command {
        script: p1_shell_guest::script(&input).into_owned(),
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
    // The host stored what the command printed before it cut it (ADR-0109); this call started
    // one command, so the last output the call produced is its own.
    let stored = tool_outputs::produced().pop().map(|info| StoredOutput {
        handle: info.handle,
        stored_bytes: info.stored_bytes,
        capture: match info.capture {
            Capture::Complete => p1_shell_guest::Capture::Complete,
            Capture::StoredCapReached => p1_shell_guest::Capture::StoredCapReached,
            Capture::StorageIncomplete => p1_shell_guest::Capture::StorageIncomplete,
            Capture::StorageFailed => p1_shell_guest::Capture::StorageFailed,
        },
    });
    // ADR-0123: the host may have handed a command that reached `timeout_seconds` over to a
    // session background job instead of killing it; `handed-over` names that job. Given one,
    // the call succeeds at once with the job the model can watch; without one (a host that
    // did not hand over, or a call that ended any other way) the footer is as before.
    if matches!(end, End::TimedOut)
        && let Some(id) = process_jobs::handed_over()
    {
        return p1_shell_guest::handed_over(&id, timeout_seconds, &output);
    }
    p1_shell_guest::finished_with_store(
        &output,
        end,
        &input.command,
        timeout_seconds,
        input.raw,
        stored.as_ref(),
    )
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
