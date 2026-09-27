//! The `edit` tool module (`p1/edit`): exact string replacement in one workspace file.
//!
//! A package of the module workspace (its frozen manifest fields are in `Cargo.toml`, the
//! format in `docs/design/modules/package.md`) implementing the `tool` world of
//! `modules/wit/`. Everything it decides is in `p1-tool-edit-logic`, the crate the native
//! `p1-tool-edit` runs too, so the texts the model sees cannot drift between the two. This
//! crate only adapts the WIT imports to that crate's [`Capabilities`] and hands each export's
//! argument text to it.
//!
//! `effect`, `describe` and `describe-result` work from their arguments alone: the host calls
//! the first two on the restricted path, where any import traps. `execute` validates the
//! input, reads the target through `workspace`, checks this agent's observation of it through
//! `snapshot` and computes the new contents, all outside the write gate; only then does it
//! `begin` a `workspace-mutation` and write. It checks `control.cancelled()` between those
//! steps. The guest has no thread: every wait is a host import it is suspended in.
//!
//! Like every module it imports no `wasi:` interface (decision D-XO-4): the built component
//! imports only `control`, `workspace`, `snapshot`, `workspace-mutation` and the type-only
//! `types`.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::{control, snapshot, workspace, workspace_mutation};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_tool_edit_logic::exec::{Capabilities, Entry, EntryKind, FsError, Mutation, Observation};
use p1_tool_edit_logic::{self as logic, wire};

struct Edit;

impl Guest for Edit {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: logic::NAME.to_owned(),
            description: logic::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(wire::input_schema_json()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::WritesFiles
    }

    fn describe(call: ToolCall) -> CallDescription {
        wire::describe_call(&call)
    }

    fn describe_result(call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        wire::describe_result_call(&call, &tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        wire::execute_call(&Imports, &call)
    }
}

p1_bindings_tool::generated::export!(Edit);

/// The imports this module is linked with, as the logic crate's capabilities.
struct Imports;

/// A held `workspace-mutation`: the gate is released when it drops.
struct Held(workspace_mutation::Mutation);

impl Mutation for Held {
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
        self.0.write(path, contents).map_err(fs_error)
    }
}

impl Capabilities for Imports {
    type Mutation = Held;

    fn cancelled(&self) -> bool {
        control::cancelled()
    }

    fn stat(&self, path: &str) -> Result<Entry, FsError> {
        let entry = workspace::stat(path).map_err(fs_error)?;
        Ok(Entry {
            path: entry.path,
            kind: match entry.kind {
                workspace::EntryKind::File => EntryKind::File,
                workspace::EntryKind::Directory => EntryKind::Directory,
                workspace::EntryKind::Other => EntryKind::Other,
            },
        })
    }

    fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
        workspace::read(path, offset, length).map_err(fs_error)
    }

    fn check(&self, path: &str, current: &[u8]) -> Result<Observation, FsError> {
        let observation = snapshot::check(path, current).map_err(fs_error)?;
        Ok(match observation {
            snapshot::Observation::NeverObserved => Observation::NeverObserved,
            snapshot::Observation::Unchanged => Observation::Unchanged,
            snapshot::Observation::ChangedSinceObserved => Observation::ChangedSinceObserved,
        })
    }

    fn observe(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
        snapshot::observe(path, contents).map_err(fs_error)
    }

    fn begin(&self) -> Held {
        Held(workspace_mutation::begin())
    }
}

/// The WIT `fs-error` as the logic crate's mirror of it.
fn fs_error(error: workspace::FsError) -> FsError {
    match error {
        workspace::FsError::OutsideWorkspace => FsError::OutsideWorkspace,
        workspace::FsError::NotFound => FsError::NotFound,
        workspace::FsError::WrongKind => FsError::WrongKind,
        workspace::FsError::AlreadyExists => FsError::AlreadyExists,
        workspace::FsError::InvalidPattern(message) => FsError::InvalidPattern(message),
        workspace::FsError::Cancelled => FsError::Cancelled,
        workspace::FsError::Io(message) => FsError::Io(message),
    }
}
