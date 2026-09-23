//! Presentation seam for tool-specific facts. The host owns tool knowledge; the TUI only paints faces.

use p1_contracts::{ToolCall, ToolResultItem};

use crate::render::diff::DiffRow;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetKind {
    Plain,
    Path,
    Command,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallFace {
    pub target: String,
    pub kind: TargetKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultFace {
    pub outcome: Option<String>,
    pub body: FaceBody,
    pub meta: Option<String>,
    /// Replaces the frozen call target once the result settles (`worker_start`'s
    /// `w1 · env/profile`: `w1` and the resolved profile only exist at the
    /// result, after `CallFace` was already built and drawn). `None` keeps the
    /// call-time target.
    pub target: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FaceBody {
    None,
    Lines(Vec<String>),
    Diff(Vec<DiffRow>),
    Files(Vec<(String, String)>),
}

pub trait ToolDescriber: Send + Sync {
    fn call(&self, call: &ToolCall) -> CallFace;
    /// `elapsed_ms` is the call's own clock (`ToolStarted` to `ToolFinished`),
    /// the same measurement `ToolRow.elapsed_ms` carries — the describer never
    /// keeps its own clock (§7.3 shell: `D · exit C · N lines`).
    fn result(
        &self,
        call: &ToolCall,
        result: &ToolResultItem,
        elapsed_ms: Option<u64>,
    ) -> ResultFace;
}

/// Name-agnostic fallback for tools the host has not described.
#[derive(Debug, Default)]
pub struct GenericDescriber;

impl ToolDescriber for GenericDescriber {
    fn call(&self, call: &ToolCall) -> CallFace {
        let raw = call.input.raw();
        CallFace {
            target: salient_string(raw).unwrap_or_else(|| raw.to_owned()),
            kind: TargetKind::Plain,
        }
    }

    fn result(
        &self,
        _call: &ToolCall,
        result: &ToolResultItem,
        _elapsed_ms: Option<u64>,
    ) -> ResultFace {
        let count = result.content.lines().count();
        ResultFace {
            outcome: Some(format!("{count} lines")),
            body: FaceBody::None,
            meta: None,
            target: None,
        }
    }
}

fn salient_string(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] != b'"' {
            i += 1;
            continue;
        }
        i += 1;
        let mut escaped = false;
        while i < bytes.len() {
            match bytes[i] {
                b'\\' if !escaped => escaped = true,
                b'"' if !escaped => break,
                _ => escaped = false,
            }
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        i += 1;
        let after = raw[i..].trim_start();
        if let Some(after_colon) = after.strip_prefix(':') {
            let value_start = after_colon.trim_start();
            if let Some(value) = value_start.strip_prefix('"') {
                let end = value.find('"')?;
                return Some(value[..end].replace("\\n", "␤"));
            }
        }
    }
    None
}
