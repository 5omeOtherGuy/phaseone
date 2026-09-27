//! The search tool module (`p1/search`): regex content and file search over the workspace.
//!
//! A package of the module workspace (its frozen manifest fields are in `Cargo.toml`, the
//! format in `docs/design/modules/package.md`) implementing the `tool` world of
//! `modules/wit/`. Everything it decides is in `p1-tool-search-logic`, the crate the native
//! `p1-tool-search` runs too, so the texts the model sees cannot drift between the two. This
//! crate only adapts the WIT imports to that crate's [`Capabilities`] and hands each export's
//! argument text to it.
//!
//! `effect`, `describe` and `describe-result` work from their arguments alone: the host calls
//! the first two on the restricted path, where any import traps. `execute` validates the
//! input and asks the host for the walk (`workspace.search`, `workspace.list-files`, the
//! native serial walk and ripgrep matching) and renders the model-facing text from what it
//! returns; it checks `control.cancelled()` between host calls. The guest has no thread:
//! every wait is a host import it is suspended in.
//!
//! Search links no `snapshot` and no `workspace-mutation`: its reads record no observation
//! and it never takes the write gate, so it can never give an agent edit permission. Like
//! every module it imports no `wasi:` interface (decision D-XO-4): the built component
//! imports only `control`, `workspace` and the type-only `types`.
#![forbid(unsafe_code)]

use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::{control, workspace};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_tool_search_logic::exec::{
    Capabilities, Entry, EntryKind, FileMatches, FsError, SearchLine, SearchQuery, SearchResult,
};
use p1_tool_search_logic::{self as logic, wire};

struct Search;

impl Guest for Search {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: logic::NAME.to_owned(),
            description: logic::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(wire::input_schema_json()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
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

p1_bindings_tool::generated::export!(Search);

/// The imports this module is linked with, as the logic crate's capabilities.
struct Imports;

impl Capabilities for Imports {
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

    fn list_files(&self, path: &str, glob: Option<&str>) -> Result<Vec<String>, FsError> {
        workspace::list_files(path, glob).map_err(fs_error)
    }

    fn search(&self, query: &SearchQuery) -> Result<SearchResult, FsError> {
        let result = workspace::search(&workspace::SearchQuery {
            pattern: query.pattern.clone(),
            path: query.path.clone(),
            glob: query.glob.clone(),
            case_insensitive: query.case_insensitive,
            context: query.context,
            max_lines: query.max_lines,
        })
        .map_err(fs_error)?;
        Ok(SearchResult {
            files: result
                .files
                .into_iter()
                .map(|file| FileMatches {
                    path: file.path,
                    lines: file
                        .lines
                        .into_iter()
                        .map(|line| SearchLine {
                            line_number: line.line_number,
                            text: line.text,
                            is_match: line.is_match,
                        })
                        .collect(),
                })
                .collect(),
            truncated: result.truncated,
            omitted_files: result.omitted_files,
        })
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
