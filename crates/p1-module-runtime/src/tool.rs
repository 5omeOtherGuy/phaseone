//! `WasmTool` (freeze item 12): the one generic adapter from a tool component to
//! `p1_contracts::Tool`. Tool modules implement the `tool` world; none writes an adapter.
//!
//! - `declaration` is read once, at construction, through the restricted path, and cached.
//! - `identity` is the loader's, from the release manifest; the module never reports one.
//! - `effect`, `describe` and `describe_result` go through the restricted path
//!   ([`crate::restricted`]); a failure there yields the worst case, never a panic.
//! - `execute` goes through the executor ([`crate::executor`]), on a fresh instance per call,
//!   honouring `ToolContext.cancel`, with every failure mapped through `ModuleFailure`.
//!
//! The host only ever receives the adapter wrapped in `p1_redact::RedactingTool`
//! ([`wasm_tool`] is the only constructor), so no module output reaches history unmasked.

use std::sync::Arc;

use p1_contracts::serde_json;
use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    BoxFuture, CallDescription, Concurrency, DeclarationKind, Effect, Grammar, Item, Tool,
    ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem,
};
use p1_module_protocol::{
    ModuleFailure, WireCallDescription, WireItem, WireResultDescription, WireToolOutcome,
    WireToolStatus,
};
use p1_redact::{MaskCounter, redacted};
use thiserror::Error;
use wasmtime::component::Val;

use crate::capabilities::{LinkError, Services, capability_linker};
use crate::delegation::{link_subagent_definitions, link_worker_lists};
use crate::executor::{ExecutionLimits, Executor};
use crate::loader::{Epochs, LoadedModule, ModuleKind};
use crate::restricted::Restricted;

/// Why a loaded module could not become a tool.
#[derive(Debug, Error)]
pub enum ToolError {
    /// The module is not of the `tool` class.
    #[error("module {name} is a {kind} module, not a tool")]
    NotATool {
        /// The module.
        name: String,
        /// Its class.
        kind: &'static str,
    },
    /// Its capabilities could not be linked.
    #[error("module {name}: {source}")]
    Link {
        /// The module.
        name: String,
        /// Why.
        source: LinkError,
    },
    /// The component does not fit the `tool` world as linked.
    #[error("module {name} cannot be instantiated: {reason}")]
    Instantiate {
        /// The module.
        name: String,
        /// wasmtime's message.
        reason: String,
    },
    /// `declaration` trapped on the restricted path or returned an invalid declaration.
    #[error("module {name} has no valid declaration: {reason}")]
    Declaration {
        /// The module.
        name: String,
        /// Why.
        reason: String,
    },
    /// No Tokio runtime is current, so the executor has nowhere to run.
    #[error("module {name}: a tool must be built inside a Tokio runtime, which runs its executor")]
    NoRuntime {
        /// The module.
        name: String,
    },
}

/// The generic tool adapter over one loaded tool component.
pub struct WasmTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    restricted: Restricted,
    executor: Executor,
    /// Deadlines advance only while the epoch clock lives; the tool may outlive its loader.
    _epochs: Arc<Epochs>,
}

/// Builds the tool for `module`, linking the capabilities its manifest grants from
/// `services`, and returns it wrapped by [`p1_redact::redacted`] with `counter`: the host
/// never sees an unwrapped module tool. Must be called inside a Tokio runtime, which runs
/// the tool's executor.
pub fn wasm_tool(
    module: &LoadedModule,
    services: Services,
    limits: ExecutionLimits,
    counter: &Arc<MaskCounter>,
) -> Result<Arc<dyn Tool>, ToolError> {
    let tool = WasmTool::new(module, services, limits)?;
    Ok(redacted(Arc::new(tool), counter))
}

