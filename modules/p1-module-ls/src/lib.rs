//! The ls component; all filesystem authority belongs to its host import.
#![forbid(unsafe_code)]
use p1_bindings_tool::generated::p1::module::{
    directory_listing as listing, types::DeclarationKind, workspace::FsError,
};
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_tool_ls as guest;
use serde_json::{Value, json};

struct Ls;
impl Guest for Ls {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: guest::NAME.into(),
            description: guest::DESCRIPTION.into(),
            kind: DeclarationKind::Function(guest::input_schema().to_string()),
        }
    }
    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }
    // ADR-0118 Decision 1: a listing always overlaps other reads (`shared`).
    fn describe(call: ToolCall) -> CallDescription {
        let target = input(&call).ok().map(|i| i.path);
        match target {
            Some(target) => {
                json!({"verb":"search","target":target,"destructive":false,"shared":true})
                    .to_string()
            }
            None => json!({"verb":"search","destructive":false,"shared":true}).to_string(),
        }
    }
    fn describe_result(_call: ToolCall, result: HistoryItem) -> ResultDescription {
        let item: Value = serde_json::from_str(&result).unwrap_or(Value::Null);
        json!({"summary": if item["status"] == "ok" { "directory listed" } else { "listing failed" }}).to_string()
    }
    fn execute(call: ToolCall) -> ToolOutcome {
        let result = input(&call)
            .map_err(|message| ("error", message))
            .and_then(|input| {
                listing::list_directory(
                    &input.path,
                    input.depth,
                    input.limit,
                    input.patterns(),
                    input.cursor.as_deref(),
                )
                .map(|page| {
                    guest::render(
                        &guest::Page {
                            entries: page
                                .entries
                                .into_iter()
                                .map(|e| guest::Entry {
                                    path: e.path,
                                    kind: match e.kind {
                                        listing::ListedKind::File => guest::Kind::File,
                                        listing::ListedKind::Directory => guest::Kind::Directory,
                                        listing::ListedKind::Symlink => guest::Kind::Symlink,
                                        listing::ListedKind::Other => guest::Kind::Other,
                                    },
                                    size: e.size,
                                    depth: e.depth,
                                })
                                .collect(),
                            next: page.next,
                            scanned: page.scanned,
                            scan_capped: page.scan_capped,
                        },
                        input.long,
                    )
                })
                .map_err(|error| match error {
                    FsError::Cancelled => ("cancelled", String::new()),
                    FsError::Io(s) | FsError::InvalidPattern(s) => ("error", s),
                    FsError::OutsideWorkspace => ("error", "path escapes workspace".into()),
                    FsError::NotFound => ("error", "no such directory in workspace".into()),
                    FsError::WrongKind => ("error", "not a directory".into()),
                    FsError::AlreadyExists => ("error", "already exists".into()),
                })
            });
        let (status, content) = match result {
            Ok(s) => ("ok", s),
            Err(e) => e,
        };
        json!({"status":status,"content":content}).to_string()
    }
}
fn input(call: &str) -> Result<guest::Input, String> {
    let call: Value = serde_json::from_str(call).map_err(|e| format!("invalid ls call: {e}"))?;
    if call["input"]["kind"] != "json" {
        return Err("ls expects JSON arguments".into());
    }
    let raw = call["input"]["raw"]
        .as_str()
        .ok_or("ls expects JSON arguments")?;
    guest::Input::parse(raw)
}
p1_bindings_tool::generated::export!(Ls);
