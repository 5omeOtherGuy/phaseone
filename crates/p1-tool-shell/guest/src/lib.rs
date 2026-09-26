//! The guest behaviour of the `shell` tool, shared by its two hosts.
//!
//! The `p1/shell` component (`modules/p1-module-shell/`) and the native adapter
//! (`ShellTool` in `p1-tool-shell`) both run this code, so the frozen native tests prove
//! exactly what the component ships. It is what the tool decides on its own: input parsing
//! and validation, the declaration, destructiveness classification, the output filters,
//! result formatting and the description of a result. How a command runs — the sandbox,
//! the environment, the process group, output capture and cancellation — is the native
//! process service's, and reaches this crate only as bytes and an [`End`].
//!
//! Pure computation (programme decision D-XO-8): no filesystem, process, clock or
//! environment access, and no dependency beyond `serde`, `serde_json` and `regex`, so the
//! same source builds for the host and for `wasm32-unknown-unknown`. Contract values are this
//! crate's own small types; each host converts them to its wire or `p1_contracts` form.
#![forbid(unsafe_code)]

mod destructive;
mod filter;

use std::borrow::Cow;
use std::path::Path;

use serde::Deserialize;

/// The model-facing name of the default face.
pub const NAME: &str = "shell";
/// The model-facing description of the default face.
pub const DESCRIPTION: &str = "Run a shell command with `bash -lc` from the workspace root, with stdin closed.\nstdout and stderr are captured together; the last line reports the exit code. Non-zero exits are not tool errors.\nSet `timeout_seconds` for long commands; on timeout or cancellation the whole process group is killed.\nThe output of a recognised command (`cargo test`/`build`/`check`/`clippy`, `git status`/`log`/`diff`, `npm`/`pnpm` test) is summarised unless `raw: true` is passed.";
const DEFAULT_TIMEOUT_SECONDS: i64 = 120;
const MIN_TIMEOUT_SECONDS: i64 = 1;
const MAX_TIMEOUT_SECONDS: i64 = 3_600;
const MAX_OUTPUT_BYTES: usize = 50_000;
const FOOTER_RESERVE: usize = 2_000;

/// Last line of a summarised result, before the exit-code footer. The raw log
/// stays one call away, which is what makes summarising safe.
const FILTERED_MARKER: &str = "[output filtered; pass raw:true for the full log]";

/// The JSON Schema of the input, the `function` declaration's payload.
pub fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "command": {
                "type": "string",
                "description": "Command line, run with `bash -lc` from the workspace root."
            },
            "timeout_seconds": {
                "type": "integer",
                "minimum": 1,
                "maximum": 3600,
                "default": 120,
                "description": "Seconds before the command and its process group are killed."
            },
            "raw": {
                "type": "boolean",
                "default": false,
                "description": "Return the full, unfiltered output instead of the summary."
            }
        },
        "required": ["command"],
        "additionalProperties": false
    })
}

/// A call's raw input as the model produced it.
#[derive(Debug, Clone, Copy)]
pub enum RawInput<'a> {
    /// A function call's arguments, JSON text that may be invalid.
    Json(&'a str),
    /// Freeform text, which this tool does not accept.
    Text(&'a str),
}

/// A validated call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ShellInput {
    /// The command line for `bash -lc`.
    pub command: String,
    #[serde(default)]
    timeout_seconds: Option<i64>,
    /// Skip the structured output filter: the model asked for the full log.
    #[serde(default)]
    pub raw: bool,
}

impl ShellInput {
    /// The time limit in seconds, the default when the call named none. Validation keeps
    /// it within 1..=3600, so it is never zero.
    pub fn timeout_seconds(&self) -> u64 {
        self.timeout_seconds
            .unwrap_or(DEFAULT_TIMEOUT_SECONDS)
            .unsigned_abs()
    }
}