impl WasmTool {
    fn new(
        module: &LoadedModule,
        services: Services,
        limits: ExecutionLimits,
    ) -> Result<Self, ToolError> {
        let name = module.name().to_owned();
        if module.kind() != ModuleKind::Tool {
            return Err(ToolError::NotATool {
                name,
                kind: module.kind().name(),
            });
        }
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ToolError::NoRuntime { name: name.clone() })?;
        let instantiate = |error: wasmtime::Error| ToolError::Instantiate {
            name: name.clone(),
            reason: format!("{error:#}"),
        };
        let mut linker = capability_linker(&module.engine, module.capabilities(), &services)
            .map_err(|source| ToolError::Link {
                name: name.clone(),
                source,
            })?;
        // D084/D085: a module granted `workers-observe` also gets the two lists of its worker
        // services, on both paths; `capability_linker` has already refused that grant when the
        // worker services are missing.
        let lists = services
            .workers
            .as_ref()
            .filter(|_| module.capabilities().iter().any(|c| c == "workers-observe"))
            .map(|workers| &workers.lists);
        if let Some(lists) = lists {
            link_worker_lists(&mut linker, lists).map_err(instantiate)?;
        }
        let subagents = services
            .workers
            .as_ref()
            .filter(|_| module.capabilities().iter().any(|c| c == "subagents-start"))
            .map(|workers| &workers.lists);
        if let Some(subagents) = subagents {
            link_subagent_definitions(&mut linker, subagents).map_err(instantiate)?;
        }
        let pre = linker
            .instantiate_pre(&module.component)
            .map_err(instantiate)?;
        let restricted = Restricted::with_worker_lists(
            &module.engine,
            &module.epochs,
            &module.component,
            lists,
            subagents,
        )
        .map_err(instantiate)?;

        let declaration = restricted
            .call("declaration", &[])
            .ok_or_else(|| "the declaration export trapped".to_owned())
            .and_then(|results| declaration(results.first()))
            .map_err(|reason| ToolError::Declaration {
                name: name.clone(),
                reason,
            })?;

        let executor = Executor::start(
            &handle,
            module.engine.clone(),
            module.epochs.clone(),
            pre,
            services,
            limits,
        );
        Ok(Self {
            declaration,
            identity: module.identity().clone(),
            restricted,
            executor,
            _epochs: module.epochs.clone(),
        })
    }
}

/// The wire text of a call, as the module reads it: exactly `WireToolCall`'s JSON.
///
/// Written field by field from the borrowed call instead of through a `WireToolCall`, which
/// owns its strings: a call's input can be a whole history (tens of MiB), and the clone that
/// conversion needs, plus the doublings of a growing buffer, were copies of it the module
/// never sees. The buffer starts at the input's size with room for its escapes, and the
/// literals are written run by run ([`push_literal`]).
fn wire_call(call: &ToolCall) -> String {
    let (kind, raw) = match &call.input {
        ToolInput::Json(raw) => ("json", raw),
        ToolInput::Text(raw) => ("text", raw),
    };
    let size = raw.len() + raw.len() / 4 + call.call_id.len() + call.name.len() + 128;
    let mut text = String::with_capacity(size);
    text.push_str("{\"call_id\":");
    push_literal(&mut text, &call.call_id);
    text.push_str(",\"name\":");
    push_literal(&mut text, &call.name);
    text.push_str(",\"input\":{\"kind\":\"");
    text.push_str(kind);
    text.push_str("\",\"raw\":");
    push_literal(&mut text, raw);
    text.push_str("}}");
    text
}

/// Appends `value` as a JSON string literal with serde_json's escapes: the short escape of
/// a quote, a backslash, `\b`, `\f`, `\n`, `\r` and `\t`, `\u00xx` for every other control
/// character, and nothing else escaped. Runs between two special bytes are copied whole;
/// every special byte is ASCII, so each run is whole characters.
fn push_literal(text: &mut String, value: &str) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let bytes = value.as_bytes();
    text.push('"');
    let mut run = 0;
    while let Some(found) = special(bytes, run) {
        text.push_str(&value[run..found]);
        let byte = bytes[found];
        match byte {
            b'"' => text.push_str("\\\""),
            b'\\' => text.push_str("\\\\"),
            0x08 => text.push_str("\\b"),
            0x0c => text.push_str("\\f"),
            b'\n' => text.push_str("\\n"),
            b'\r' => text.push_str("\\r"),
            b'\t' => text.push_str("\\t"),
            control => {
                text.push_str("\\u00");
                text.push(char::from(HEX[usize::from(control >> 4)]));
                text.push(char::from(HEX[usize::from(control & 0xf)]));
            }
        }
        run = found + 1;
    }
    text.push_str(&value[run..]);
    text.push('"');
}

