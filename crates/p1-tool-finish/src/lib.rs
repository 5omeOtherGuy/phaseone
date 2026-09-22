//! The `finish` tool: completion as an OBSERVABLE ACT instead of a phrase.
//!
//! An unattended model ends its work by calling this tool. The tool does NOT take
//! the model's word for it: it reads the session through the [`SessionActivity`]
//! trait (implemented by the host from its event stream) and refuses `done` until
//! each named verification command really ran, succeeded, and ran after the last
//! file change. A `blocked` call records what the model needs and stops the run.
//!
//! The accepted outcome is stored in a shared [`FinishOutcome`] cell the host
//! reads after the turn; a rejected call stores nothing. Invalid input is an
//! ordinary tool result the model can act on — never a panic.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use p1_contracts::{
    BoxFuture, DeclarationKind, Effect, Tool, ToolCall, ToolContext, ToolDeclaration, ToolIdentity,
    ToolInput, ToolOutcome,
};
use serde::Deserialize;

/// One finished `Executes` call, oldest first in [`SessionActivity::shell_runs`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    /// The command exactly as the model passed it to the `shell` tool.
    pub command: String,
    /// The parsed `[exit code: N]` footer; `None` when there was none (timeout,
    /// cancellation, a non-shell `Executes` tool). `None` never counts as success.
    pub exit_code: Option<i32>,
    /// Monotonically increasing order of the finished call within the session.
    pub order: u64,
}

/// What the `finish` tool can see of the session so far. Implemented by the host
/// from the event stream it already receives.
pub trait SessionActivity: Send + Sync {
    /// `order` of the last finished tool call whose effect was `WritesFiles`, if any.
    fn last_file_change(&self) -> Option<u64>;
    /// Every finished `Executes` call so far, oldest first.
    fn shell_runs(&self) -> Vec<ShellRun>;
}

/// Which completion rule a `finish` tool applies (ADR-0051 item 1). The HOST chooses
/// it at construction, from the assembled tools' identities; the tool never inspects
/// grant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// Today's rule (ADR-0037), unchanged: `done` needs a command whose successful
    /// run is recorded after the last file change, and `["none"]` is accepted only in
    /// a session that changed no file.
    RecordedCommands,
    /// For an agent with no tool that runs commands: `["none"]` is accepted after a
    /// file change too, and the accepted result is labelled unverified for the parent.
    ReportToParent,
}

impl CompletionPolicy {
    /// The model-facing description that carries this policy. The default face of a
    /// tool built under the policy, so a host that switches the policy late can
    /// present the tool the model would have got from the factory.
    fn description(self) -> &'static str {
        match self {
            Self::RecordedCommands => DESCRIPTION,
            Self::ReportToParent => REPORT_DESCRIPTION,
        }
    }
}

/// What an accepted `done` established (ADR-0051 item 2). Host-owned: it is built
/// from the session's own record, never from the model's input, and it is what the
/// parent is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// These commands all have a recorded successful run after the last file change,
    /// spelled as the trailer spells them (normalised).
    CommandsPassed(Vec<String>),
    /// Nothing was established, and why. Never printed as "verified".
    NotRun(String),
}

/// A `finish` call the tool accepted. Last accepted call wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    Done {
        summary: String,
        evidence: Evidence,
    },
    Blocked {
        summary: String,
        needs: String,
        tried: Vec<String>,
    },
}

/// The shared cell the host reads after a turn. Cheap to clone; all clones share
/// one value. The tool writes it; the host reads and clears it.
#[derive(Clone, Default)]
pub struct FinishOutcome {
    inner: Arc<Mutex<Option<Accepted>>>,
}

impl FinishOutcome {
    /// The last accepted outcome, if any.
    pub fn get(&self) -> Option<Accepted> {
        self.inner.lock().unwrap().clone()
    }

    /// Drop any outcome, so an earlier turn cannot end a later one.
    pub fn clear(&self) {
        *self.inner.lock().unwrap() = None;
    }

    fn set(&self, accepted: Accepted) {
        *self.inner.lock().unwrap() = Some(accepted);
    }
}

