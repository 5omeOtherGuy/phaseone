//! The Codex faces of `exec_command` and `write_stdin` (ADR-0219): what the model is told and
//! shown, byte for byte as the Codex CLI tells and shows it, over p1's process capability.
//!
//! Copied from openai/codex at tag `rust-v0.162.1` (commit 092d3acd), Apache-2.0
//! (`docs/licenses/openai-codex/`): the schemas and descriptions of
//! `codex-rs/core/src/tools/handlers/shell_spec.rs`, the argument defaults of
//! `core/src/tools/handlers/unified_exec.rs`, the response text of
//! `core/src/tools/context.rs` (`ExecCommandToolOutput`), the clamps of
//! `core/src/unified_exec/mod.rs`, the error texts of `core/src/unified_exec/errors.rs` and
//! the middle truncation of `utils/string/src/truncate.rs` and
//! `utils/output-truncation/src/lib.rs`.
//!
//! What p1 cannot do, this crate says plainly in `docs/design/tools.md`: there is no PTY and
//! no stdin on a p1 job, one shell runs every command, and an escalation has no approval path
//! (#761). The fields exist so the model's calls parse exactly as Codex parses them.
#![forbid(unsafe_code)]

use serde::Deserialize;
use serde_json::{Value, json};

/// Bytes per token, the approximation Codex truncates with.
const APPROX_BYTES_PER_TOKEN: usize = 4;
/// The model's output policy: the truncation every bundled Codex model carries (10 000 tokens).
pub const MODEL_POLICY_TOKENS: usize = 10_000;
/// `max_output_tokens` when the call names none.
pub const DEFAULT_MAX_OUTPUT_TOKENS: usize = 10_000;
/// The longest error text an `exec_command` failure returns.
pub const REJECTION_MAX_BYTES: usize = 900;

/// The session id Codex trained the model on is a number; p1's job ids are `j<number>`.
pub fn session_id(job_id: &str) -> Option<u64> {
    job_id.strip_prefix('j')?.parse().ok()
}

/// The job id of a session the model names.
pub fn job_id(session_id: u64) -> String {
    format!("j{session_id}")
}

pub mod exec_command {
    use super::*;

    pub const NAME: &str = "exec_command";
    pub const DESCRIPTION: &str =
        "Runs a command in a PTY, returning output or a session ID for ongoing interaction.";
    pub const DEFAULT_YIELD_TIME_MS: u64 = 10_000;
    pub const MIN_YIELD_TIME_MS: u64 = 250;
    pub const MAX_YIELD_TIME_MS: u64 = 30_000;
    /// What Codex answers a `require_escalated` request under an approval policy that never
    /// asks: p1 has no escalation path (#761), so every such request is this case.
    pub const ESCALATION_REFUSED: &str = "approval policy is Never; reject command — you cannot ask for escalated permissions if the approval policy is Never";