/// The module's `tool_outcome` text as a `ToolOutcome`, exactly as the protocol's
/// `WireToolOutcome` reads it: unknown fields, unknown statuses and anything not JSON are
/// `InvalidOutput`.
fn read_outcome(text: String) -> Result<ToolOutcome, ModuleFailure> {
    let text = match compact_outcome(text) {
        Ok(read) => return read,
        Err(text) => text,
    };
    serde_json::from_str::<WireToolOutcome>(&text)
        .map(ToolOutcome::from)
        .map_err(|error| ModuleFailure::InvalidOutput(parse_error(&error)))
}

/// The host's own text for an outcome that does not parse: the error's category and
/// position only. Serde's message names unknown fields and variants, which is the guest's
/// text, and protocol.md promises the model none of it.
fn parse_error(error: &serde_json::Error) -> String {
    let category = match error.classify() {
        serde_json::error::Category::Io => "an I/O",
        serde_json::error::Category::Syntax => "a syntax",
        serde_json::error::Category::Data => "a data",
        serde_json::error::Category::Eof => "an end-of-input",
    };
    format!(
        "{category} error at line {} column {}",
        error.line(),
        error.column()
    )
}

/// The outcome of the compact text `{"status":"<status>","content":"<content>"}` whose
/// content has only short escapes (no `\u`), or the text given back, intact, for the
/// protocol's own reader.
///
/// A tool outcome can be a whole history (tens of MiB). The general reader decodes a string
/// with escapes into a growing scratch buffer and copies it into a fresh one; this decodes
/// the content over the text's own buffer, whose pages are already there, where every page
/// of a fresh buffer that size is a page fault. The compact shape is what every module
/// writing compact JSON produces, and what this accepts reads the same through
/// `WireToolOutcome` (a test holds the two readers to that).
fn compact_outcome(text: String) -> Result<Result<ToolOutcome, ModuleFailure>, String> {
    let Some((status, body)) = compact_shape(&text) else {
        return Err(text);
    };
    let (start, end) = (body.start, body.end);
    let bytes = text.as_bytes();
    // First the whole content is checked, so a text this declines is given back intact.
    let mut at = start;
    while let Some(found) = special(&bytes[..end], at) {
        // The letter must lie inside the content: `\"` just before the end would be an
        // escaped closing quote, an unterminated literal.
        let letter = bytes[..end].get(found + 1);
        if bytes[found] != b'\\' || letter.and_then(|&letter| short(letter)).is_none() {
            return Err(text);
        }
        at = found + 2;
    }
    let mut bytes = text.into_bytes();
    let (mut read, mut write) = (start, 0);
    while let Some(found) = special(&bytes[..end], read) {
        bytes.copy_within(read..found, write);
        write += found - read;
        // Checked above: every escape is a short one, one ASCII byte decoded.
        bytes[write] = short(bytes[found + 1]).unwrap_or(b'?');
        write += 1;
        read = found + 2;
    }
    bytes.copy_within(read..end, write);
    bytes.truncate(write + (end - read));
    // Whole runs of a `str` and ASCII bytes are UTF-8 whatever the content was, so this
    // never fails; were it to, the output is refused rather than trusted.
    Ok(String::from_utf8(bytes)
        .map(|content| ToolOutcome {
            status: status.into(),
            content,
        })
        .map_err(|error| ModuleFailure::InvalidOutput(error.to_string())))
}

/// The status and the content's byte range of a compact outcome text, before its content
/// is checked.
fn compact_shape(text: &str) -> Option<(WireToolStatus, std::ops::Range<usize>)> {
    const STATUS: &str = "{\"status\":\"";
    const CONTENT: &str = "\",\"content\":\"";
    const CLOSE: &str = "\"}";
    let rest = text.strip_prefix(STATUS)?;
    let name_end = rest.find(['"', '\\'])?;
    let (name, rest) = rest.split_at(name_end);
    let status: WireToolStatus =
        serde_json::from_value(serde_json::Value::String(name.to_owned())).ok()?;
    if !rest.starts_with(CONTENT) || !rest.ends_with(CLOSE) {
        return None;
    }
    let start = STATUS.len() + name_end + CONTENT.len();
    let end = text.len() - CLOSE.len();
    (start <= end).then_some((status, start..end))
}

/// The byte a short escape `\<letter>` stands for, or `None` for any other letter.
fn short(letter: u8) -> Option<u8> {
    Some(match letter {
        b'"' => b'"',
        b'\\' => b'\\',
        b'/' => b'/',
        b'b' => 0x08,
        b'f' => 0x0c,
        b'n' => b'\n',
        b'r' => b'\r',
        b't' => b'\t',
        _ => return None,
    })
}

