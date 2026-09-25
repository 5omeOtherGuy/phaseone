//! The fixture tool module: one guest component that the runtime spike and the cancellation
//! tests of later slices load.
//!
//! It is a package of the module workspace (`p1/fixture`; its frozen manifest fields are in
//! `Cargo.toml`, the format in `docs/design/modules/package.md`) and implements the `tool`
//! world of `modules/wit/`. Its whole point is to be a known guest: the raw input text picks
//! one mode, each mode exists so the host can exercise one runtime property, and every answer
//! is fixed text.
//!
//! The modes of `execute`, one per line of its input:
//!
//! | Mode | What it does |
//! |---|---|
//! | `echo:<text>` | ok outcome with `<text>` |
//! | `clock` | two `clock.monotonic-now` reads (an asynchronous host import); ok `monotonic ok` when the second is not smaller |
//! | `spin` | an endless CPU loop that calls no import, for the epoch-deadline and fuel tests |
//! | `cooperative` | a loop that checks `control.cancelled()` each step and returns the `cancelled` status once it is true |
//! | `stream:<script>` | `process.spawn` `<script>`, drains the resource to `exited`, and returns ok with the collected output and the exit status — or the `cancelled` status for `exited(cancelled)` |
//! | `stream-drop:<script>` | spawns, takes one event, drops the resource while the process may still run, ok `dropped` |
//! | `trap-after-terminal:<script>` | spawns, drains to `none`, then calls `next` once more: the host traps that call (`process.wit`) |
//! | `trap` | a guest trap: the wasm `unreachable` |
//! | anything else | an error outcome naming the known modes |
//!
//! `describe` has one mode of its own, `describe-import`: it reads `clock.monotonic-now`, so a
//! runtime can prove that an import called on the restricted path traps during `describe`.
//! Every other mode describes from its input alone.
//!
//! The module imports no `wasi:` interface of its own: it never prints, never reads the
//! environment and never touches a file. The `wasi:` imports in the built component come from
//! Rust std, which the `wasm32-wasip2` target links (open question S0-Q9; the build writes the
//! component's full import list to `<package>.imports`).
#![forbid(unsafe_code)]

mod wire;

use p1_bindings_tool::generated::p1::module::process::{self, ExitStatus, ProcessEvent};
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::{clock, control};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};

/// The model-facing tool name of the declaration.
const TOOL_NAME: &str = "fixture";

/// What the model is told this tool does; the modes are the module's own contract.
const TOOL_DESCRIPTION: &str = "Test fixture tool for the p1 module runtime: the input text picks one of the fixture's documented modes (echo:<text>, clock, spin, cooperative, stream:<script>, stream-drop:<script>, trap-after-terminal:<script>, trap).";

/// How long each process mode lets its command run before the host kills it (milliseconds).
const PROCESS_TIMEOUT_MS: u64 = 30_000;

/// What an unreadable or unknown mode is told, with the list a caller can act on.
const KNOWN_MODES: &str = "known modes: echo:<text>, clock, spin, cooperative, stream:<script>, stream-drop:<script>, trap-after-terminal:<script>, trap";

struct Fixture;