/// Parse and validate a call's input. `tool` is the model-facing name the error names.
pub fn parse_input(tool: &str, input: RawInput<'_>) -> Result<ShellInput, String> {
    let raw = match input {
        RawInput::Json(raw) => raw,
        RawInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    let input: ShellInput =
        serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
    if matches!(
        input.timeout_seconds,
        Some(seconds) if !(MIN_TIMEOUT_SECONDS..=MAX_TIMEOUT_SECONDS).contains(&seconds)
    ) {
        return Err(invalid(
            tool,
            "`timeout_seconds` must be between 1 and 3600",
        ));
    }
    Ok(input)
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// What a call is about (ADR-0057), before it runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallSummary {
    /// Always `run`.
    pub verb: &'static str,
    /// The command's first line, trimmed to 80 characters; absent for invalid input.
    pub target: Option<String>,
    /// Whether the call may destroy data.
    pub destructive: bool,
}

/// Describe a call from its input alone. `workspace` is the root when the host knows it;
/// without it the classification is more cautious (see `destructive`).
pub fn describe(input: RawInput<'_>, workspace: Option<&Path>) -> CallSummary {
    let input = parse_input(NAME, input).ok();
    CallSummary {
        verb: "run",
        target: input.as_ref().map(|input| {
            let first = input.command.lines().next().unwrap_or_default().trim();
            first.chars().take(80).collect()
        }),
        // Invalid input is classified at the tool's worst case, as required
        // by the Tool contract. Execution will still return the input error.
        destructive: input
            .as_ref()
            .is_none_or(|input| destructive::is_destructive(&input.command, workspace)),
    }
}

/// How a call ended, as the model is told.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Error,
    Cancelled,
}

/// A call's result: `content` is exactly what the model sees.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub content: String,
}

impl Outcome {
    /// An error the model reads and can act on: invalid input, or a command that could not
    /// be started or observed (the message is the process service's own).
    pub fn error(message: impl Into<String>) -> Self {
        Self {
            status: Status::Error,
            content: message.into(),
        }
    }

    /// A call cancelled before any command started: nothing to report.
    pub fn cancelled_before_start() -> Self {
        Self {
            status: Status::Cancelled,
            content: String::new(),
        }
    }
}

/// How a started command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum End {
    /// The shell exited with this code.
    Exited(i32),
    /// Killed because the call was cancelled.
    Cancelled,
    /// Killed at the time limit.
    TimedOut,
    /// Ended by this signal.
    TerminatedBySignal(i32),
    /// Ended by a signal the process service could not name.
    TerminatedByUnknownSignal,
}

/// The model-visible result of a started command: the captured output plus the footer for
/// how it ended. `timeout_seconds` is only what a timed-out footer reports.
pub fn finished(
    output: &[u8],
    end: End,
    command: &str,
    timeout_seconds: u64,
    raw: bool,
) -> Outcome {
    match end {
        End::Exited(code) => {
            // The seam: the filter only ever sees a COMPLETED command, and
            // `exit_ok` is true only for exit code 0.
            let filter = Filter {
                command,
                raw,
                exit_ok: code == 0,
            };
            render(
                output,
                &format!("[exit code: {code}]"),
                Status::Ok,
                Some(filter),
            )
        }
        // A cancelled or timed-out command is an incomplete run with no exit
        // status, and a signalled one was killed before it could exit: their
        // output is never summarised.
        End::Cancelled => render(output, "[cancelled]", Status::Cancelled, None),
        End::TimedOut => render(
            output,
            &format!("[timed out after {timeout_seconds} s]"),
            Status::Error,
            None,
        ),
        End::TerminatedBySignal(signal) => render(
            output,
            &format!("[terminated by signal {signal}]"),
            Status::Error,
            None,
        ),
        End::TerminatedByUnknownSignal => render(
            output,
            "[terminated by an unknown signal]",
            Status::Error,
            None,
        ),
    }
}

/// A completed command whose output may be summarised: what ran, whether the
/// model asked for the full log, and whether it exited 0. Absent for an
/// incomplete run (cancellation, timeout, a signal) — there is no complete
/// output to summarise then.
#[derive(Clone, Copy)]
struct Filter<'a> {
    command: &'a str,
    raw: bool,
    exit_ok: bool,
}

