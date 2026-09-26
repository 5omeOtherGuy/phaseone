//! The `apply_patch` tool component (`p1/patch`): V4A patches confined to the workspace.
//!
//! A package of the module workspace (its frozen manifest fields are in `Cargo.toml`, the
//! format in `docs/design/modules/package.md`) implementing the `tool` world of
//! `modules/wit/`. Everything the tool decides — input validation, the parser, the plan of
//! every change, the descriptions and every text the model sees — is `p1-tool-patch-logic`,
//! the crate the native `p1-tool-patch` calls too (S0-R3). This crate only binds that logic
//! to the world: the JSON value families of `docs/design/modules/protocol.md` in and out,
//! and the imported capabilities behind the logic's `Host`.
//!
//! - The declaration is the native default face: freeform, with the V4A grammar. A host
//!   that presents the tool as a function tool (the native `function_face`) sends JSON
//!   `{"patch": …}` input, and the input kind picks the form a call is read in.
//! - `effect`, `describe` and `describe-result` work from their arguments alone: they run
//!   on the restricted path, where any import traps. `describe` therefore decides lexically
//!   whether a path escapes (`logic::lexically_confined`); `execute` enforces confinement
//!   by the capability.
//! - `execute` validates the input, parses the patch, reads every file it needs through
//!   `workspace` and computes every resulting file before it takes the write gate; then it
//!   `begin`s a mutation and applies the planned changes, one per resolved path, so a path
//!   the patch reaches twice is written once with its final contents. It records nothing
//!   itself: the host records every file a change wrote in the agent's observations as it
//!   applies it, which is why this component imports no `snapshot` (ADR-0088 point 4,
//!   `docs/design/modules/workspace-mutation.md`: "patch links no snapshot"). It checks no
//!   observation either: the patch exemption, which the host grants by assembling the
//!   mutation patch-authorized. It checks `control.cancelled()` between hunks, between read
//!   windows and between changes. No thread is started: the component has none.
//!
//! The guest has no WASI surface (D-XO-4): no preopened directory and no descriptor, so
//! every file operation is a host import that the host confines.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::types::{DeclarationKind, Grammar};
use p1_bindings_tool::generated::p1::module::{control, workspace, workspace_mutation};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_tool_patch_logic::{self as logic, Outcome, Status};

struct Patch;

impl Guest for Patch {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: logic::NAME.to_owned(),
            description: logic::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Freeform(Some(Grammar {
                syntax: logic::GRAMMAR_SYNTAX.to_owned(),
                definition: logic::PATCH_GRAMMAR.to_owned(),
            })),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        // Every call writes files; invalid input is classified at that worst case too.
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
            Ok(call) => {
                logic::execute(&mut Capabilities, &call.name, call.freeform(), call.input())
            }
            Err(error) => Outcome {
                status: Status::Error,
                content: format!("Invalid tool call for {}: {error}", logic::NAME),
            },
        };
        logic::outcome_json(&outcome)
    }
}

p1_bindings_tool::generated::export!(Patch);

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

    fn begin(&mut self) -> HeldGate {
        HeldGate(workspace_mutation::begin())
    }
}

impl logic::Mutation for HeldGate {
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), logic::FsError> {
        self.0.write(path, contents).map_err(fs_error)
    }

    fn create(&self, path: &str, contents: &[u8]) -> Result<(), logic::FsError> {
        self.0.create(path, contents).map_err(fs_error)
    }

    fn remove(&self, path: &str) -> Result<(), logic::FsError> {
        self.0.remove(path).map_err(fs_error)
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
