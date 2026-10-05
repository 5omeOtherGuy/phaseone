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
//! environment access, and no dependency beyond `serde`, `serde_json` and `regex` (the
//! vendored declarative filters are JSON embedded at compile time), so the
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
pub const DESCRIPTION: &str = "Run a shell command with `bash -lc` from the workspace root, with stdin closed.\nstdout and stderr are captured together; the last line reports the exit code. Non-zero exits are not tool errors.\nSet `timeout_seconds` for long commands; on timeout or cancellation the whole process group is killed.\nThe output of a recognised command (`cargo test`/`build`/`check`/`clippy`, `git status`/`log`/`diff`, `npm`/`pnpm` test, and the tool classes of the built-in declarative filters such as `make`, `helm`, `terraform plan` and `uv sync`) is summarised unless `raw: true` is passed.";
/// The paragraph the model reads when the host turned the sandbox on (ADR-0035: the
/// description says what the boundary is). It belongs to the side that assembled the
/// sandbox: a tool running over the process service cannot know whether it is sandboxed, so
/// whoever presents the tool appends this to the face's description.
pub const SANDBOX_PARAGRAPH: &str = "Commands run in a sandbox: only the workspace and /tmp are writable, the rest of the filesystem is read-only, and most of the home directory is not visible. Do not try to install software outside the workspace.";
/// Appended to a tool's identity variant when its commands run in the sandbox, for the
/// same reason as [`SANDBOX_PARAGRAPH`].
pub const SANDBOX_VARIANT_SUFFIX: &str = "+sandbox";
const DEFAULT_TIMEOUT_SECONDS: i64 = 120;
const MIN_TIMEOUT_SECONDS: i64 = 1;
const MAX_TIMEOUT_SECONDS: i64 = 3_600;
const MAX_OUTPUT_BYTES: usize = 50_000;
const FOOTER_RESERVE: usize = 2_000;

/// Last line of a summarised result, before the exit-code footer. The raw log
/// stays one call away, which is what makes summarising safe.
const FILTERED_MARKER: &str = "[output filtered; pass raw:true for the full log]";

/// The stored-output line when the host could not store the output (ADR-0109 item 8): no
/// handle, because the one the host lists for it names nothing.
const STORAGE_FAILED_NOTICE: &str = "[full output not stored; recovery unavailable]";
/// The host's omission line, around its dropped-byte count.
const HOST_CUT_PREFIX: &str = "[… ";
const HOST_CUT_SUFFIX: &str =
    " bytes omitted; diagnostics may be missing, including in raw output …]";
/// The start of the stored-output line that names a handle.
const STORED_NOTICE_PREFIX: &str = "[stored output: handle_id ";

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
    finished_with_store(output, end, command, timeout_seconds, raw, None)
}

/// How much of a command's output the host stored (`tool-outputs.capture`, ADR-0109).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Capture {
    /// Everything the command printed.
    Complete,
    /// The store stopped at its byte cap; what it holds is exact up to there.
    StoredCapReached,
    /// The store fell behind and stopped; what it holds is exact up to there.
    StorageIncomplete,
    /// Nothing is recoverable, and the handle names nothing.
    StorageFailed,
}

/// The host's stored copy of a command's output (`tool-outputs.output-info`, ADR-0109).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredOutput {
    /// The opaque handle `read_output` pages it by.
    pub handle: String,
    /// The stored bytes.
    pub stored_bytes: u64,
    /// How much of the output that is.
    pub capture: Capture,
}

impl StoredOutput {
    /// The one line that points the model at the stored output (ADR-0109 item 8): the handle and
    /// how much is stored, or that nothing can be recovered. Short and bounded: a host handle is
    /// a few dozen bytes, so the line fits the footer reserve beside any end footer.
    fn notice(&self) -> String {
        let state = match self.capture {
            Capture::Complete => "complete",
            Capture::StoredCapReached => "stopped at the store's cap, later output not stored",
            Capture::StorageIncomplete => {
                "incomplete, the store fell behind and later output was not stored"
            }
            Capture::StorageFailed => return STORAGE_FAILED_NOTICE.to_owned(),
        };
        format!(
            "[stored output: handle_id {}, {} bytes, {state}; page it with read_output]",
            self.handle, self.stored_bytes
        )
    }
}

