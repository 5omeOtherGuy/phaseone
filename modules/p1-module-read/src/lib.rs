//! The `read` tool as a component (`p1/read`, ADR-0071).
//!
//! Everything the tool decides is `p1-read-guest`, the same code the native adapter
//! (`ReadTool` in `p1-tool-read`) runs; this crate binds it to the `tool` world and reads the
//! file through the host. Confinement and the credential refusal (issue #142) are the host's
//! `workspace` capability's, which answers a refused path with the native tool's own text, so
//! this component shows the host's `io` messages unchanged. The observation a later edit is
//! checked against is recorded through `snapshot.observe`, and only after a read succeeded,
//! as the native tool records it.
#![forbid(unsafe_code)]

mod wire;

use p1_bindings_tool::generated::p1::module::control;
use p1_bindings_tool::generated::p1::module::snapshot;
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::p1::module::workspace::{self, EntryKind, FsError};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_read_guest::{NAME, READ_BUFFER_BYTES, ReadInput, WindowedRender};
use wire::Status;

struct Read;

impl Guest for Read {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: NAME.to_owned(),
            description: p1_read_guest::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(p1_read_guest::input_schema().to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }

    fn describe(call: ToolCall) -> CallDescription {
        // Restricted path: from the input alone. An unreadable call has no target.
        let target = serde_json::from_str::<wire::Call>(&call)
            .ok()
            .and_then(|call| p1_read_guest::describe_target(NAME, call.input.raw()));
        wire::text(&wire::CallDescription {
            verb: p1_read_guest::VERB,
            target,
            destructive: false,
        })
    }

    fn describe_result(_call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let (content, ok) = match serde_json::from_str::<wire::ResultItem>(&tool_result) {
            Ok(item) => (item.content, item.status == "ok"),
            Err(_) => (String::new(), false),
        };
        wire::text(&wire::ResultDescription {
            summary: p1_read_guest::describe_result(&content, ok),
        })
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        match run(&call) {
            Ok(content) => wire::outcome(Status::Ok, &content),
            Err(Failure::Message(message)) => wire::outcome(Status::Error, &message),
            Err(Failure::Cancelled) => wire::outcome(Status::Cancelled, ""),
        }
    }
}

p1_bindings_tool::generated::export!(Read);

/// Why a call produced no content.
enum Failure {
    /// The error text the model is shown.
    Message(String),
    /// The call was cancelled.
    Cancelled,
}

impl From<String> for Failure {
    fn from(message: String) -> Self {
        Self::Message(message)
    }
}

fn run(call: &str) -> Result<String, Failure> {
    // Cancellation before any work: touch nothing, not even a stat.
    if control::cancelled() {
        return Err(Failure::Cancelled);
    }
    let call = serde_json::from_str::<wire::Call>(call)
        .map_err(|error| p1_read_guest::invalid(NAME, &error.to_string()))?;
    let input = p1_read_guest::parse_input(NAME, call.input.raw())?;
    let fs = |error| fs_failure(&input.file_path, error);

    let entry = workspace::stat(&input.file_path).map_err(fs)?;
    if entry.kind != EntryKind::File {
        return Err(p1_read_guest::not_a_regular_file(&entry.path).into());
    }
    if entry.size == 0 {
        // An empty file is a successful read of zero bytes: observe it so a later `write`
        // to it is not treated as an unread blind overwrite.
        snapshot::observe(&entry.path, &[]).map_err(fs)?;
        return Ok(p1_read_guest::empty(&entry.path));
    }
    read_whole(&entry.path, entry.size, &input)
}

/// Reads the file window by window from the host's snapshot, rendering as the bytes arrive,
/// and observes exactly the bytes it rendered once the read succeeded. A binary file or an
/// invalid byte stops the read at the window that shows it.
fn read_whole(display: &str, size: u64, input: &ReadInput) -> Result<String, Failure> {
    let fs = |error| fs_failure(display, error);
    let sniffed = p1_read_guest::sniff_len(size);
    let mut contents: Vec<u8> = Vec::new();
    let mut render: Option<WindowedRender> = None;
    loop {
        let chunk = workspace::read(display, contents.len() as u64, READ_BUFFER_BYTES as u64)
            .map_err(fs)?;
        if chunk.is_empty() {
            break;
        }
        contents.extend_from_slice(&chunk);
        match &mut render {
            Some(render) => render.feed(&chunk)?,
            None if contents.len() >= sniffed => {
                let mut started = WindowedRender::start(&contents[..sniffed], display, input)?;
                started.feed(&contents[sniffed..])?;
                render = Some(started);
            }
            None => {}
        }
    }
    let Some(render) = render else {
        // The file shrank below what `stat` reported before its sniff could be read: the
        // native tool's short read says the same.
        return Err(p1_read_guest::could_not_be_read(display, "failed to fill whole buffer").into());
    };
    let output = render.finish()?;
    // A read always observes the FULL file, even when offset/limit windows the returned
    // lines: a later edit compares against the whole file.
    snapshot::observe(display, &contents).map_err(fs)?;
    Ok(output)
}

/// A host refusal as the native tool words it. `io` is already the host's model-facing text.
fn fs_failure(requested: &str, error: FsError) -> Failure {
    match error {
        FsError::Cancelled => Failure::Cancelled,
        FsError::Io(message) => Failure::Message(message),
        FsError::OutsideWorkspace => {
            Failure::Message(p1_read_guest::outside_workspace(requested))
        }
        FsError::NotFound => Failure::Message(p1_read_guest::missing(
            &p1_read_guest::display_of_request(requested),
        )),
        FsError::WrongKind => Failure::Message(p1_read_guest::not_a_regular_file(
            &p1_read_guest::display_of_request(requested),
        )),
        FsError::AlreadyExists | FsError::InvalidPattern(_) => {
            Failure::Message(p1_read_guest::could_not_be_read(
                &p1_read_guest::display_of_request(requested),
                "the host answered a read with an unrelated error",
            ))
        }
    }
}