/// Model-facing name + description override, mirroring `p1-workspace::ToolFace`
/// without taking a dependency on it.
#[derive(Debug, Clone)]
pub struct ToolFace {
    pub name: String,
    pub description: String,
}

impl ToolFace {
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
        }
    }
}

const NAME: &str = "finish";
const DESCRIPTION: &str = "End the task by saying, in a tool call, that it is done or blocked.\n`done`: verify first with a command, then name the exact command(s) you ran in `verification`; they must have succeeded after your last file change. Use `[\"none\"]` only when the task changed no files.\n`blocked`: say what you need in `needs` and what you tried; the run stops and reports it.\nA pipe does not count: a command run through a pipe (for example `... | tail`) exits with its last stage's code, so run the check without a pipe. The same goes for `;`, `||`, a single `&` or a new line after the check. Name the command as you ran it; a leading `cd <dir> &&` and spacing differences are ignored.";

/// The `ReportToParent` face (ADR-0051 item 1): the same tool and the same checks,
/// presented to an agent that has no tool that runs commands.
const REPORT_DESCRIPTION: &str = "End the task by saying, in a tool call, that it is done or blocked.\nYou have no tool that runs commands, so no command can verify this work: call `done` with `verification: [\"none\"]`, and say in `summary` what you did and what remains unchecked. The result is reported to your parent as \"not verified; parent verification required\".\n`done`: `verification` is `[\"none\"]` — a command you cannot run proves nothing.\n`blocked`: say what you need in `needs` and what you tried; the run stops and reports it.";

/// The three exact rule texts, model-visible.
const ERR_MISSING_VERIFICATION: &str = "Name the commands you ran to verify the work in \"verification\". If nothing can be verified by a command, say why in \"summary\" and pass [\"none\"].";
const ERR_NONE_CHANGED_FILES: &str =
    "This session changed files; verify the result with a command before finishing.";
const ERR_NEEDS: &str = "Say what you need in \"needs\".";

/// Error 1 additionally shows the call shape, so the model has a template.
const CALL_SHAPE: &str =
    "Call finish again with \"verification\": [\"<one of the commands below>\"].";
/// Every verification rejection ends with what would be accepted right now.
const TRAILER_HEADING: &str =
    "Runs that count right now (successful, not piped, after the last file change):";
const TRAILER_NONE: &str =
    "No run counts right now: run your checks (without a pipe) after your last file change.";

/// The `finish` tool. Holds the session view, the outcome cell and the completion
/// policy the host chose for this agent.
pub struct FinishTool {
    activity: Arc<dyn SessionActivity>,
    outcome: FinishOutcome,
    policy: CompletionPolicy,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl FinishTool {
    /// Build the tool with the default (`finish`, Claude-family) face and the
    /// default policy, [`CompletionPolicy::RecordedCommands`].
    pub fn new(activity: Arc<dyn SessionActivity>, outcome: FinishOutcome) -> Self {
        let policy = CompletionPolicy::RecordedCommands;
        Self {
            activity,
            outcome,
            policy,
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
    }

    /// Apply the policy the host chose for this agent (ADR-0051 item 1). The
    /// model-facing description follows the policy, because it is what tells the
    /// model which completion rule applies to it; a later [`FinishTool::with_face`]
    /// still overrides it.
    pub fn with_policy(mut self, policy: CompletionPolicy) -> Self {
        self.policy = policy;
        self.declaration.description = policy.description().to_string();
        self
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            activity: self.activity,
            outcome: self.outcome,
            policy: self.policy,
            declaration: declaration(face),
            identity: identity(variant),
        }
    }
}

fn default_face() -> ToolFace {
    ToolFace::new(NAME, DESCRIPTION)
}

fn declaration(face: ToolFace) -> ToolDeclaration {
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind: DeclarationKind::Function {
            input_schema: input_schema(),
        },
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

fn input_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["done", "blocked"],
                "description": "\"done\" when the task is complete and verified, \"blocked\" when something outside your control stops you."
            },
            "summary": {
                "type": "string",
                "description": "Short summary of what you did, or why nothing could be verified."
            },
            "verification": {
                "type": "array",
                "items": { "type": "string" },
                "description": "For \"done\": the exact commands you ran that prove the work. [\"none\"] only when no files changed."
            },
            "needs": {
                "type": "string",
                "description": "For \"blocked\": what you need from outside to continue."
            },
            "tried": {
                "type": "array",
                "items": { "type": "string" },
                "description": "For \"blocked\": what you already tried."
            }
        },
        "required": ["status", "summary"],
        "additionalProperties": false
    })
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Status {
    Done,
    Blocked,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct FinishInput {
    status: Status,
    summary: String,
    #[serde(default)]
    verification: Option<Vec<String>>,
    #[serde(default)]
    needs: Option<String>,
    #[serde(default)]
    tried: Option<Vec<String>>,
}

impl Tool for FinishTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let input = match parse_input(&self.declaration.name, call) {
                Ok(input) => input,
                Err(message) => return ToolOutcome::error(message),
            };
            match self.evaluate(input) {
                Ok(message) => ToolOutcome::ok(message),
                Err(message) => ToolOutcome::error(message),
            }
        })
    }
}

