//! Native file-tool previews: parsing and summaries stay in each tool's logic.

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{CallDescription, EditPreview};

use crate::Workspace;

impl Workspace {
    /// Resolve the preview's path for the destructive flag, including granted roots.
    pub fn describe_edit(&self, verb: &'static str, edit: Option<EditPreview>) -> CallDescription {
        CallDescription {
            verb,
            target: edit.as_ref().map(|edit| edit.path.clone()),
            destructive: edit
                .as_ref()
                .is_some_and(|edit| self.resolve(&edit.path).is_err()),
            edit,
        }
    }
}

/// Convert the tool's successful edit preview to the contract's result detail.
pub fn describe_edit_result(summary: String, edit: Option<EditPreview>) -> ResultDescription {
    ResultDescription {
        summary,
        detail: edit.map(|edit| ResultDetail::Diff {
            path: edit.path,
            before: edit.old,
            after: edit.new,
        }),
    }
}
