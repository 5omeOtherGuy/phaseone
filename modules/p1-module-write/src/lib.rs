//! The `write` tool component (`p1/write`): atomic create-or-replace of one workspace file.
//!
//! A package of the module workspace (its frozen manifest fields are in `Cargo.toml`, the
//! format in `docs/design/modules/package.md`) implementing the `tool` world of
//! `modules/wit/`. Everything the tool decides — input validation, the descriptions, the
//! order of the execute flow and every text the model sees — is `p1-tool-write-logic`, the
//! crate the native `p1-tool-write` calls too (S0-R3). This crate only binds that logic to
//! the world: the JSON value families of `docs/design/modules/protocol.md` in and out, and
//! the four imported capabilities behind the logic's `Host`.
//!
//! - `effect`, `describe` and `describe-result` work from their arguments alone: they run on
//!   the restricted path, where any import traps. `describe` therefore decides lexically
//!   whether a path escapes (`logic::lexically_confined`): an absolute path, even one the
//!   root would hold, is shown destructive, and a relative path through a symlink that
//!   escapes is not. That worst-case lexical answer is the component's contract until a
//!   capability can resolve at describe time — U-write.2/U-desc pins it against the
//!   production loader — while `execute` enforces confinement again by the capability.
//! - `execute` validates the input, reads an existing target through `workspace` and checks
//!   this agent's observation through `snapshot` before it takes the write gate, then
//!   `begin`s a mutation, writes, and records the new contents while the gate is held. The
//!   host rechecks the observation under the gate. Between the long steps it checks
//!   `control.cancelled()`. No thread is started: the component has none.
//!
//! The guest has no WASI surface (D-XO-4): no preopened directory and no descriptor, so
//! every file operation is a host import that the host confines.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::{control, snapshot, workspace, workspace_mutation};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_tool_write_logic::{self as logic, Outcome, Status};

struct Write;

impl Guest for Write {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: logic::NAME.to_owned(),
            description: logic::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(logic::input_schema().to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        // Every call writes a file; invalid input is classified at that worst case too.
        CallEffect::WritesFiles
    }

    fn describe(call: ToolCall) -> CallDescription {
        // The logic crate's wire helper is the one body: this export, the native suite and
        // the U-desc suite all answer from it (S0-R3).
        logic::wire::describe_call(&call)
    }

    fn describe_result(call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        logic::wire::describe_result_call(&call, &tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let outcome = match logic::parse_call(&call) {
            // Invalid input names the tool as the model called it, as the native tool names
            // its presented face.
            Ok(call) => logic::execute(&mut Capabilities, &call.name, call.input()),
            Err(error) => Outcome {
                status: Status::Error,
                content: format!("Invalid tool call for {}: {error}", logic::NAME),
            },
        };
        logic::outcome_json(&outcome)
    }
}

p1_bindings_tool::generated::export!(Write);

/// The imported capabilities, as the logic's `Host`.
struct Capabilities;

/// The `mutation` resource: the write gate, held until this value is dropped.
struct HeldGate(workspace_mutation::Mutation);

impl logic::Host for Capabilities {
    type Mutation = HeldGate;

    fn cancelled(&mut self) -> bool {
        control::cancelled()
    }

    fn stat(&mut self, path: &str) -> Result<logic::Entry, logic::FsError> {
        let entry = workspace::stat(path).map_err(fs_error)?;
        Ok(logic::Entry {
            path: entry.path,
            kind: match entry.kind {
                workspace::EntryKind::File => logic::EntryKind::File,
                workspace::EntryKind::Directory => logic::EntryKind::Directory,
                workspace::EntryKind::Other => logic::EntryKind::Other,
            },
            size: entry.size,
        })
    }

    fn read(&mut self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, logic::FsError> {
        workspace::read(path, offset, length).map_err(fs_error)
    }

    fn check(&mut self, path: &str, current: &[u8]) -> Result<logic::Observation, logic::FsError> {
        let observation = snapshot::check(path, current).map_err(fs_error)?;
        Ok(match observation {
            snapshot::Observation::NeverObserved => logic::Observation::NeverObserved,
            snapshot::Observation::Unchanged => logic::Observation::Unchanged,
            snapshot::Observation::ChangedSinceObserved => logic::Observation::ChangedSinceObserved,
        })
    }

    fn observe(&mut self, path: &str, contents: &[u8]) -> Result<(), logic::FsError> {
        snapshot::observe(path, contents).map_err(fs_error)
    }

    fn begin(&mut self) -> HeldGate {
        HeldGate(workspace_mutation::begin())
    }
}

impl logic::Mutation for HeldGate {
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), logic::FsError> {
        self.0.write(path, contents).map_err(fs_error)
    }
}

/// The WIT `fs-error` as the logic's error, case by case.
fn fs_error(error: workspace::FsError) -> logic::FsError {
    match error {
        workspace::FsError::OutsideWorkspace => logic::FsError::OutsideWorkspace,
        workspace::FsError::NotFound => logic::FsError::NotFound,
        workspace::FsError::WrongKind => logic::FsError::WrongKind,
        workspace::FsError::AlreadyExists => logic::FsError::AlreadyExists,
        workspace::FsError::InvalidPattern(message) => logic::FsError::InvalidPattern(message),
        workspace::FsError::Cancelled => logic::FsError::Cancelled,
        workspace::FsError::Io(message) => logic::FsError::Io(message),
    }
}