fn parse_input(tool: &str, call: &ToolCall) -> Result<FinishInput, String> {
    let raw = match &call.input {
        ToolInput::Json(raw) => raw,
        ToolInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

impl FinishTool {
    /// Apply the §2 rules. `Ok` is the accepted model-visible text and stores the
    /// outcome; `Err` is a rule violation that stores nothing. The policy decides
    /// what `["none"]` means (ADR-0051 item 1); every other rule is shared, so
    /// invalid evidence never downgrades to an accepted unverified result.
    fn evaluate(&self, input: FinishInput) -> Result<String, String> {
        match input.status {
            Status::Done => {
                let verification = input.verification.unwrap_or_default();
                if verification.is_empty() {
                    return Err(self.error_one(ERR_MISSING_VERIFICATION));
                }
                let evidence = if verification.len() == 1 && verification[0].trim() == "none" {
                    match self.policy {
                        // An agent with no command tool cannot verify anything itself;
                        // it ends honestly and the parent verifies (ADR-0051 item 1).
                        CompletionPolicy::ReportToParent => {
                            Evidence::NotRun("no command tool granted".to_string())
                        }
                        CompletionPolicy::RecordedCommands => {
                            if self.activity.last_file_change().is_some() {
                                return Err(self.with_trailer(ERR_NONE_CHANGED_FILES));
                            }
                            // Not writing a file is no proof that an answer is right.
                            Evidence::NotRun("no file changed".to_string())
                        }
                    }
                } else {
                    self.verify(&verification)?;
                    Evidence::CommandsPassed(
                        verification
                            .iter()
                            .map(|named| normalise_command(named))
                            .collect(),
                    )
                };
                self.outcome.set(Accepted::Done {
                    summary: input.summary,
                    evidence,
                });
                Ok("Finished.".to_string())
            }
            Status::Blocked => {
                let needs = input.needs.unwrap_or_default();
                if needs.trim().is_empty() {
                    return Err(ERR_NEEDS.to_string());
                }
                self.outcome.set(Accepted::Blocked {
                    summary: input.summary,
                    needs,
                    tried: input.tried.unwrap_or_default(),
                });
                Ok("Recorded as blocked.".to_string())
            }
        }
    }

    /// Every named command must match, after normalisation, the LAST recorded
    /// run of that command, and that run must be a success newer than the last
    /// file change. EVERY failing command is reported in one error, in the order
    /// named, so one mistake costs one call.
    fn verify(&self, verification: &[String]) -> Result<(), String> {
        let last_change = self.activity.last_file_change();
        let runs = self.activity.shell_runs();
        let failures: Vec<String> = verification
            .iter()
            .filter_map(|named| failure_of(named, &runs, last_change))
            .collect();
        if failures.is_empty() {
            Ok(())
        } else {
            Err(self.with_trailer(failures.join("\n")))
        }
    }

    /// Error 1 additionally shows the call shape before the trailer.
    fn error_one(&self, message: &str) -> String {
        format!("{message}\n\n{CALL_SHAPE}\n\n{}", self.trailer())
    }

    /// Errors 1–3 end with a blank line and the runs that would be accepted now.
    fn with_trailer(&self, message: impl AsRef<str>) -> String {
        format!("{}\n\n{}", message.as_ref(), self.trailer())
    }

    fn trailer(&self) -> String {
        let commands = self.counting_commands();
        if commands.is_empty() {
            return TRAILER_NONE.to_string();
        }
        let mut text = String::from(TRAILER_HEADING);
        for command in commands {
            text.push_str("\n- ");
            text.push_str(&command);
        }
        text
    }

    /// The commands `finish` would accept right now: one entry per normalised
    /// spelling (the LAST run decides), successful, unpiped, unmasked and newer than
    /// the last file change; newest last, at most five.
    fn counting_commands(&self) -> Vec<String> {
        let last_change = self.activity.last_file_change();
        let mut last: HashMap<String, ShellRun> = HashMap::new();
        for run in self.activity.shell_runs() {
            last.insert(normalise_command(&run.command), run);
        }
        let mut counting: Vec<(u64, String)> = last
            .into_iter()
            .filter(|(_, run)| {
                run.exit_code == Some(0)
                    && !is_piped(&run.command)
                    && !is_masked(&run.command)
                    && last_change.is_none_or(|change| run.order > change)
            })
            .map(|(command, run)| (run.order, command))
            .collect();
        counting.sort();
        let keep_from = counting.len().saturating_sub(5);
        counting
            .into_iter()
            .skip(keep_from)
            .map(|(_, command)| command)
            .collect()
    }
}

fn no_successful_run(named: &str) -> String {
    format!(
        "No successful run of `{named}` is recorded in this session. Run it, read the result, then finish."
    )
}

fn pipe_error(named: &str) -> String {
    format!(
        "`{named}` was run through a pipe, so its exit code says nothing about it. Run it without a pipe, then finish."
    )
}

fn masked_error(named: &str) -> String {
    format!(
        "`{named}` continues after a failure (`;`, `||`, `&` or a new line), so its exit code says nothing about the check. Run the check on its own, then finish."
    )
}

/// `None` when the named command passes rules 2, 3, the pipe rule and the masked rule;
/// `Some` with the model-visible message of the FIRST rule it breaks.
fn failure_of(named: &str, runs: &[ShellRun], last_change: Option<u64>) -> Option<String> {
    let wanted = normalise_command(named);
    let Some(run) = runs
        .iter()
        .rev()
        .find(|run| normalise_command(&run.command) == wanted)
    else {
        return Some(no_successful_run(named));
    };
    // A pipe hides the check's exit code behind its last stage's, so the recorded
    // status says nothing even when it is zero.
    if is_piped(&run.command) {
        return Some(pipe_error(named));
    }
    // `;`, `||`, a newline or a single `&` lets something else run last, with the
    // same effect.
    if is_masked(&run.command) {
        return Some(masked_error(named));
    }
    if run.exit_code != Some(0) {
        return Some(no_successful_run(named));
    }
    if let Some(change) = last_change
        && run.order < change
    {
        return Some(format!(
            "You changed files after running `{named}`. Run it again, then finish."
        ));
    }
    None
}

/// Normalise a command for comparison: trim, collapse every run of whitespace to
/// one space, and drop ONE leading `cd <path> &&` segment. Applied to both the
/// recorded command and the named one, so matching is symmetric.
fn normalise_command(command: &str) -> String {
    let collapsed = command.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.find(" && ") {
        // `at > 3` keeps the path between `cd ` and ` && ` non-empty.
        Some(at) if collapsed.starts_with("cd ") && at > 3 => collapsed[at + 4..].to_string(),
        _ => collapsed,
    }
}

/// Every character of `command` that sits OUTSIDE `'…'`/`"…"`, with the characters
/// on either side of it (quoted or not). Quoting is a simple scan for quotes, not a
/// shell parser (a stated limit in `docs/design/completion.md` §2); the pipe and the
/// masking test share this one scan.
fn outside_quotes(command: &str) -> Vec<(char, Option<char>, Option<char>)> {
    let chars: Vec<char> = command.chars().collect();
    let mut unquoted = Vec::new();
    let mut quote: Option<char> = None;
    for (index, &character) in chars.iter().enumerate() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None => match character {
                '\'' => quote = Some('\''),
                '"' => quote = Some('"'),
                _ => unquoted.push((
                    character,
                    index.checked_sub(1).map(|i| chars[i]),
                    chars.get(index + 1).copied(),
                )),
            },
        }
    }
    unquoted
}