/// The index of the first quote, backslash or control byte of `bytes` at or after `from`,
/// testing eight bytes per step: the content between two escapes is usually long.
fn special(bytes: &[u8], from: usize) -> Option<usize> {
    const ONES: u64 = 0x0101_0101_0101_0101;
    const HIGHS: u64 = 0x8080_8080_8080_8080;
    // A high bit where a byte of `word` is zero; exact for the first such byte, which is
    // the one the scan below stops at.
    let zero = |word: u64| word.wrapping_sub(ONES) & !word & HIGHS;
    let (chunks, _) = bytes.get(from..)?.as_chunks::<8>();
    let mut offset = from;
    for &chunk in chunks {
        let word = u64::from_le_bytes(chunk);
        let hit = zero(word ^ (ONES * u64::from(b'"')))
            | zero(word ^ (ONES * u64::from(b'\\')))
            // A byte below 0x20 has its three top bits clear.
            | zero(word & (ONES * 0xe0));
        if hit != 0 {
            break;
        }
        offset += 8;
    }
    bytes[offset..]
        .iter()
        .position(|&byte| byte == b'"' || byte == b'\\' || byte < 0x20)
        .map(|found| offset + found)
}

/// The single string result of a restricted call, if it returned one.
fn string_result(results: Option<Vec<Val>>) -> Option<String> {
    match results?.into_iter().next()? {
        Val::String(text) => Some(text),
        _ => None,
    }
}

/// Reads the `declaration` record.
fn declaration(value: Option<&Val>) -> Result<ToolDeclaration, String> {
    let Some(Val::Record(fields)) = value else {
        return Err("declaration did not return a record".to_owned());
    };
    let field = |key: &str| {
        fields
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value)
    };
    let text = |key: &str| match field(key) {
        Some(Val::String(text)) => Ok(text.clone()),
        _ => Err(format!("declaration.{key} is not a string")),
    };
    let kind = match field("kind") {
        Some(Val::Variant(case, payload)) => match (case.as_str(), payload.as_deref()) {
            ("function", Some(Val::String(schema))) => DeclarationKind::Function {
                input_schema: serde_json::from_str(schema)
                    .map_err(|error| format!("the input schema is not JSON: {error}"))?,
            },
            ("freeform", Some(Val::Option(grammar))) => DeclarationKind::Freeform {
                grammar: match grammar.as_deref() {
                    None => None,
                    Some(Val::Record(grammar)) => {
                        let part = |key: &str| {
                            grammar
                                .iter()
                                .find_map(|(name, value)| match value {
                                    Val::String(text) if name == key => Some(text.clone()),
                                    _ => None,
                                })
                                .ok_or_else(|| format!("grammar.{key} is not a string"))
                        };
                        Some(Grammar {
                            syntax: part("syntax")?,
                            definition: part("definition")?,
                        })
                    }
                    Some(_) => return Err("the grammar is not a record".to_owned()),
                },
            },
            _ => return Err(format!("declaration.kind {case} is not a known kind")),
        },
        _ => return Err("declaration.kind is not a variant".to_owned()),
    };
    Ok(ToolDeclaration {
        name: text("name")?,
        description: text("description")?,
        kind,
    })
}

/// What a failed `describe` yields: the neutral verb and nothing the module said,
/// destructive, because a call the module could not describe fails closed (owner decision
/// 2026-10-01): the approval floor holds and no persistent grant covers it.
fn empty_description() -> CallDescription {
    CallDescription {
        verb: "call",
        target: None,
        edit: None,
        destructive: true,
    }
}

/// ADR-0118: what a `describe` text says about overlapping. Shared only for a valid call
/// description whose `shared` is true; an absent or false flag, a malformed text and a
/// failed call are Exclusive.
fn concurrency_of(described: Option<&str>) -> Concurrency {
    match described.and_then(|text| serde_json::from_str::<WireCallDescription>(text).ok()) {
        Some(description) if description.shared => Concurrency::Shared,
        _ => Concurrency::Exclusive,
    }
}