    /// The parameter schema, properties in the alphabetical order Codex serialises them in.
    pub fn input_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "cmd": { "type": "string", "description": "Shell command to execute." },
                "justification": { "type": "string", "description": "User-facing approval question for `require_escalated`; omit otherwise." },
                "login": { "type": "boolean", "description": "True runs the shell with -l/-i semantics; false disables them. Defaults to true." },
                "max_output_tokens": { "type": "number", "description": "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy." },
                "prefix_rule": { "type": "array", "items": { "type": "string" }, "description": "Reusable approval prefix for `cmd`, only with `sandbox_permissions: \"require_escalated\"`; for example [\"git\", \"pull\"]." },
                "sandbox_permissions": { "type": "string", "enum": ["use_default", "require_escalated"], "description": "Per-command sandbox override. Defaults to `use_default`; use `require_escalated` for unsandboxed execution." },
                "shell": { "type": "string", "description": "Shell binary to launch. Defaults to the user's default shell." },
                "tty": { "type": "boolean", "description": "True allocates a PTY for the command; false or omitted uses plain pipes." },
                "workdir": { "type": "string", "description": "Working directory for the command. Defaults to the turn cwd." },
                "yield_time_ms": { "type": "number", "description": "Wait before yielding output. Defaults to 10000 ms; effective range is 250-30000 ms." }
            },
            "required": ["cmd"],
            "additionalProperties": false
        })
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
    #[serde(rename_all = "snake_case")]
    pub enum SandboxPermissions {
        UseDefault,
        RequireEscalated,
    }

    /// A call's arguments as Codex reads them: unknown fields are ignored, as its
    /// `parse_arguments` ignores them, and every default is Codex's.
    #[derive(Debug, Clone, Deserialize)]
    pub struct Input {
        pub cmd: String,
        #[serde(default)]
        pub workdir: Option<String>,
        #[serde(default)]
        pub shell: Option<String>,
        #[serde(default)]
        pub login: Option<bool>,
        #[serde(default)]
        pub tty: bool,
        #[serde(default = "default_yield_time_ms")]
        pub yield_time_ms: u64,
        #[serde(default)]
        pub max_output_tokens: Option<usize>,
        #[serde(default)]
        pub sandbox_permissions: Option<SandboxPermissions>,
        #[serde(default)]
        pub justification: Option<String>,
        #[serde(default)]
        pub prefix_rule: Option<Vec<String>>,
    }

    fn default_yield_time_ms() -> u64 {
        DEFAULT_YIELD_TIME_MS
    }

    pub fn parse(raw: &str) -> Result<Input, String> {
        serde_json::from_str(raw).map_err(|e| format!("failed to parse function arguments: {e}"))
    }

    impl Input {
        /// The deadline p1 gives the command, after which the host hands it over to a session
        /// job (ADR-0123) and the response names the session: Codex's clamp of
        /// `yield_time_ms`.
        pub fn yield_time_ms(&self) -> u64 {
            self.yield_time_ms
                .clamp(MIN_YIELD_TIME_MS, MAX_YIELD_TIME_MS)
        }

        /// Whether the call asks for what p1 cannot grant.
        pub fn requests_escalation(&self) -> bool {
            self.sandbox_permissions == Some(SandboxPermissions::RequireEscalated)
        }

        /// The script p1's process capability runs with `bash -lc` from the workspace root:
        /// Codex starts the command in `workdir`, so the script changes there first and the
        /// command's own exit status is still the script's.
        pub fn script(&self) -> String {
            match self.workdir.as_deref().filter(|dir| !dir.is_empty()) {
                Some(dir) => format!(
                    "cd -- '{}' || exit 1\n{}",
                    dir.replace('\'', "'\\''"),
                    self.cmd
                ),
                None => self.cmd.clone(),
            }
        }
    }
}

pub mod write_stdin {
    use super::*;

    pub const NAME: &str = "write_stdin";
    pub const DESCRIPTION: &str =
        "Writes characters to an existing unified exec session and returns recent output.";
    pub const DEFAULT_YIELD_TIME_MS: u64 = 250;
    /// Codex's text when a session's stdin takes nothing; p1's jobs have no stdin at all.
    pub const STDIN_CLOSED: &str = "write_stdin failed: stdin is closed for this session; rerun exec_command with tty=true to keep stdin open";

    pub fn input_schema() -> Value {
        json!({
            "type": "object",
            "properties": {
                "chars": { "type": "string", "description": "Bytes to write to stdin. Defaults to empty, which polls without writing." },
                "max_output_tokens": { "type": "number", "description": "Output token budget. Defaults to 10000 tokens; larger requests may be capped by policy." },
                "session_id": { "type": "number", "description": "Identifier of the running unified exec session." },
                "yield_time_ms": { "type": "number", "description": "Wait before yielding output. Non-empty writes default to 250 ms and cap at 30000 ms; empty polls wait 5000-300000 ms by default." }
            },
            "required": ["session_id"],
            "additionalProperties": false
        })
    }

    #[derive(Debug, Clone, Deserialize)]
    pub struct Input {
        pub session_id: u64,
        #[serde(default)]
        pub chars: String,
        #[serde(default = "default_yield_time_ms")]
        pub yield_time_ms: u64,
        #[serde(default)]
        pub max_output_tokens: Option<usize>,
    }

    fn default_yield_time_ms() -> u64 {
        DEFAULT_YIELD_TIME_MS
    }

    pub fn parse(raw: &str) -> Result<Input, String> {
        serde_json::from_str(raw).map_err(|e| format!("failed to parse function arguments: {e}"))
    }

    pub fn unknown_session(session_id: u64) -> String {
        format!("write_stdin failed: Unknown process id {session_id}")
    }
}