impl Guest for Fixture {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: TOOL_NAME.to_owned(),
            description: TOOL_DESCRIPTION.to_owned(),
            // A freeform tool: its input arrives as text, which is what the modes read.
            kind: DeclarationKind::Freeform(None),
        }
    }

    fn effect(call: ToolCall) -> CallEffect {
        // The worst case of this fixture is a call that runs a script, and only a script mode
        // asks for one; everything else, unknown input included, is read-only.
        if executes(head(&call).as_deref()) {
            CallEffect::Executes
        } else {
            CallEffect::ReadOnly
        }
    }

    fn describe(call: ToolCall) -> CallDescription {
        let mode = head(&call);
        if mode.as_deref() == Some("describe-import") {
            // The one describe mode that reaches for a capability, so that a runtime can prove
            // an import called on the restricted path traps. The reading itself is not part of
            // the answer, so it is consumed through `black_box`: the call is the point.
            let _ = std::hint::black_box(clock::monotonic_now());
        }
        let destructive = executes(mode.as_deref());
        wire::call_description(verb(mode.as_deref()), mode.as_deref(), destructive)
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        // A `tool_result` item's `content` is exactly what the model was shown, so its first
        // line is what the UI shows for the result.
        let content = wire::string_field(&tool_result, "content").unwrap_or_default();
        let first = content.lines().next().unwrap_or("").trim();
        wire::result_description(first)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let raw = match wire::string_field(&call, "raw") {
            Some(raw) => raw,
            None => return wire::error_outcome(&format!("no text input; {KNOWN_MODES}")),
        };
        let mode = raw.lines().next().unwrap_or("").trim();
        if let Some(text) = mode.strip_prefix("echo:") {
            return wire::ok_outcome(text);
        }
        if let Some(script) = mode.strip_prefix("stream:") {
            return stream(script);
        }
        if let Some(script) = mode.strip_prefix("stream-drop:") {
            return stream_drop(script);
        }
        if let Some(script) = mode.strip_prefix("trap-after-terminal:") {
            return trap_after_terminal(script);
        }
        match mode {
            "clock" => clock_reads(),
            "spin" => spin(),
            "cooperative" => cooperative(),
            "trap" => trap(),
            other => wire::error_outcome(&format!("unknown mode {other:?}; {KNOWN_MODES}")),
        }
    }
}

p1_bindings_tool::generated::export!(Fixture);

/// The first line of the input text, trimmed, or `None` when there is no input.
fn head(call: &str) -> Option<String> {
    let raw = wire::string_field(call, "raw")?;
    let line = raw.lines().next().unwrap_or("").trim();
    if line.is_empty() {
        None
    } else {
        Some(line.to_owned())
    }
}

/// Whether the mode runs a command: the streaming modes and the trap-after-terminal mode.
fn executes(mode: Option<&str>) -> bool {
    mode.is_some_and(|mode| mode.starts_with("stream") || mode.starts_with("trap-after-terminal:"))
}

/// The description verb: `run` for a call that runs a command, `call` for every other.
fn verb(mode: Option<&str>) -> &'static str {
    if executes(mode) { "run" } else { "call" }
}

/// `clock`: two reads of the host's monotonic clock, which must not go backwards.
fn clock_reads() -> ToolOutcome {
    let first = clock::monotonic_now();
    let second = clock::monotonic_now();
    if second < first {
        wire::error_outcome(&format!(
            "the monotonic clock went backwards: {first} then {second}"
        ))
    } else {
        wire::ok_outcome("monotonic ok")
    }
}

/// `spin`: an endless CPU loop with no import, for the host's epoch-deadline and fuel tests.
/// Every step is opaque to the optimiser, so the loop cannot be folded away.
fn spin() -> ToolOutcome {
    let mut step: u64 = 0;
    loop {
        step = std::hint::black_box(step.wrapping_add(1));
    }
}

/// `cooperative`: a CPU loop that checks `control.cancelled()` each step and returns the
/// `cancelled` status as soon as the host says the call is cancelled.
fn cooperative() -> ToolOutcome {
    let mut step: u64 = 0;
    while !control::cancelled() {
        step = std::hint::black_box(step.wrapping_add(1));
    }
    wire::cancelled_outcome()
}

/// `stream:<script>`: run the script, drain the resource to its terminal event and return the
/// collected output plus the exit status; a process the host cancelled for the call ends as
/// the `cancelled` status instead.
fn stream(script: &str) -> ToolOutcome {
    let running = match spawn(script) {
        Ok(running) => running,
        Err(error) => return wire::error_outcome(&error),
    };
    let mut output: Vec<u8> = Vec::new();
    loop {
        match running.next() {
            Some(ProcessEvent::Output(chunk)) => output.extend_from_slice(&chunk),
            Some(ProcessEvent::Exited(status)) => {
                if matches!(status, ExitStatus::Cancelled) {
                    return wire::cancelled_outcome();
                }
                return wire::ok_outcome(&format!(
                    "{}\nexit: {}",
                    String::from_utf8_lossy(&output),
                    exit_status_text(&status)
                ));
            }
            None => return wire::error_outcome("the process stream ended without an exit event"),
        }
    }
}

