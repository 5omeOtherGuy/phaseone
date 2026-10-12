//! The Claude Code hook bridge (ADR-0213, seam #711): the first adapter over the hook core.
//!
//! It reads the `hooks` table of Claude Code's settings files ([`hook_files`], [`read_hooks`])
//! into a [`HookSet`], and speaks Claude Code's hook protocol ([`ClaudeCodeCodec`]): the JSON a
//! hook reads on its standard input, and what its exit code and output mean. A hook written
//! for Claude Code runs unchanged under p1 for the events p1 serves (`SessionStart`,
//! `UserPromptSubmit`, `PreToolUse`); a hook of another event is read and reported as not run.
//!
//! What p1 does NOT take over from Claude Code: an `allow` grants nothing (the session's own
//! authorization still decides, ADR-0213), and `updatedInput` is not applied.

use std::path::{Path, PathBuf};
use std::time::Duration;

use p1_contracts::serde_json::{Value, json};
use p1_contracts::{HookDecision, HookEvent, HookOutput};
use p1_hooks::{HookCodec, HookCommand, HookSet, Matcher, ProcessOutput};
use serde::Deserialize;

/// A hook's timeout when the file names none: Claude Code's default.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(60);

/// The settings files Claude Code reads hooks from, lowest priority first: the user's, the
/// project's, then the project's local one. Hooks of every file run, in this order.
pub fn hook_files(home: Option<&Path>, workspace: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = home
        .map(|home| home.join(".claude/settings.json"))
        .into_iter()
        .collect();
    files.push(workspace.join(".claude/settings.json"));
    files.push(workspace.join(".claude/settings.local.json"));
    files
}

#[derive(Deserialize)]
struct SettingsFile {
    #[serde(default)]
    hooks: std::collections::BTreeMap<String, Vec<MatcherGroup>>,
}

#[derive(Deserialize)]
struct MatcherGroup {
    #[serde(default)]
    matcher: String,
    hooks: Vec<HookEntry>,
}

#[derive(Deserialize)]
struct HookEntry {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    command: Option<String>,
    /// Seconds.
    #[serde(default)]
    timeout: Option<u64>,
}

/// What reading the hook files found: the hooks p1 runs, and one note for each it does not.
#[derive(Debug, Default)]
pub struct ReadHooks {
    pub set: HookSet,
    pub notes: Vec<String>,
}

/// Reads the `hooks` of every file of `files` that exists, in order. A file that exists but is
/// not a settings file is an error naming it; a hook p1 cannot run is a note.
pub fn read_hooks(files: &[PathBuf]) -> Result<ReadHooks, String> {
    let mut read = ReadHooks::default();
    for path in files {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(format!("{}: {error}", path.display())),
        };
        let settings: SettingsFile = serde_json::from_str(&text)
            .map_err(|error| format!("{}: not Claude Code settings: {error}", path.display()))?;
        for (name, groups) in settings.hooks {
            let Some(event) = HookEvent::parse(&name) else {
                read.notes.push(format!(
                    "{}: {name} hooks are not run: p1 has no {name} event",
                    path.display()
                ));
                continue;
            };
            if !event.is_served() {
                read.notes.push(format!(
                    "{}: {name} hooks are read and not run: p1 has no interception point for {name} yet",
                    path.display()
                ));
            }
            for group in groups {
                let matcher = Matcher::parse(&group.matcher)
                    .map_err(|error| format!("{}: {name}: {error}", path.display()))?;
                for hook in group.hooks {
                    let Some(command) = hook.command.filter(|_| hook.kind == "command") else {
                        read.notes.push(format!(
                            "{}: a {name} hook of type `{}` is not run: p1 runs command hooks",
                            path.display(),
                            hook.kind
                        ));
                        continue;
                    };
                    read.set.push(
                        event,
                        HookCommand {
                            matcher: matcher.clone(),
                            command,
                            timeout: hook.timeout.map_or(DEFAULT_TIMEOUT, Duration::from_secs),
                        },
                    );
                }
            }
        }
    }
    Ok(read)
}

/// The name Claude Code gives the tool p1 calls `name`, so a hook's matcher (`Bash`, `Edit`)
/// applies to p1's own tools. A tool Claude Code has no counterpart for keeps p1's name.
pub fn claude_tool_name(name: &str) -> &str {
    match name {
        "shell" => "Bash",
        "read" => "Read",
        "write" => "Write",
        "edit" | "apply_patch" => "Edit",
        "grep" => "Grep",
        "ls" => "LS",
        "todo_write" => "TodoWrite",
        "task" => "Task",
        other => other,
    }
}