impl Tool for WasmTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, call: &ToolCall) -> Effect {
        let results = self
            .restricted
            .call("effect", &[Val::String(wire_call(call))]);
        match results.and_then(|results| results.into_iter().next()) {
            Some(Val::Enum(case)) => match case.as_str() {
                "read-only" => Effect::ReadOnly,
                "writes-files" => Effect::WritesFiles,
                "delegates" => Effect::Delegates,
                // `executes`, and the worst case for anything unreadable.
                _ => Effect::Executes,
            },
            _ => Effect::Executes,
        }
    }

    fn describe(&self, call: &ToolCall) -> CallDescription {
        let results = self
            .restricted
            .call("describe", &[Val::String(wire_call(call))]);
        string_result(results)
            .and_then(|text| serde_json::from_str::<WireCallDescription>(&text).ok())
            .map(CallDescription::from)
            .unwrap_or_else(empty_description)
    }

    /// ADR-0118: the module's `shared` flag in the same `describe` record. A failed or
    /// malformed `describe` and an absent flag are Exclusive: a call the module could not
    /// describe runs alone.
    fn concurrency(&self, call: &ToolCall) -> Concurrency {
        let results = self
            .restricted
            .call("describe", &[Val::String(wire_call(call))]);
        concurrency_of(string_result(results).as_deref())
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let item = serde_json::to_string(&WireItem::from(Item::ToolResult(result.clone()))).ok();
        let described = item
            .and_then(|item| {
                string_result(self.restricted.call(
                    "describe-result",
                    &[Val::String(wire_call(call)), Val::String(item)],
                ))
            })
            .and_then(|text| serde_json::from_str::<WireResultDescription>(&text).ok())
            .and_then(|wire| ResultDescription::try_from(wire).ok());
        // The host's own summary when the module's failed: the first line of what the model
        // was shown, as the trait's default gives.
        described.unwrap_or_else(|| ResultDescription {
            summary: result.content.lines().next().unwrap_or_default().to_owned(),
            detail: None,
        })
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let results = self
                .executor
                .call(
                    "execute",
                    vec![Val::String(wire_call(call))],
                    context.cancel,
                )
                .await;
            let outcome = results.and_then(|results| {
                let text = string_result(Some(results)).ok_or_else(|| {
                    ModuleFailure::InvalidOutput("execute returned no text".to_owned())
                })?;
                read_outcome(text)
            });
            outcome.unwrap_or_else(ModuleFailure::into_tool_outcome)
        })
    }
}

#[cfg(test)]
mod tests {
    use p1_module_protocol::WireToolCall;

    use super::*;

