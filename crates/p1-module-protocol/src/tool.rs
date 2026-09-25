//! Wire form of the tool-side values (`schema/tool-outcome.json`,
//! `call-description.json`, `result-description.json`).

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{CallDescription, EditPreview, ToolOutcome};
use serde::{Deserialize, Serialize};

use crate::history::WireToolStatus;
use crate::{ConversionError, call_verb, refuse_null, to_u64, to_usize};

/// The result of one execution; `content` is exactly what the model will see.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireToolOutcome {
    /// How the call ended.
    pub status: WireToolStatus,
    /// What the model sees.
    pub content: String,
}

impl From<ToolOutcome> for WireToolOutcome {
    fn from(outcome: ToolOutcome) -> Self {
        Self {
            status: outcome.status.into(),
            content: outcome.content,
        }
    }
}

impl From<WireToolOutcome> for ToolOutcome {
    fn from(outcome: WireToolOutcome) -> Self {
        Self {
            status: outcome.status.into(),
            content: outcome.content,
        }
    }
}

/// An edit a call would make, shown to the operator before it runs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireEditPreview {
    /// The file.
    pub path: String,
    /// Text being replaced.
    pub old: String,
    /// Replacement text.
    pub new: String,
}

impl From<EditPreview> for WireEditPreview {
    fn from(edit: EditPreview) -> Self {
        Self {
            path: edit.path,
            old: edit.old,
            new: edit.new,
        }
    }
}

impl From<WireEditPreview> for EditPreview {
    fn from(edit: WireEditPreview) -> Self {
        Self {
            path: edit.path,
            old: edit.old,
            new: edit.new,
        }
    }
}

/// A tool's description of one call for the host and the UI (ADR-0057).
///
/// `verb` is a string on the wire, but only the closed vocabulary survives conversion:
/// see [`call_verb`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireCallDescription {
    /// One of [`crate::CALL_VERBS`]; anything else is shown as `call`.
    pub verb: String,
    /// What the call is about, trimmed for display.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub target: Option<String>,
    /// The edit the call would make, when it is one.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub edit: Option<WireEditPreview>,
    /// Whether the call destroys data; always stated, because a missing flag must not be
    /// read as "safe" by a peer that forgot it.
    pub destructive: bool,
}

impl From<CallDescription> for WireCallDescription {
    fn from(description: CallDescription) -> Self {
        Self {
            verb: description.verb.to_owned(),
            target: description.target,
            edit: description.edit.map(Into::into),
            destructive: description.destructive,
        }
    }
}

impl From<WireCallDescription> for CallDescription {
    fn from(description: WireCallDescription) -> Self {
        Self {
            verb: call_verb(&description.verb),
            target: description.target,
            edit: description.edit.map(Into::into),
            destructive: description.destructive,
        }
    }
}

/// Structured detail of a tool result for the UI.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WireResultDetail {
    /// A file change.
    Diff {
        /// The file.
        path: String,
        /// Content before.
        before: String,
        /// Content after.
        after: String,
    },
    /// A process run.
    Command {
        /// Absent when the process did not exit normally.
        #[serde(
            default,
            deserialize_with = "refuse_null",
            skip_serializing_if = "Option::is_none"
        )]
        exit_code: Option<i32>,
        /// Absent when not measured.
        #[serde(
            default,
            deserialize_with = "refuse_null",
            skip_serializing_if = "Option::is_none"
        )]
        elapsed_ms: Option<u64>,
        /// Last output lines.
        tail: Vec<String>,
    },
    /// Search matches.
    Matches {
        /// Number of matches.
        count: u64,
        /// Files with matches.
        files: Vec<String>,
    },
    /// A list of files.
    Files {
        /// The paths.
        paths: Vec<String>,
    },
    /// Free text; a struct variant because a tagged object cannot hold a bare string.
    Text {
        /// The text.
        text: String,
    },
}

impl From<ResultDetail> for WireResultDetail {
    fn from(detail: ResultDetail) -> Self {
        match detail {
            ResultDetail::Diff {
                path,
                before,
                after,
            } => Self::Diff {
                path,
                before,
                after,
            },
            ResultDetail::Command {
                exit_code,
                elapsed_ms,
                tail,
            } => Self::Command {
                exit_code,
                elapsed_ms,
                tail,
            },
            ResultDetail::Matches { count, files } => Self::Matches {
                count: to_u64(count),
                files,
            },
            ResultDetail::Files { paths } => Self::Files { paths },
            ResultDetail::Text(text) => Self::Text { text },
        }
    }
}

impl TryFrom<WireResultDetail> for ResultDetail {
    type Error = ConversionError;

    fn try_from(detail: WireResultDetail) -> Result<Self, Self::Error> {
        Ok(match detail {
            WireResultDetail::Diff {
                path,
                before,
                after,
            } => Self::Diff {
                path,
                before,
                after,
            },
            WireResultDetail::Command {
                exit_code,
                elapsed_ms,
                tail,
            } => Self::Command {
                exit_code,
                elapsed_ms,
                tail,
            },
            WireResultDetail::Matches { count, files } => Self::Matches {
                count: to_usize("matches.count", count)?,
                files,
            },
            WireResultDetail::Files { paths } => Self::Files { paths },
            WireResultDetail::Text { text } => Self::Text(text),
        })
    }
}

/// A tool's description of its result, so the host never decodes private output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WireResultDescription {
    /// One-line summary.
    pub summary: String,
    /// Structured detail, when the tool has one.
    #[serde(
        default,
        deserialize_with = "refuse_null",
        skip_serializing_if = "Option::is_none"
    )]
    pub detail: Option<WireResultDetail>,
}

impl From<ResultDescription> for WireResultDescription {
    fn from(description: ResultDescription) -> Self {
        Self {
            summary: description.summary,
            detail: description.detail.map(Into::into),
        }
    }
}

impl TryFrom<WireResultDescription> for ResultDescription {
    type Error = ConversionError;

    fn try_from(description: WireResultDescription) -> Result<Self, Self::Error> {
        Ok(Self {
            summary: description.summary,
            detail: description.detail.map(TryInto::try_into).transpose()?,
        })
    }
}