/// `stream-drop:<script>`: spawn, take one event, then drop the resource while the process may
/// still run. Dropping it is what ends the process group (`process.wit`).
fn stream_drop(script: &str) -> ToolOutcome {
    let running = match spawn(script) {
        Ok(running) => running,
        Err(error) => return wire::error_outcome(&error),
    };
    let _first = running.next();
    drop(running);
    wire::ok_outcome("dropped")
}

/// `trap-after-terminal:<script>`: drain the resource to `none` and call `next` once more. The
/// host traps that call, so the return below is reached only if the host did not.
fn trap_after_terminal(script: &str) -> ToolOutcome {
    let running = match spawn(script) {
        Ok(running) => running,
        Err(error) => return wire::error_outcome(&error),
    };
    while running.next().is_some() {}
    let after = running.next();
    wire::error_outcome(&format!(
        "the host did not trap next() after the terminal event; it returned {}",
        if after.is_some() { "an event" } else { "none" }
    ))
}

/// `trap`: the wasm `unreachable` trap. With `panic = "abort"` (the release profile of every
/// module) a guest panic is that same trap, which the host maps through `ModuleFailure`.
fn trap() -> ToolOutcome {
    unreachable!("the fixture's trap mode")
}

/// Starts `script` with the fixture's timeout, naming the script when the host refuses it.
fn spawn(script: &str) -> Result<process::Running, String> {
    process::spawn(&process::Command {
        script: script.to_owned(),
        timeout_ms: PROCESS_TIMEOUT_MS,
    })
    .map_err(|error| format!("process.spawn refused {script:?}: {error}"))
}

/// How an exit status reads in the `stream:` answer.
fn exit_status_text(status: &ExitStatus) -> String {
    match status {
        ExitStatus::Code(code) => format!("code({code})"),
        ExitStatus::Signal(signal) => format!("signal({signal})"),
        ExitStatus::UnknownSignal => "unknown-signal".to_owned(),
        ExitStatus::TimedOut => "timed-out".to_owned(),
        ExitStatus::Cancelled => "cancelled".to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A wire tool call with `raw` as its text input, spelled as the host serializes one.
    fn call(raw: &str) -> ToolCall {
        format!(
            "{{\"call_id\":\"c1\",\"name\":\"fixture\",\"input\":{{\"kind\":\"text\",\"raw\":{}}}}}",
            wire::json_text(raw)
        )
    }

    #[test]
    fn the_first_line_picks_the_mode() {
        assert_eq!(head(&call("clock")).as_deref(), Some("clock"));
        // Later lines and surrounding whitespace do not change the mode.
        assert_eq!(head(&call("echo:hi\nmore")).as_deref(), Some("echo:hi"));
        assert_eq!(head(&call("  spin  \n")).as_deref(), Some("spin"));
        // An empty input or an item without the field has no mode.
        assert_eq!(head(&call("")), None);
        assert_eq!(head("{}"), None);
        assert_eq!(head("{\"item\":\"tool_result\",\"content\":\"x\"}"), None);
    }

    #[test]
    fn only_the_command_modes_execute() {
        assert!(executes(Some("stream:true")));
        assert!(executes(Some("stream-drop:sleep 9")));
        assert!(executes(Some("trap-after-terminal:true")));
        assert!(!executes(Some("echo:rm -rf /")));
        assert!(!executes(Some("spin")));
        assert!(!executes(Some("trap")));
        assert!(!executes(Some("describe-import")));
        assert!(!executes(None));
        assert_eq!(verb(Some("stream:true")), "run");
        assert_eq!(verb(Some("clock")), "call");
    }

    #[test]
    fn an_exit_status_reads_as_its_wire_name() {
        assert_eq!(exit_status_text(&ExitStatus::Code(7)), "code(7)");
        assert_eq!(exit_status_text(&ExitStatus::Signal(9)), "signal(9)");
        assert_eq!(
            exit_status_text(&ExitStatus::UnknownSignal),
            "unknown-signal"
        );
        assert_eq!(exit_status_text(&ExitStatus::TimedOut), "timed-out");
        assert_eq!(exit_status_text(&ExitStatus::Cancelled), "cancelled");
    }
}