/// One response of either tool, rendered as Codex renders `ExecCommandToolOutput`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Response<'a> {
    /// Codex's per-response chunk id; the line is omitted when empty.
    pub chunk_id: &'a str,
    pub wall_time_ms: u64,
    /// The exit code when the command finished during this call.
    pub exit_code: Option<i32>,
    /// The session the command still runs under.
    pub session_id: Option<u64>,
    pub raw_output: &'a [u8],
    pub max_output_tokens: Option<usize>,
}

impl Response<'_> {
    /// The model's budget for this response: the smaller of its request and the model policy.
    fn policy_tokens(&self) -> usize {
        let requested = self.max_output_tokens.unwrap_or(DEFAULT_MAX_OUTPUT_TOKENS);
        requested.min(MODEL_POLICY_TOKENS)
    }

    fn header(&self, original_token_count: Option<usize>) -> String {
        let mut sections = Vec::new();
        if !self.chunk_id.is_empty() {
            sections.push(format!("Chunk ID: {}", self.chunk_id));
        }
        let wall_time_seconds = self.wall_time_ms as f64 / 1_000.0;
        sections.push(format!("Wall time: {wall_time_seconds:.4} seconds"));
        if let Some(exit_code) = self.exit_code {
            sections.push(format!("Process exited with code {exit_code}"));
        }
        if let Some(session_id) = self.session_id {
            sections.push(format!("Process running with session ID {session_id}"));
        }
        if let Some(original_token_count) = original_token_count {
            sections.push(format!("Original token count: {original_token_count}"));
        }
        sections.push("Output:".to_string());
        sections.join("\n")
    }

    /// `ExecCommandToolOutput::response_text`: the header, then the output truncated to the
    /// policy, shrunk until the whole response fits the policy's serialisation allowance.
    pub fn render(&self) -> String {
        let text = String::from_utf8_lossy(self.raw_output);
        let policy = self.policy_tokens();
        let original_token_count =
            (text.len() > bytes_for_tokens(policy)).then(|| approx_token_count(&text));
        let header = self.header(original_token_count);
        let allowance = (policy as f64 * 1.2).ceil() as usize;
        let output_budget =
            bytes_for_tokens(allowance).saturating_sub(header.len().saturating_add(1));
        let mut tokens = policy;
        let mut output = formatted_truncate_text(&text, tokens);
        while output.len() > output_budget && tokens > 0 {
            let excess_bytes = output.len() - output_budget;
            tokens = tokens.saturating_sub(tokens_for_bytes(excess_bytes));
            output = formatted_truncate_text(&text, tokens);
        }
        format!("{header}\n{output}")
    }
}

/// What the model reads when a command is cancelled under it (Codex `parallel.rs`).
pub fn aborted(wall_time_ms: u64) -> String {
    format!(
        "Wall time: {:.1} seconds\naborted by user",
        wall_time_ms as f64 / 1_000.0
    )
}

/// An `exec_command` failure as Codex words it, bounded to [`REJECTION_MAX_BYTES`].
pub fn failure(error: &str) -> String {
    truncate_middle_chars(
        &format!("exec_command failed: {error}"),
        REJECTION_MAX_BYTES,
    )
}

pub fn approx_token_count(text: &str) -> usize {
    text.len()
        .saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1))
        / APPROX_BYTES_PER_TOKEN
}

fn bytes_for_tokens(tokens: usize) -> usize {
    tokens.saturating_mul(APPROX_BYTES_PER_TOKEN)
}

fn tokens_for_bytes(bytes: usize) -> usize {
    approx_token_count_of_len(bytes)
}

fn approx_token_count_of_len(len: usize) -> usize {
    len.saturating_add(APPROX_BYTES_PER_TOKEN.saturating_sub(1)) / APPROX_BYTES_PER_TOKEN
}

/// `formatted_truncate_text`: the text when it fits `max_tokens`, else the warning header and
/// the middle-truncated text.
pub fn formatted_truncate_text(content: &str, max_tokens: usize) -> String {
    if content.len() <= bytes_for_tokens(max_tokens) {
        return content.to_string();
    }
    let original_token_count = approx_token_count(content);
    let total_lines = content.lines().count();
    let result = truncate_middle_with_token_budget(content, max_tokens);
    format!(
        "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n{result}"
    )
}

