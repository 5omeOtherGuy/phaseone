//! The Codex `apply_patch` tool as a component (`p1/patch-codex`, ADR-0219): Codex's
//! description, grammar and result text (`p1-patch-codex-guest`) over the patch logic
//! `p1/patch` runs (`p1-tool-patch-logic`), with the same capability adapters.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::types::{DeclarationKind, Grammar};
use p1_bindings_tool::generated::p1::module::{clock, control, workspace, workspace_mutation};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_patch_codex_guest as face;
use p1_tool_patch_logic::{self as logic, Outcome, Status};

struct Patch;

impl Guest for Patch {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: face::NAME.to_owned(),
            description: face::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Freeform(Some(Grammar {
                syntax: face::GRAMMAR_SYNTAX.to_owned(),
                definition: face::GRAMMAR.to_owned(),
            })),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::WritesFiles
    }

    fn describe(call: ToolCall) -> CallDescription {
        logic::wire::describe_call(&call)
    }

    fn describe_result(call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        logic::wire::describe_result_call(&call, &tool_result)
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        let started = clock::now();
        let outcome = match logic::parse_call(&call) {
            Ok(call) => {
                logic::execute(&mut Capabilities, &call.name, call.freeform(), call.input())
            }
            Err(error) => Outcome {
                status: Status::Error,
                content: format!("Invalid tool call for {}: {error}", face::NAME),
            },
        };
        let outcome = match outcome.status {
            Status::Ok => Outcome {
                status: Status::Ok,
                content: face::success(&outcome.content, elapsed_ms(started)),
            },
            Status::Error => Outcome {
                status: Status::Error,
                content: face::verification_failed(&outcome.content),
            },
            Status::Cancelled => outcome,
        };
        logic::outcome_json(&outcome)
    }
}

p1_bindings_tool::generated::export!(Patch);

fn elapsed_ms(started: clock::Datetime) -> u64 {
    let now = clock::now();
    let start = u128::from(started.seconds) * 1_000 + u128::from(started.nanoseconds / 1_000_000);
    let end = u128::from(now.seconds) * 1_000 + u128::from(now.nanoseconds / 1_000_000);
    end.saturating_sub(start).min(u128::from(u64::MAX)) as u64
}

struct Capabilities;

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