/// True when the command contains an unquoted `|` that is not part of `||`.
fn is_piped(command: &str) -> bool {
    outside_quotes(command)
        .into_iter()
        .any(|(character, previous, next)| {
            character == '|' && previous != Some('|') && next != Some('|')
        })
}

/// True when the exit status is masked: outside quotes the command contains `;`, `||`,
/// a newline, or a single `&` that is not part of `&&`. Each of those runs something
/// else afterwards, which decides the exit code, so the recorded status says nothing
/// about the check that ran first. `&&` chains stay honest, and so does an `&` that
/// belongs to a redirection (`2>&1`, `>&2` after the `>`, `&>file` before it).
fn is_masked(command: &str) -> bool {
    outside_quotes(command)
        .into_iter()
        .any(|(character, previous, next)| match character {
            ';' | '\n' => true,
            '|' => next == Some('|'),
            '&' => !matches!(previous, Some('&' | '>')) && !matches!(next, Some('&' | '>')),
            _ => false,
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_trims_collapses_and_drops_one_leading_cd() {
        assert_eq!(normalise_command("  cargo   test  "), "cargo test");
        assert_eq!(
            normalise_command("cd /w/x && cargo fmt --check"),
            "cargo fmt --check"
        );
        assert_eq!(normalise_command("cargo fmt --check"), "cargo fmt --check");
        assert_eq!(normalise_command("cd a && cd b && x"), "cd b && x");
        assert_eq!(normalise_command("cd a &&  cd b && x"), "cd b && x");
        assert_eq!(normalise_command("cdx a && b"), "cdx a && b");
        assert_eq!(normalise_command("cd && b"), "cd && b");
    }

    #[test]
    fn pipe_detection_ignores_quoted_and_double_pipes() {
        assert!(is_piped("cargo test 2>&1 | tail -5"));
        assert!(is_piped("grep x f | wc -l"));
        assert!(!is_piped("a || b"));
        assert!(!is_piped("echo 'a|b'"));
        assert!(!is_piped("echo \"a|b\""));
        assert!(!is_piped("cargo test"));
    }

    #[test]
    fn masked_detection_sees_sequencing_and_background_runs() {
        assert!(is_masked("cargo test; echo done"));
        assert!(is_masked("cargo test || true"));
        assert!(is_masked("cargo test &"));
        assert!(is_masked("cargo test\ncargo fmt --check"));
        assert!(is_masked("cd /w && cargo test; echo done"));
        assert!(!is_masked("cargo fmt --check && cargo test"));
        assert!(!is_masked("cd /w && cargo test"));
        assert!(!is_masked("echo \"a;b\""));
        assert!(!is_masked("echo 'x || y'"));
        assert!(!is_masked("cargo test"));
    }

    #[test]
    fn a_redirected_stream_is_not_backgrounding() {
        // An `&` that belongs to a redirection leaves the exit code to the check.
        assert!(!is_masked("cargo test 2>&1"));
        assert!(!is_masked("cargo test >&2"));
        assert!(!is_masked("cargo test &> log.txt"));
        assert!(!is_masked("cargo test &>> log.txt"));
        assert!(!is_masked("cargo test 2>&1 | tail -5"));
        // Backgrounding still masks.
        assert!(is_masked("cargo test &"));
        assert!(is_masked("cargo test & echo x"));
    }
}