    /// ADR-0118 test 9: `shared` in the describe record, absent, false, true and malformed.
    #[test]
    fn concurrency_comes_from_the_shared_flag_and_fails_closed() {
        for (text, expected) in [
            (
                Some(r#"{"verb":"read","destructive":false,"shared":true}"#),
                Concurrency::Shared,
            ),
            (
                Some(r#"{"verb":"read","destructive":false,"shared":false}"#),
                Concurrency::Exclusive,
            ),
            (
                Some(r#"{"verb":"read","destructive":false}"#),
                Concurrency::Exclusive,
            ),
            (
                Some(r#"{"verb":"read","destructive":false,"shared":null}"#),
                Concurrency::Exclusive,
            ),
            (
                Some(r#"{"verb":"read","destructive":false,"shared":"yes"}"#),
                Concurrency::Exclusive,
            ),
            (
                Some(r#"{"verb":"read","shared":true}"#),
                Concurrency::Exclusive,
            ),
            (Some("not json"), Concurrency::Exclusive),
            (None, Concurrency::Exclusive),
        ] {
            assert_eq!(concurrency_of(text), expected, "{text:?}");
        }
    }

    /// The hand-written call text is the protocol's own serialization, byte for byte, for
    /// both input kinds and for text every escape of JSON touches, each special byte in
    /// every lane of the eight-byte scan.
    #[test]
    fn the_call_text_is_the_wire_tool_calls_json() {
        let awkward = "quote\" backslash\\ newline\n tab\t nul\u{0} del\u{7f} \u{2028} \u{1f600}";
        let mut inputs = vec![
            ToolInput::Text("echo:hi".to_owned()),
            ToolInput::Json("{\"path\":\"src/lib.rs\"}".to_owned()),
            ToolInput::Text(awkward.to_owned()),
            ToolInput::Json(String::new()),
        ];
        for pad in 0..9 {
            let every: String = (0u8..0x80).map(char::from).collect();
            inputs.push(ToolInput::Text(format!(
                "{}{every}{every}",
                "x".repeat(pad)
            )));
        }
        for input in inputs {
            let call = ToolCall {
                call_id: format!("c1 {awkward}"),
                name: "fixture\"".to_owned(),
                input,
            };
            let expected = serde_json::to_string(&WireToolCall::from(call.clone()))
                .expect("a wire call serializes");
            assert_eq!(wire_call(&call), expected);
        }
    }

    /// The protocol's own reading of an outcome text, its error in the host's words.
    fn general(text: &str) -> Result<ToolOutcome, String> {
        serde_json::from_str::<WireToolOutcome>(text)
            .map(ToolOutcome::from)
            .map_err(|error| parse_error(&error))
    }

    /// Every text the compact reader accepts reads the same through `WireToolOutcome`, and
    /// every text it declines is left to that reader, errors included; the escapes and
    /// special bytes sit in every lane of its eight-byte scan.
    #[test]
    fn the_compact_reader_reads_what_the_protocol_reads() {
        let mut contents: Vec<String> = Vec::new();
        for pad in 0..9 {
            let x = "x".repeat(pad);
            for piece in [
                r#"\""#,
                r"\\",
                r"\/",
                r"\b",
                r"\f",
                r"\n",
                r"\r",
                r"\t",
                r"\u0041",
                r"\ud83d\ude00",
                r"\ud83d",
                r"\q",
                "\"",
                "\u{1}",
                "\u{7f}",
                "\u{e9}",
                "\u{1f600}",
                r"\",
            ] {
                contents.push(format!("{x}{piece}{x}y"));
                contents.push(format!("{x}{piece}"));
            }
        }
        contents.extend(["", "plain text"].map(str::to_owned));
        let mut compact = 0;
        for content in &contents {
            for status in [
                "ok",
                "error",
                "cancelled",
                "unknown",
                "maybe",
                "OK",
                "o\\u006b",
            ] {
                let text = format!("{{\"status\":\"{status}\",\"content\":\"{content}\"}}");
                let spaced = format!("{{ \"status\": \"{status}\", \"content\": \"{content}\" }}");
                let reordered = format!("{{\"content\":\"{content}\",\"status\":\"{status}\"}}");
                let extra =
                    format!("{{\"status\":\"{status}\",\"content\":\"{content}\",\"x\":1}}");
                for text in [text, spaced, reordered, extra] {
                    let expected = general(&text);
                    match compact_outcome(text.clone()) {
                        Ok(read) => {
                            compact += 1;
                            let outcome = read.expect("a compact text is read");
                            assert_eq!(Ok(outcome), expected, "{text}");
                        }
                        Err(back) => {
                            // Declined: the text comes back intact, and the reader the
                            // runtime falls back to gives the answer.
                            assert_eq!(back, text);
                            let read =
                                read_outcome(text.clone()).map_err(|failure| match failure {
                                    ModuleFailure::InvalidOutput(message) => message,
                                    other => panic!("{other:?}"),
                                });
                            assert_eq!(read, expected, "{text}");
                        }
                    }
                }
            }
        }
        // The compact shape with short escapes is read by the compact reader.
        assert!(compact > contents.len(), "{compact}");
        assert_eq!(
            compact_outcome(r#"{"status":"ok","content":"a\"b\\c\nd"}"#.to_owned()),
            Ok(Ok(ToolOutcome::ok("a\"b\\c\nd")))
        );
    }

    /// protocol.md: an invalid-output message is the host's, never a copy of the output.
    /// Serde names unknown fields and variants, so its text must not reach the model.
    #[test]
    fn an_invalid_outcome_echoes_no_guest_text() {
        let marker = "PRIVATEMARKER";
        for text in [
            format!(r#"{{"status":"ok","content":"x","{marker}":1}}"#),
            format!(r#"{{"status":"{marker}","content":"x"}}"#),
            format!(r#"{{"status":"ok","content":["{marker}"]}}"#),
            format!(r#"{{"status":"ok","{marker}""#),
            format!("{marker} is not JSON"),
        ] {
            let failure = read_outcome(text.clone()).expect_err(&text);
            assert!(
                matches!(&failure, ModuleFailure::InvalidOutput(_)),
                "{failure:?}"
            );
            let outcome = failure.into_tool_outcome();
            assert!(!outcome.content.contains(marker), "{}", outcome.content);
            assert!(outcome.content.contains("line 1"), "{}", outcome.content);
        }
    }
}
