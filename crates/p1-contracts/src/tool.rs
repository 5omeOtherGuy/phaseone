//! Tool seam: declaration + validation + execution, registered together.
//!
//! A tool is constructed by the composition root with whatever it needs (workspace
//! root, limits, services) — there is no shared tool state and no service bag.
//! Safety invariants (path confinement, atomic writes) live inside the tool;
//! whether a call may run at all is the authorization policy's decision.

use serde::{Deserialize, Serialize};

use crate::history::{ToolCall, ToolStatus};
use crate::{BoxFuture, CancellationToken};

/// The model-facing name and description of a tool. The schema and semantics
/// stay the same when an environment presents a different face.
#[derive(Debug, Clone, PartialEq, Eq)]
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

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeclarationKind {
    /// JSON-schema function tool; input arrives as `ToolInput::Json`.
    Function { input_schema: serde_json::Value },
    /// Freeform tool; input arrives as `ToolInput::Text`, optionally constrained by
    /// a grammar (`syntax` e.g. `"lark"`).
    Freeform { grammar: Option<Grammar> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grammar {
    pub syntax: String,
    pub definition: String,
}

/// What the model is told about a tool. The provider translates this and nothing else.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolDeclaration {
    /// Model-facing call name, unique within one agent's environment.
    pub name: String,
    pub description: String,
    pub kind: DeclarationKind,
}

/// Stable identity of the implementation + model-facing variant behind a name
/// (e.g. `p1-tool-edit` / `claude`). Journalled with every call so that a renamed
/// or replaced implementation never silently inherits another one's calls or grants.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct ToolIdentity {
    pub implementation: String,
    pub variant: String,
}

/// What executing THIS input may do; the input to authorization.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Effect {
    /// Reads inside the tool's confinement only.
    ReadOnly,
    /// Changes files inside the tool's confinement.
    WritesFiles,
    /// Runs arbitrary processes; effects are not bounded by the tool.
    Executes,
    /// Starts or controls other agents.
    Delegates,
}

/// A tool's own description of one call's target, for the host and the UI
/// (ADR-0057). `verb` is a short word the UI can show (`read`, `edit`, `run`,
/// `search`, `finish`, `worker`, `workflow`…); `target` is the file, directory,
/// command or worker the call is about, already trimmed for display. No argument
/// key leaves the tool: only the tool knows what its input means.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EditPreview {
    pub path: String,
    pub old: String,
    pub new: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CallDescription {
    pub verb: &'static str,
    pub target: Option<String>,
    #[serde(default)]
    pub edit: Option<EditPreview>,
    #[serde(default)]
    pub destructive: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResultDescription {
    pub summary: String,
    pub detail: Option<ResultDetail>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ResultDetail {
    Diff {
        path: String,
        before: String,
        after: String,
    },
    Command {
        exit_code: Option<i32>,
        elapsed_ms: Option<u64>,
        tail: Vec<String>,
    },
    Matches {
        count: usize,
        files: Vec<String>,
    },
    Files {
        paths: Vec<String>,
    },
    Text(String),
}

pub struct ToolContext {
    pub cancel: CancellationToken,
}

/// Result of one execution. `content` is exactly what the model will see.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutcome {
    pub status: ToolStatus,
    pub content: String,
}

impl ToolOutcome {
    pub fn ok(content: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Ok,
            content: content.into(),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            status: ToolStatus::Error,
            content: content.into(),
        }
    }
}

pub trait Tool: Send + Sync {
    fn declaration(&self) -> &ToolDeclaration;

    fn identity(&self) -> &ToolIdentity;

    /// Classify a call for authorization. Must not have side effects. Invalid input
    /// is classified by the tool's worst case; `execute` reports the input error.
    fn effect(&self, call: &ToolCall) -> Effect;

    /// Describe what this call is about, for the host and the UI (ADR-0057). Must
    /// not have side effects; invalid input yields a best-effort or empty target.
    /// The default names the declaration in `target`: `verb` is the neutral `"call"`
    /// because a `&'static str` verb cannot borrow the declaration's own `String`
    /// name, so the name goes in `target` instead.
    fn describe(&self, _call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "call",
            target: Some(self.declaration().name.clone()),
            edit: None,
            destructive: false,
        }
    }

    /// Describe this tool's result without requiring the host to decode private output.
    fn describe_result(
        &self,
        _call: &ToolCall,
        result: &crate::history::ToolResultItem,
    ) -> ResultDescription {
        ResultDescription {
            summary: result
                .content
                .lines()
                .next()
                .unwrap_or_default()
                .to_string(),
            detail: None,
        }
    }

    /// Validate the raw input and run. Invalid input is an `Error` outcome with a
    /// message the model can act on — never a panic and never a guessed repair.
    /// On cancellation return promptly with `ToolStatus::Cancelled`.
    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn old_call_descriptions_default_to_non_destructive() {
        let description: CallDescription =
            serde_json::from_str(r#"{"verb":"read","target":"src/lib.rs","edit":null}"#).unwrap();
        assert!(!description.destructive);
    }

    #[test]
    fn result_detail_round_trips() {
        let result = ResultDescription {
            summary: "+1 −1".into(),
            detail: Some(ResultDetail::Diff {
                path: "file.rs".into(),
                before: "old".into(),
                after: "new".into(),
            }),
        };
        let json = serde_json::to_string(&result).unwrap();
        assert_eq!(
            serde_json::from_str::<ResultDescription>(&json).unwrap(),
            result
        );
    }
}