/// Claude Code's hook protocol over p1's payloads.
pub struct ClaudeCodeCodec {
    session_id: String,
    cwd: PathBuf,
    transcript_path: Option<PathBuf>,
}

impl ClaudeCodeCodec {
    /// The codec of one session: its id, its workspace and, when it has one, its journal.
    pub fn new(session_id: String, cwd: PathBuf, transcript_path: Option<PathBuf>) -> Self {
        Self {
            session_id,
            cwd,
            transcript_path,
        }
    }
}

impl HookCodec for ClaudeCodeCodec {
    fn subject(&self, event: HookEvent, payload: &Value) -> Option<String> {
        matches!(event, HookEvent::PreToolUse | HookEvent::PostToolUse)
            .then(|| claude_tool_name(payload["tool_name"].as_str().unwrap_or_default()).to_owned())
    }

    /// `session_id`, `transcript_path`, `cwd` and `hook_event_name`, then the event's own
    /// fields: `tool_name` (Claude Code's) and `tool_input`, `prompt`, or `source`.
    fn encode(&self, event: HookEvent, payload: &Value) -> Vec<u8> {
        let mut input = json!({
            "session_id": self.session_id,
            "transcript_path": self.transcript_path.as_ref().map(|path| path.display().to_string()),
            "cwd": self.cwd.display().to_string(),
            "hook_event_name": event.name(),
        });
        match event {
            HookEvent::PreToolUse | HookEvent::PostToolUse => {
                input["tool_name"] = json!(claude_tool_name(
                    payload["tool_name"].as_str().unwrap_or_default()
                ));
                input["tool_input"] = payload["tool_input"].clone();
            }
            HookEvent::UserPromptSubmit => input["prompt"] = payload["prompt"].clone(),
            HookEvent::SessionStart => input["source"] = payload["source"].clone(),
            HookEvent::Stop | HookEvent::SubagentStart | HookEvent::SubagentStop => {}
        }
        serde_json::to_vec(&input).expect("hook input serializes")
    }

    /// Exit 0 with JSON is its answer, with plain text (a prompt or session hook) context;
    /// exit 2 blocks with standard error as the reason; any other exit is a failure that blocks
    /// nothing.
    fn decode(&self, event: HookEvent, output: &ProcessOutput) -> Result<HookOutput, String> {
        let text = |bytes: &[u8]| String::from_utf8_lossy(bytes).trim().to_owned();
        match output.status {
            Some(0) => {}
            Some(2) => {
                let reason = text(&output.stderr);
                return Ok(HookOutput {
                    decision: Some(HookDecision::Deny {
                        reason: if reason.is_empty() {
                            "blocked by a hook".to_owned()
                        } else {
                            reason
                        },
                    }),
                    ..HookOutput::default()
                });
            }
            Some(code) => return Err(format!("exited {code}: {}", text(&output.stderr))),
            None => return Err("was killed by a signal".to_owned()),
        }
        let stdout = text(&output.stdout);
        if stdout.is_empty() {
            return Ok(HookOutput::default());
        }
        let Ok(Value::Object(answer)) = serde_json::from_str::<Value>(&stdout) else {
            // Plain text on a prompt or session hook is context; elsewhere it is a log line.
            return Ok(HookOutput {
                additional_context: matches!(
                    event,
                    HookEvent::UserPromptSubmit | HookEvent::SessionStart
                )
                .then_some(stdout),
                ..HookOutput::default()
            });
        };
        let field = |name: &str| answer.get(name).and_then(Value::as_str);
        let specific = answer.get("hookSpecificOutput").and_then(Value::as_object);
        let specific_field = |name: &str| {
            specific
                .and_then(|specific| specific.get(name))
                .and_then(Value::as_str)
        };
        let reason = || {
            specific_field("permissionDecisionReason")
                .or_else(|| field("reason"))
                .unwrap_or("blocked by a hook")
                .to_owned()
        };
        let decision = match (specific_field("permissionDecision"), field("decision")) {
            (Some("deny"), _) | (_, Some("block")) => Some(HookDecision::Deny { reason: reason() }),
            (Some("ask"), _) => Some(HookDecision::Ask),
            (Some("allow"), _) | (_, Some("approve")) => Some(HookDecision::Allow),
            _ => None,
        };
        let stop = (answer.get("continue").and_then(Value::as_bool) == Some(false)).then(|| {
            field("stopReason")
                .unwrap_or("a hook stopped the session")
                .to_owned()
        });
        Ok(HookOutput {
            decision,
            additional_context: specific_field("additionalContext").map(str::to_owned),
            stop,
        })
    }
}