/// Keep the beginning and the end of `s` within `max_tokens`, with Codex's token marker in
/// the middle.
pub fn truncate_middle_with_token_budget(s: &str, max_tokens: usize) -> String {
    if s.is_empty() {
        return String::new();
    }
    if max_tokens > 0 && s.len() <= bytes_for_tokens(max_tokens) {
        return s.to_string();
    }
    truncate_with_byte_estimate(s, bytes_for_tokens(max_tokens), true)
}

/// Keep at most `max_bytes` of `s` from its ends, with Codex's character marker in the middle.
pub fn truncate_middle_chars(s: &str, max_bytes: usize) -> String {
    truncate_with_byte_estimate(s, max_bytes, false)
}

fn truncate_with_byte_estimate(s: &str, max_bytes: usize, use_tokens: bool) -> String {
    if s.is_empty() {
        return String::new();
    }
    if max_bytes == 0 {
        let total_chars = if use_tokens { 0 } else { s.chars().count() };
        return marker(use_tokens, removed_units(use_tokens, s.len(), total_chars));
    }
    if s.len() <= max_bytes {
        return s.to_string();
    }
    let total_bytes = s.len();
    let (left_budget, right_budget) = split_budget(max_bytes);
    let (removed_chars, left, right) = if use_tokens {
        let prefix_end = floor_char_boundary(s, left_budget);
        let suffix_start = ceil_char_boundary(s, total_bytes - right_budget);
        (0, &s[..prefix_end], &s[suffix_start..])
    } else {
        split_string(s, left_budget, right_budget)
    };
    let marker = marker(
        use_tokens,
        removed_units(
            use_tokens,
            total_bytes.saturating_sub(max_bytes),
            removed_chars,
        ),
    );
    format!("{left}{marker}{right}")
}

fn split_string(s: &str, beginning_bytes: usize, end_bytes: usize) -> (usize, &str, &str) {
    let len = s.len();
    let tail_start_target = len.saturating_sub(end_bytes);
    let mut prefix_end = 0usize;
    let mut suffix_start = len;
    let mut removed_chars = 0usize;
    let mut suffix_started = false;
    for (idx, ch) in s.char_indices() {
        let char_end = idx + ch.len_utf8();
        if char_end <= beginning_bytes {
            prefix_end = char_end;
            continue;
        }
        if idx >= tail_start_target {
            if !suffix_started {
                suffix_start = idx;
                suffix_started = true;
            }
            continue;
        }
        removed_chars = removed_chars.saturating_add(1);
    }
    if suffix_start < prefix_end {
        suffix_start = prefix_end;
    }
    (removed_chars, &s[..prefix_end], &s[suffix_start..])
}

fn split_budget(budget: usize) -> (usize, usize) {
    let left = budget / 2;
    (left, budget - left)
}

fn marker(use_tokens: bool, removed_count: usize) -> String {
    if use_tokens {
        format!("…{removed_count} tokens truncated…")
    } else {
        format!("…{removed_count} chars truncated…")
    }
}

fn removed_units(use_tokens: bool, removed_bytes: usize, removed_chars: usize) -> usize {
    if use_tokens {
        approx_token_count_of_len(removed_bytes)
    } else {
        removed_chars
    }
}

fn floor_char_boundary(s: &str, index: usize) -> usize {
    let mut index = index.min(s.len());
    while !s.is_char_boundary(index) {
        index -= 1;
    }
    index
}

fn ceil_char_boundary(s: &str, index: usize) -> usize {
    let mut index = index.min(s.len());
    while !s.is_char_boundary(index) {
        index += 1;
    }
    index
}

/// The wire values both components speak with the host.
pub mod wire {
    use serde::{Deserialize, Serialize};

    #[derive(Deserialize)]
    pub struct Call {
        pub name: String,
        pub input: Input,
    }

    #[derive(Deserialize)]
    #[serde(tag = "kind", content = "raw", rename_all = "snake_case")]
    pub enum Input {
        Json(String),
        Text(String),
    }

    impl Input {
        /// A function tool's arguments; a freeform text is not a call of these tools.
        pub fn json(&self) -> Result<&str, String> {
            match self {
                Self::Json(raw) => Ok(raw),
                Self::Text(_) => Err("expected JSON input".to_string()),
            }
        }
    }

    #[derive(Deserialize)]
    pub struct ResultItem {
        pub status: String,
        pub content: String,
    }