impl Filter<'_> {
    /// The model-visible body: the summary plus its marker line when a filter
    /// applied, the raw body when none did (unrecognised command, decline,
    /// panic, no reduction) or `raw: true` was passed.
    fn apply<'a>(&self, body: &'a str) -> Cow<'a, str> {
        if self.raw {
            return Cow::Borrowed(body);
        }
        match filter::filter_output(self.command, body, self.exit_ok) {
            Some(summary) => Cow::Owned(format!("{summary}\n{FILTERED_MARKER}")),
            None => Cow::Borrowed(body),
        }
    }
}

/// Render captured output plus a footer as the model-visible content.
fn render(bytes: &[u8], footer: &str, status: Status, filter: Option<Filter<'_>>) -> Outcome {
    let text = String::from_utf8_lossy(bytes);
    // The footer goes on its own line without an extra blank line after the
    // command's usual trailing newline.
    let body = text.trim_end_matches('\n');
    // The structured filter runs BEFORE the byte bound: a summary is the
    // smaller, more useful body to squeeze when it is still too long.
    let body = match filter {
        Some(filter) => filter.apply(body),
        None => Cow::Borrowed(body),
    };
    // Lossy decoding can TRIPLE the size of binary output (each bad byte becomes
    // U+FFFD), pushing already-capped bytes past the content bound. Squeeze the body
    // — head and tail kept, like the collector — and never bound the footer: the exit
    // code must survive however noisy the output was.
    let body = squeeze(&body, MAX_OUTPUT_BYTES - FOOTER_RESERVE);
    let content = if body.is_empty() {
        footer.to_string()
    } else {
        format!("{body}\n{footer}")
    };
    Outcome { status, content }
}

/// Keep the first and last halves of `text` (cut on char boundaries) when it exceeds `max`.
fn squeeze(text: &str, max: usize) -> Cow<'_, str> {
    if text.len() <= max {
        return Cow::Borrowed(text);
    }
    let half = max / 2;
    let mut head_end = half;
    while !text.is_char_boundary(head_end) {
        head_end -= 1;
    }
    let mut tail_start = text.len() - half;
    while !text.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    Cow::Owned(format!(
        "{}\n[… {} bytes omitted …]\n{}",
        &text[..head_end],
        tail_start - head_end,
        &text[tail_start..]
    ))
}

/// What the UI shows for a result: a one-line summary and the command's detail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    /// The exit code from an `[exit code: N]` footer; absent when the command did not exit.
    pub exit_code: Option<i32>,
    /// The output lines without the footer.
    pub tail: Vec<String>,
}