/// [`finished`] for a command whose output the host stored: when the result shows less than
/// the command printed (a filter summarised it, or the host or the byte bound cut it), one line
/// before the end footer names the stored output, or says it could not be stored.
pub fn finished_with_store(
    output: &[u8],
    end: End,
    command: &str,
    timeout_seconds: u64,
    raw: bool,
    stored: Option<&StoredOutput>,
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
                stored,
            )
        }
        // A cancelled or timed-out command is an incomplete run with no exit
        // status, and a signalled one was killed before it could exit: their
        // output is never summarised.
        End::Cancelled => render(output, "[cancelled]", Status::Cancelled, None, stored),
        End::TimedOut => render(
            output,
            &format!("[timed out after {timeout_seconds} s]"),
            Status::Error,
            None,
            stored,
        ),
        End::TerminatedBySignal(signal) => render(
            output,
            &format!("[terminated by signal {signal}]"),
            Status::Error,
            None,
            stored,
        ),
        End::TerminatedByUnknownSignal => render(
            output,
            "[terminated by an unknown signal]",
            Status::Error,
            None,
            stored,
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

/// Render captured output plus a footer as the model-visible content. With a stored copy of
/// the output, a result that shows less than the command printed carries the stored-output line
/// just above the footer; a result that shows everything needs none.
fn render(
    bytes: &[u8],
    footer: &str,
    status: Status,
    filter: Option<Filter<'_>>,
    stored: Option<&StoredOutput>,
) -> Outcome {
    let text = String::from_utf8_lossy(bytes);
    // The footer goes on its own line without an extra blank line after the
    // command's usual trailing newline.
    let body = text.trim_end_matches('\n');
    // The structured filter runs BEFORE the byte bound: a summary is the
    // smaller, more useful body to squeeze when it is still too long.
    let raw_body = body;
    let mut body = match filter {
        Some(filter) => filter.apply(body),
        None => Cow::Borrowed(body),
    };
    // A smaller parser body is not enough: every notice added by filtering
    // consumes model-visible bytes too. The exit footer is common framing.
    if matches!(body, Cow::Owned(_)) {
        let notice_bytes = stored.map_or(0, |stored| stored.notice().len() + 1);
        if body.len().saturating_add(notice_bytes) >= raw_body.len() {
            body = Cow::Borrowed(raw_body);
        }
    }
    let filtered = matches!(body, Cow::Owned(_));
    // Lossy decoding can TRIPLE the size of binary output (each bad byte becomes
    // U+FFFD), pushing already-capped bytes past the content bound. Squeeze the body
    // — head and tail kept, like the collector — and never bound the footer: the exit
    // code must survive however noisy the output was.
    let squeezed = squeeze(&body, MAX_OUTPUT_BYTES - FOOTER_RESERVE);
    let cut = matches!(squeezed, Cow::Owned(_));
    // Loss is decided from explicit signals only, never from byte counts: the host's head/tail
    // cut is line-based and its marker can outweigh what it dropped (#527 review). The host
    // marks its cut with one omission line; a store that stopped early cannot say how much the
    // command printed (at the default cap far more than any result shows), so it counts too.
    let shortened = host_cut(&text)
        || stored.is_some_and(|stored| {
            matches!(
                stored.capture,
                Capture::StoredCapReached | Capture::StorageIncomplete
            )
        });
    let footer = match stored {
        Some(stored) if filtered || cut || shortened => format!("{}\n{footer}", stored.notice()),
        _ => footer.to_owned(),
    };
    let content = if squeezed.is_empty() {
        footer
    } else {
        format!("{squeezed}\n{footer}")
    };
    Outcome { status, content }
}

/// Whether the host's process capture dropped the middle of the output: it then puts one
/// omission line in its place (`crates/p1-module-runtime/src/process/mod.rs`, `take_rest`). The
/// line crosses the frozen `process` interface as output text, the one signal the guest gets.
/// Output that merely prints such a line only adds a handle line to its result.
fn host_cut(text: &str) -> bool {
    text.lines().any(|line| {
        line.strip_prefix(HOST_CUT_PREFIX)
            .and_then(|rest| rest.strip_suffix(HOST_CUT_SUFFIX))
            .is_some_and(|count| !count.is_empty() && count.bytes().all(|b| b.is_ascii_digit()))
    })
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
        // The stored-output line sits just above the end footer and is framing too.
        if lines.last().is_some_and(|line| {
            *line == STORAGE_FAILED_NOTICE || line.starts_with(STORED_NOTICE_PREFIX)
        }) {
            lines.pop();
        }
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

    fn stored(capture: Capture, stored_bytes: u64) -> StoredOutput {
        StoredOutput {
            handle: format!("out-{}", "a".repeat(32)),
            stored_bytes,
            capture,
        }
    }

    /// ADR-0109 item 8: a result that shows less than the command printed names the stored
    /// output and its state on the line above the end footer, filtered or raw, and inside the
    /// 50,000-byte envelope however long the output was.
    #[test]
    fn a_cut_result_names_the_stored_output_in_each_state() {
        let long = "x".repeat(200_000);
        let handle = format!("out-{}", "a".repeat(32));
        for (capture, words) in [
            (Capture::Complete, "complete"),
            (
                Capture::StoredCapReached,
                "stopped at the store's cap, later output not stored",
            ),
            (
                Capture::StorageIncomplete,
                "incomplete, the store fell behind and later output was not stored",
            ),
        ] {
            for raw in [false, true] {
                let copy = stored(capture, 200_000);
                let outcome = finished_with_store(
                    long.as_bytes(),
                    End::Exited(1),
                    "yes",
                    120,
                    raw,
                    Some(&copy),
                );
                let expected = format!(
                    "\n[stored output: handle_id {handle}, 200000 bytes, {words}; page it with read_output]\n[exit code: 1]"
                );
                assert!(
                    outcome.content.ends_with(&expected),
                    "{capture:?} raw={raw}"
                );
                assert!(
                    outcome.content.len() <= MAX_OUTPUT_BYTES,
                    "{}",
                    outcome.content.len()
                );
            }
        }
        let failed = stored(Capture::StorageFailed, 0);
        for raw in [false, true] {
            let outcome = finished_with_store(
                long.as_bytes(),
                End::Exited(0),
                "yes",
                120,
                raw,
                Some(&failed),
            );
            assert!(
                outcome
                    .content
                    .ends_with("\n[full output not stored; recovery unavailable]\n[exit code: 0]"),
                "raw={raw}"
            );
            assert!(!outcome.content.contains("out-"), "no handle is shown");
        }
    }

    #[test]
    fn a_filtered_result_names_the_stored_output() {
        let log = "   Compiling a v0.1.0\n".repeat(50);
        let copy = stored(Capture::Complete, log.len() as u64);
        let outcome = finished_with_store(
            log.as_bytes(),
            End::Exited(0),
            "cargo build",
            120,
            false,
            Some(&copy),
        );
        assert!(outcome.content.contains(FILTERED_MARKER), "{outcome:?}");
        assert!(
            outcome.content.contains(&format!(
                "{FILTERED_MARKER}\n[stored output: handle_id out-"
            )),
            "{outcome:?}"
        );
        // The same output raw is shown whole: nothing to recover, no line.
        let raw = finished_with_store(
            log.as_bytes(),
            End::Exited(0),
            "cargo build",
            120,
            true,
            Some(&copy),
        );
        assert!(!raw.content.contains("[stored output:"), "{raw:?}");
        assert_eq!(
            raw,
            finished(log.as_bytes(), End::Exited(0), "cargo build", 120, true)
        );
    }

    #[test]
    fn a_host_cut_names_the_stored_output_and_ends_keep_their_footer() {
        // The host kept head and tail of a longer output and marked the cut.
        let copy = stored(Capture::Complete, 1_000_000);
        let received = "head\n\n[… 99 bytes omitted; diagnostics may be missing, including in raw output …]\ntail\n";
        for (end, footer) in [
            (End::TimedOut, "[timed out after 9 s]"),
            (End::Cancelled, "[cancelled]"),
            (End::TerminatedBySignal(9), "[terminated by signal 9]"),
        ] {
            let outcome =
                finished_with_store(received.as_bytes(), end, "make", 9, false, Some(&copy));
            assert!(
                outcome
                    .content
                    .contains("…]\ntail\n[stored output: handle_id out-"),
                "{outcome:?}"
            );
            assert!(
                outcome.content.ends_with(&format!("\n{footer}")),
                "{outcome:?}"
            );
        }
    }

    /// #527 review: the host's line-based cut drops `FAIL` while its marker adds more bytes than
    /// it dropped, so the stored bytes are FEWER than the bytes received; the marker alone says
    /// the result was cut, raw or not.
    #[test]
    fn a_host_cut_smaller_than_its_marker_still_names_the_stored_output() {
        let head = "x\n".repeat(990);
        let received = format!(
            "{head}\n[… 5 bytes omitted; diagnostics may be missing, including in raw output …]\n{head}"
        );
        let copy = stored(Capture::Complete, (head.len() * 2 + 5) as u64);
        assert!(copy.stored_bytes < received.len() as u64);
        for raw in [false, true] {
            let outcome = finished_with_store(
                received.as_bytes(),
                End::Exited(1),
                "sh",
                120,
                raw,
                Some(&copy),
            );
            assert!(
                outcome.content.contains("\n[stored output: handle_id out-"),
                "raw={raw}"
            );
        }
        // Byte counts alone never decide: more stored than received, nothing marked, is whole.
        let more = stored(Capture::Complete, 1_000);
        let outcome =
            finished_with_store(b"hi\n", End::Exited(0), "echo hi", 120, true, Some(&more));
        assert_eq!(outcome.content, "hi\n[exit code: 0]");
    }

    #[test]
    fn a_store_that_stopped_early_is_named_whatever_arrived() {
        // A cap smaller than what the host kept: the stored bytes cannot show the cut.
        let copy = stored(Capture::StoredCapReached, 2);
        let outcome =
            finished_with_store(b"hi\n", End::Exited(0), "echo hi", 120, true, Some(&copy));
        assert!(
            outcome
                .content
                .starts_with("hi\n[stored output: handle_id out-"),
            "{outcome:?}"
        );
    }

    #[test]
    fn a_whole_result_carries_no_stored_output_line() {
        let copy = stored(Capture::Complete, 3);
        let outcome =
            finished_with_store(b"hi\n", End::Exited(0), "echo hi", 120, false, Some(&copy));
        assert_eq!(outcome.content, "hi\n[exit code: 0]");
    }

    #[test]
    fn the_stored_output_line_is_framing_in_a_result_description() {
        let described = describe_result(
            "one\n[stored output: handle_id out-1, 9 bytes, complete; page it with read_output]\n[exit code: 2]",
            true,
        );
        assert_eq!(described.summary, "exit 2 · 1 lines");
        assert_eq!(described.tail, ["one"]);
        let described = describe_result(
            "one\n[full output not stored; recovery unavailable]\n[exit code: 0]",
            true,
        );
        assert_eq!(described.tail, ["one"]);
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