    #[derive(Serialize)]
    pub struct CallDescription {
        pub verb: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        pub target: Option<String>,
        pub destructive: bool,
        #[serde(skip_serializing_if = "std::ops::Not::not")]
        pub shared: bool,
    }

    #[derive(Serialize)]
    pub struct ResultDescription {
        pub summary: String,
        pub detail: CommandDetail,
    }

    #[derive(Serialize)]
    #[serde(tag = "kind", rename = "command")]
    pub struct CommandDetail {
        #[serde(skip_serializing_if = "Option::is_none")]
        pub exit_code: Option<i32>,
        pub tail: Vec<String>,
    }

    pub fn text<T: Serialize>(value: &T) -> String {
        serde_json::to_string(value).expect("wire values always serialize")
    }

    pub fn outcome(status: &str, content: &str) -> String {
        text(&serde_json::json!({ "status": status, "content": content }))
    }
}

/// How a call reads on the restricted path: the first line of the command, and p1's own
/// destructiveness and read-only classification of it (the shell's), which the Codex face
/// does not change.
pub fn describe_command(cmd: &str) -> wire::CallDescription {
    let as_shell = json!({ "command": cmd }).to_string();
    let summary = p1_shell_guest::describe(p1_shell_guest::RawInput::Json(&as_shell), None);
    wire::CallDescription {
        verb: "run",
        target: summary.target,
        destructive: summary.destructive,
        shared: summary.shared,
    }
}

