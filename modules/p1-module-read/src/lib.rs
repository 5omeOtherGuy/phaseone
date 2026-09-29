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

const MAX_GUEST_READ_BYTES: u64 = 8 * 1024 * 1024;

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
    // Until the observation capability supports streaming, refuse oversized files before
    // allocating a guest copy. The host snapshot may also hold a copy of this file.
    if exceeds_guest_read_budget(entry.size) {
        return Err(p1_read_guest::could_not_be_read(
            &entry.path,
            "file exceeds the component read budget",
        )
        .into());
    }
    read_whole(&entry.path, entry.size, &input)
}

/// Reads the file window by window from the host's snapshot, rendering as the bytes arrive,
/// and observes exactly the bytes it rendered once the read succeeded. A binary file or an
/// invalid byte stops the read at the window that shows it.
fn read_whole(display: &str, size: u64, input: &ReadInput) -> Result<String, Failure> {
    read_whole_with(
        display,
        size,
        input,
        |offset, length| {
            workspace::read(display, offset, length).map_err(|error| fs_failure(display, error))
        },
        |bytes| snapshot::observe(display, bytes).map_err(|error| fs_failure(display, error)),
        control::cancelled,
    )
}

/// A testable import seam: cancellation is checked before every chunk and before observe.
fn read_whole_with(
    display: &str,
    size: u64,
    input: &ReadInput,
    mut read: impl FnMut(u64, u64) -> Result<Vec<u8>, Failure>,
    mut observe: impl FnMut(&[u8]) -> Result<(), Failure>,
    cancelled: impl Fn() -> bool,
) -> Result<String, Failure> {
    let sniffed = p1_read_guest::sniff_len(size);
    let mut contents: Vec<u8> = Vec::new();
    let mut render: Option<WindowedRender> = None;
    loop {
        if cancelled() {
            return Err(Failure::Cancelled);
        }
        let chunk = read(contents.len() as u64, READ_BUFFER_BYTES as u64)?;
        if chunk.is_empty() {
            break;
        }
        if exceeds_guest_read_budget(contents.len().saturating_add(chunk.len()) as u64) {
            return Err(p1_read_guest::could_not_be_read(
                display,
                "file exceeds the component read budget",
            )
            .into());
        }
        contents.extend_from_slice(&chunk);
        match &mut render {
            Some(render) => render.feed(&chunk)?,
            None if contents.len() >= sniffed => {
                // `stat` can understate a file that grows before this read (notably a
                // zero-byte `stat`), so sniff the bytes actually read: an empty slice would
                // skip the NUL check and let a newly binary file through as text.
                let actual_sniff = p1_read_guest::sniff_len(contents.len() as u64);
                let mut started = WindowedRender::start(&contents[..actual_sniff], display, input)?;
                started.feed(&contents[actual_sniff..])?;
                render = Some(started);
            }
            None => {}
        }
    }
    if contents.is_empty() {
        if cancelled() {
            return Err(Failure::Cancelled);
        }
        observe(&[])?;
        return Ok(p1_read_guest::empty(display));
    }
    let Some(render) = render else {
        // The file shrank below what `stat` reported before its sniff could be read: the
        // native tool's short read says the same.
        return Err(
            p1_read_guest::could_not_be_read(display, "failed to fill whole buffer").into(),
        );
    };
    let output = render.finish()?;
    // A read always observes the FULL file, even when offset/limit windows the returned
    // lines: a later edit compares against the whole file.
    if cancelled() {
        return Err(Failure::Cancelled);
    }
    observe(&contents)?;
    Ok(output)
}

fn exceeds_guest_read_budget(size: u64) -> bool {
    size > MAX_GUEST_READ_BYTES
}

#[cfg(test)]
mod tests {
    use super::{Failure, ReadInput, read_whole_with};
    use std::cell::Cell;

    #[test]
    fn cancellation_after_first_guest_chunk_stops_without_observation() {
        let cancelled = Cell::new(false);
        let reads = Cell::new(0);
        let observed = Cell::new(false);
        let input = ReadInput {
            file_path: "a".into(),
            offset: None,
            limit: None,
        };
        let result = read_whole_with(
            "a",
            4,
            &input,
            |_, _| {
                reads.set(reads.get() + 1);
                cancelled.set(true);
                Ok(b"abc\n".to_vec())
            },
            |_| {
                observed.set(true);
                Ok(())
            },
            || cancelled.get(),
        );
        assert!(matches!(result, Err(Failure::Cancelled)));
        assert_eq!(reads.get(), 1);
        assert!(!observed.get());
    }

    #[test]
    fn zero_size_stat_does_not_observe_empty_when_bytes_arrive() {
        let input = ReadInput {
            file_path: "a".into(),
            offset: None,
            limit: None,
        };
        let contents = Cell::new(Vec::new());
        let result = read_whole_with(
            "a",
            0,
            &input,
            |offset, _| {
                if offset == 0 {
                    Ok(b"later\n".to_vec())
                } else {
                    Ok(Vec::new())
                }
            },
            |bytes| {
                contents.set(bytes.to_vec());
                Ok(())
            },
            || false,
        );
        assert!(result.is_ok());
        assert_eq!(contents.into_inner(), b"later\n");
    }

    #[test]
    fn zero_size_stat_sniffs_the_arriving_bytes_for_nul() {
        let input = ReadInput {
            file_path: "bin".into(),
            offset: None,
            limit: None,
        };
        let observed = Cell::new(false);
        let outcome = read_whole_with(
            "bin",
            0,
            &input,
            |offset, _| {
                if offset == 0 {
                    Ok(b"\0binary".to_vec())
                } else {
                    Ok(Vec::new())
                }
            },
            |_| {
                observed.set(true);
                Ok(())
            },
            || false,
        );
        assert!(
            matches!(outcome, Err(Failure::Message(message)) if message.contains("binary file"))
        );
        assert!(!observed.get());
    }

    #[test]
    fn oversized_guest_chunk_refuses_without_observation() {
        let input = ReadInput {
            file_path: "large".into(),
            offset: None,
            limit: Some(1),
        };
        let observed = Cell::new(false);
        let outcome = read_whole_with(
            "large",
            1,
            &input,
            |_, _| Ok(vec![b'a'; super::MAX_GUEST_READ_BYTES as usize + 1]),
            |_| {
                observed.set(true);
                Ok(())
            },
            || false,
        );
        assert!(
            matches!(outcome, Err(Failure::Message(message)) if message.contains("read budget"))
        );
        assert!(!observed.get());
    }

    #[test]
    fn refuses_oversized_stat_before_guest_buffering() {
        assert!(super::exceeds_guest_read_budget(u64::MAX));
        assert!(!super::exceeds_guest_read_budget(
            super::MAX_GUEST_READ_BYTES
        ));
    }
}

/// A host refusal as the native tool words it. `io` is already the host's model-facing text.
fn fs_failure(requested: &str, error: FsError) -> Failure {
    match error {
        FsError::Cancelled => Failure::Cancelled,
        FsError::Io(message) => Failure::Message(message),
        FsError::OutsideWorkspace => Failure::Message(p1_read_guest::outside_workspace(requested)),
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