/// Describe a result from the content the model was shown. `ok` is whether the call's
/// status was `ok`.
pub fn describe_result(content: &str, ok: bool) -> ResultSummary {
    let mut lines: Vec<&str> = content.lines().collect();
    let exit_code = lines.last().and_then(|line| {
        line.strip_prefix("[exit code: ")
            .and_then(|code| code.strip_suffix(']'))
            .and_then(|code| code.parse().ok())
    });
    // Shell terminal lines are framing, not command output. Only an exit
    // footer carries an exit code, but timeout/cancellation/signal footers
    // must not leak into the command tail either.
    if lines.last().is_some_and(|line| {
        exit_code.is_some()
            || matches!(*line, "[cancelled]" | "[terminated by an unknown signal]")
            || line.starts_with("[timed out after ")
            || line.starts_with("[terminated by signal ")
    }) {
        lines.pop();
    }
    let line_count = lines.len();
    let summary = if ok {
        match exit_code {
            Some(code) => format!("exit {code} · {line_count} lines"),
            None => format!("{line_count} lines"),
        }
    } else {
        content.lines().next().unwrap_or_default().to_string()
    };
    ResultSummary {
        summary,
        exit_code,
        tail: lines.into_iter().map(str::to_string).collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn json(command: &str) -> String {
        serde_json::json!({ "command": command }).to_string()
    }

    #[test]
    fn an_exit_renders_the_output_and_the_code() {
        let outcome = finished(b"hi\n", End::Exited(0), "echo hi", 120, false);
        assert_eq!(
            outcome,
            Outcome {
                status: Status::Ok,
                content: "hi\n[exit code: 0]".into()
            }
        );
        // A non-zero exit is not a tool error.
        let outcome = finished(b"", End::Exited(3), "false", 120, false);
        assert_eq!(
            outcome,
            Outcome {
                status: Status::Ok,
                content: "[exit code: 3]".into()
            }
        );
    }

    #[test]
    fn every_other_end_has_its_footer_and_status() {
        let cases = [
            (End::Cancelled, Status::Cancelled, "out\n[cancelled]"),
            (End::TimedOut, Status::Error, "out\n[timed out after 7 s]"),
            (
                End::TerminatedBySignal(15),
                Status::Error,
                "out\n[terminated by signal 15]",
            ),
            (
                End::TerminatedByUnknownSignal,
                Status::Error,
                "out\n[terminated by an unknown signal]",
            ),
        ];
        for (end, status, content) in cases {
            let outcome = finished(b"out\n", end, "cargo test", 7, false);
            assert_eq!(
                outcome,
                Outcome {
                    status,
                    content: content.into()
                },
                "{end:?}"
            );
        }
    }

    #[test]
    fn an_incomplete_run_is_never_filtered() {
        let log = "   Compiling a v0.1.0\n".repeat(50);
        let exited = finished(log.as_bytes(), End::Exited(0), "cargo build", 120, false);
        let timed_out = finished(log.as_bytes(), End::TimedOut, "cargo build", 120, false);
        assert!(exited.content.contains(FILTERED_MARKER), "{exited:?}");
        assert!(
            !timed_out.content.contains(FILTERED_MARKER),
            "{timed_out:?}"
        );
        // `raw: true` skips the filter on a completed run too.
        let raw = finished(log.as_bytes(), End::Exited(0), "cargo build", 120, true);
        assert!(!raw.content.contains(FILTERED_MARKER), "{raw:?}");
    }

    #[test]
    fn output_over_the_bound_keeps_head_tail_and_footer() {
        let output = "x".repeat(200_000);
        let outcome = finished(output.as_bytes(), End::Exited(0), "yes", 120, false);
        assert!(
            outcome.content.len() < MAX_OUTPUT_BYTES,
            "{}",
            outcome.content.len()
        );
        assert!(outcome.content.contains("bytes omitted"));
        assert!(outcome.content.ends_with("\n[exit code: 0]"));
    }

    #[test]
    fn input_is_validated_with_the_tool_name() {
        let error = parse_input("Run", RawInput::Text("echo hi")).unwrap_err();
        assert!(error.starts_with("Invalid input for Run: "), "{error}");
        let error = parse_input(
            "shell",
            RawInput::Json(r#"{"command":"x","timeout_seconds":0}"#),
        )
        .unwrap_err();
        assert_eq!(
            error,
            "Invalid input for shell: `timeout_seconds` must be between 1 and 3600"
        );
        let input = parse_input("shell", RawInput::Json(&json("true"))).unwrap();
        assert_eq!(input.timeout_seconds(), 120);
        assert!(!input.raw);
    }

    #[test]
    fn describe_is_bounded_and_worst_case_for_invalid_input() {
        let described = describe(RawInput::Json(&json("echo one\necho two")), None);
        assert_eq!(described.target.as_deref(), Some("echo one"));
        assert!(!described.destructive);
        let invalid = describe(RawInput::Json("{"), None);
        assert_eq!(
            invalid,
            CallSummary {
                verb: "run",
                target: None,
                destructive: true
            }
        );
    }

    #[test]
    fn a_result_description_drops_the_footer_from_the_tail() {
        let described = describe_result("one\ntwo\n[exit code: 7]", true);
        assert_eq!(described.summary, "exit 7 · 2 lines");
        assert_eq!(described.exit_code, Some(7));
        assert_eq!(described.tail, ["one", "two"]);
        let described = describe_result("partial\n[timed out after 1 s]", false);
        assert_eq!(described.summary, "partial");
        assert_eq!(described.exit_code, None);
        assert_eq!(described.tail, ["partial"]);
    }
}