/// How a response reads: its exit or session line and the last lines of its output.
pub fn describe_response(content: &str, ok: bool) -> wire::ResultDescription {
    let mut lines = content.lines();
    let mut exit_code = None;
    let mut session = None;
    let mut body = Vec::new();
    let mut in_output = false;
    for line in lines.by_ref() {
        if in_output {
            body.push(line.to_string());
            continue;
        }
        if let Some(code) = line.strip_prefix("Process exited with code ") {
            exit_code = code.parse().ok();
        } else if let Some(id) = line.strip_prefix("Process running with session ID ") {
            session = Some(id.to_string());
        } else if line == "Output:" {
            in_output = true;
        }
    }
    let summary = match (ok, exit_code, session) {
        (true, Some(code), _) => format!("exit {code} · {} lines", body.len()),
        (true, None, Some(id)) => format!("running as session {id} · {} lines", body.len()),
        (true, None, None) => format!("{} lines", body.len()),
        (false, _, _) => content.lines().next().unwrap_or_default().to_string(),
    };
    let tail = body.iter().rev().take(5).rev().cloned().collect();
    wire::ResultDescription {
        summary,
        detail: wire::CommandDetail { exit_code, tail },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_exec_command_face_is_codex_shell_spec_verbatim() {
        let schema = exec_command::input_schema();
        assert_eq!(schema["required"], json!(["cmd"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(
            properties.keys().collect::<Vec<_>>(),
            [
                "cmd",
                "justification",
                "login",
                "max_output_tokens",
                "prefix_rule",
                "sandbox_permissions",
                "shell",
                "tty",
                "workdir",
                "yield_time_ms"
            ]
        );
        assert_eq!(
            properties["cmd"]["description"],
            "Shell command to execute."
        );
        assert_eq!(
            properties["sandbox_permissions"]["enum"],
            json!(["use_default", "require_escalated"])
        );
        assert_eq!(
            exec_command::DESCRIPTION,
            "Runs a command in a PTY, returning output or a session ID for ongoing interaction."
        );
    }

    #[test]
    fn the_write_stdin_face_is_codex_shell_spec_verbatim() {
        let schema = write_stdin::input_schema();
        assert_eq!(schema["required"], json!(["session_id"]));
        let properties = schema["properties"].as_object().unwrap();
        assert_eq!(
            properties.keys().collect::<Vec<_>>(),
            ["chars", "max_output_tokens", "session_id", "yield_time_ms"]
        );
        assert_eq!(properties["session_id"]["type"], "number");
    }

    #[test]
    fn defaults_and_clamps_are_codex_s() {
        let input = exec_command::parse(r#"{"cmd":"ls"}"#).unwrap();
        assert_eq!(input.yield_time_ms, 10_000);
        assert!(!input.tty);
        assert_eq!(input.script(), "ls");
        let input =
            exec_command::parse(r#"{"cmd":"ls","yield_time_ms":1,"workdir":"it's"}"#).unwrap();
        assert_eq!(input.yield_time_ms(), 250);
        assert_eq!(input.script(), "cd -- 'it'\\''s' || exit 1\nls");
        assert_eq!(
            exec_command::parse(r#"{"cmd":"ls","yield_time_ms":90000}"#)
                .unwrap()
                .yield_time_ms(),
            30_000
        );
        let escalated = exec_command::parse(
            r#"{"cmd":"ls","sandbox_permissions":"require_escalated","justification":"why"}"#,
        )
        .unwrap();
        assert!(escalated.requests_escalation());
        assert!(
            exec_command::parse(r#"{"cmd":"ls","unknown":1}"#).is_ok(),
            "Codex ignores unknown fields"
        );
        assert_eq!(
            exec_command::parse("{}").unwrap_err(),
            "failed to parse function arguments: missing field `cmd` at line 1 column 2"
        );
        let poll = write_stdin::parse(r#"{"session_id":3}"#).unwrap();
        assert_eq!(
            (poll.session_id, poll.chars.as_str(), poll.yield_time_ms),
            (3, "", 250)
        );
    }

    #[test]
    fn session_ids_are_the_job_numbers() {
        assert_eq!(session_id("j12"), Some(12));
        assert_eq!(session_id("job-12"), None);
        assert_eq!(job_id(7), "j7");
    }

    #[test]
    fn a_finished_response_reads_as_codex_renders_it() {
        let response = Response {
            chunk_id: "ab12cd",
            wall_time_ms: 1_234,
            exit_code: Some(0),
            session_id: None,
            raw_output: b"hello\n",
            max_output_tokens: None,
        };
        assert_eq!(
            response.render(),
            "Chunk ID: ab12cd\nWall time: 1.2340 seconds\nProcess exited with code 0\nOutput:\nhello\n"
        );
        let running = Response {
            chunk_id: "",
            wall_time_ms: 10_000,
            exit_code: None,
            session_id: Some(4),
            raw_output: b"",
            max_output_tokens: None,
        };
        assert_eq!(
            running.render(),
            "Wall time: 10.0000 seconds\nProcess running with session ID 4\nOutput:\n"
        );
    }

    #[test]
    fn long_output_is_truncated_in_the_middle_with_the_token_marker() {
        let text = "a".repeat(100);
        let response = Response {
            chunk_id: "",
            wall_time_ms: 0,
            exit_code: Some(0),
            session_id: None,
            raw_output: text.as_bytes(),
            max_output_tokens: Some(10),
        };
        let rendered = response.render();
        assert!(rendered.starts_with("Wall time: 0.0000 seconds\nProcess exited with code 0\nOriginal token count: 25\nOutput:\nWarning: truncated output (original token count: 25)\nTotal output lines: 1\n\n"), "{rendered}");
        assert!(rendered.contains("tokens truncated…"), "{rendered}");
        assert_eq!(
            truncate_middle_with_token_budget("abcdefgh", 1),
            "ab…1 tokens truncated…gh"
        );
        assert_eq!(
            truncate_middle_chars("abcdefgh", 4),
            "ab…4 chars truncated…gh"
        );
        assert_eq!(failure("boom"), "exec_command failed: boom");
        assert_eq!(aborted(1_500), "Wall time: 1.5 seconds\naborted by user");
    }

    #[test]
    fn descriptions_classify_the_command_as_the_shell_does() {
        let call = describe_command("rg --files");
        assert_eq!(
            (
                call.verb,
                call.target.as_deref(),
                call.destructive,
                call.shared
            ),
            ("run", Some("rg --files"), false, true)
        );
        let two_lines = describe_command("rg --files\ncargo build");
        assert_eq!(
            (two_lines.target.as_deref(), two_lines.shared),
            (Some("rg --files"), false)
        );
        let unsafe_call = describe_command("rm -rf /");
        assert!(unsafe_call.destructive);
        let result = describe_response(
            "Wall time: 0.1 seconds\nProcess exited with code 3\nOutput:\none\ntwo\n",
            true,
        );
        assert_eq!(result.summary, "exit 3 · 2 lines");
        assert_eq!(result.detail.exit_code, Some(3));
        assert_eq!(result.detail.tail, ["one", "two"]);
        let running = describe_response(
            "Wall time: 1 seconds\nProcess running with session ID 2\nOutput:\n",
            true,
        );
        assert_eq!(running.summary, "running as session 2 · 0 lines");
    }
}
