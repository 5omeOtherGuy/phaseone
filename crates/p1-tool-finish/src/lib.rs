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

/// A `finish` call the tool accepted. Last accepted call wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    Done {
        summary: String,
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
const DESCRIPTION: &str = "End the task by saying, in a tool call, that it is done or blocked.\n`done`: verify first with a command, then name the exact command(s) you ran in `verification`; they must have succeeded after your last file change. Use `[\"none\"]` only when the task changed no files.\n`blocked`: say what you need in `needs` and what you tried; the run stops and reports it.";

/// The three exact rule texts, model-visible.
const ERR_MISSING_VERIFICATION: &str = "Name the commands you ran to verify the work in \"verification\". If nothing can be verified by a command, say why in \"summary\" and pass [\"none\"].";
const ERR_NONE_CHANGED_FILES: &str =
    "This session changed files; verify the result with a command before finishing.";
const ERR_NEEDS: &str = "Say what you need in \"needs\".";

/// The `finish` tool. Holds the session view and the outcome cell.
pub struct FinishTool {
    activity: Arc<dyn SessionActivity>,
    outcome: FinishOutcome,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl FinishTool {
    /// Build the tool with the default (`finish`, Claude-family) face.
    pub fn new(activity: Arc<dyn SessionActivity>, outcome: FinishOutcome) -> Self {
        Self {
            activity,
            outcome,
            declaration: declaration(default_face()),
            identity: identity("claude"),
        }
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        Self {
            activity: self.activity,
            outcome: self.outcome,
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
    /// outcome; `Err` is a rule violation that stores nothing.
    fn evaluate(&self, input: FinishInput) -> Result<String, String> {
        match input.status {
            Status::Done => {
                let verification = input.verification.unwrap_or_default();
                if verification.is_empty() {
                    return Err(ERR_MISSING_VERIFICATION.to_string());
                }
                if verification.len() == 1 && verification[0].trim() == "none" {
                    if self.activity.last_file_change().is_some() {
                        return Err(ERR_NONE_CHANGED_FILES.to_string());
                    }
                } else {
                    self.verify(&verification)?;
                }
                self.outcome.set(Accepted::Done {
                    summary: input.summary,
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

    /// Every named command must match the LAST recorded run of that command, and
    /// that run must be a success newer than the last file change.
    fn verify(&self, verification: &[String]) -> Result<(), String> {
        let last_change = self.activity.last_file_change();
        let runs = self.activity.shell_runs();
        for named in verification {
            let trimmed = named.trim();
            let run = runs.iter().rev().find(|run| run.command.trim() == trimmed);
            let successful = run.is_some_and(|run| run.exit_code == Some(0));
            if !successful {
                return Err(format!(
                    "No successful run of `{named}` is recorded in this session. Run it, read the result, then finish."
                ));
            }
            let run = run.expect("checked above");
            if let Some(change) = last_change
                && run.order < change
            {
                return Err(format!(
                    "You changed files after running `{named}`. Run it again, then finish."
                ));
            }
        }
        Ok(())
    }
}
