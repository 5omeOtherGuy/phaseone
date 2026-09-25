//! Presentation of tool-owned descriptions. The model-facing name is solely a
//! lookup key into the assembled tools, never a classifier of input or output.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{Tool, ToolCall, ToolResultItem, ToolStatus};
use p1_tui::face::{CallFace, FaceBody, GenericDescriber, ResultFace, TargetKind, ToolDescriber};
use p1_tui::wrap::{cell_width, fit_cells};

const FACT_CELLS: usize = 40;

pub struct HostDescriber {
    workspace: PathBuf,
    sandbox: String,
    tools: Arc<Vec<Arc<dyn Tool>>>,
}

impl HostDescriber {
    pub fn new(workspace: PathBuf, sandbox: String, tools: Arc<Vec<Arc<dyn Tool>>>) -> Self {
        Self {
            workspace,
            sandbox,
            tools,
        }
    }

    fn tool(&self, call: &ToolCall) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|tool| tool.declaration().name == call.name)
            .map(|tool| tool.as_ref())
    }
}

impl ToolDescriber for HostDescriber {
    fn call(&self, call: &ToolCall) -> CallFace {
        let Some(tool) = self.tool(call) else {
            return GenericDescriber.call(call);
        };
        let description = tool.describe(call);
        let kind = match description.verb {
            "edit"
                if description
                    .target
                    .as_deref()
                    .is_some_and(|target| target.ends_with(" files")) =>
            {
                TargetKind::Plain
            }
            "read" | "edit" | "write" => TargetKind::Path,
            "run" => TargetKind::Command,
            _ => TargetKind::Plain,
        };
        CallFace {
            target: description.target.unwrap_or_default(),
            kind,
        }
    }

    fn result(
        &self,
        call: &ToolCall,
        result: &ToolResultItem,
        elapsed_ms: Option<u64>,
    ) -> ResultFace {
        let Some(tool) = self.tool(call) else {
            return GenericDescriber.result(call, result, elapsed_ms);
        };
        if !matches!(result.status, ToolStatus::Ok | ToolStatus::Error) {
            return empty_face();
        }
        let description = tool.describe_result(call, result);
        if result.status == ToolStatus::Error {
            return ResultFace {
                outcome: Some(bound(&description.summary)),
                ..empty_face()
            };
        }
        render_result(&self.workspace, &self.sandbox, description, elapsed_ms)
    }
}

fn empty_face() -> ResultFace {
    ResultFace {
        outcome: None,
        body: FaceBody::None,
        meta: None,
        target: None,
    }
}

fn bound(text: &str) -> String {
    if cell_width(text) > FACT_CELLS {
        format!("{}…", fit_cells(text, FACT_CELLS - 1))
    } else {
        text.to_owned()
    }
}

fn render_result(
    workspace: &Path,
    sandbox: &str,
    description: ResultDescription,
    elapsed_ms: Option<u64>,
) -> ResultFace {
    let mut face = ResultFace {
        outcome: Some(description.summary),
        ..empty_face()
    };
    // An ok call renders ONE row (handoff §7.3, owner 2026-09-24), so its body
    // would never be drawn and is not built here. Only the facts that ride in
    // band A survive: the shell's elapsed and sandbox posture, and the
    // result-time target (`worker_start`'s `w1 · env/profile`).
    match description.detail {
        Some(ResultDetail::Command {
            elapsed_ms: tool_elapsed,
            ..
        }) => {
            if let Some(ms) = elapsed_ms.or(tool_elapsed) {
                face.outcome = Some(format!(
                    "{} · {}",
                    p1_tui::render::elapsed(ms),
                    face.outcome.unwrap_or_default()
                ));
            }
            if sandbox != "off" {
                face.meta = Some(format!("cwd {} · {sandbox}", workspace.display()));
            }
        }
        Some(ResultDetail::Text(text)) if !text.is_empty() => {
            // Only the explicitly marked target line is presentation; raw read
            // and workflow text stays available to callers of the contract.
            if let Some(target) = text
                .lines()
                .next()
                .and_then(|first| first.strip_prefix("target\t"))
            {
                face.target = Some(target.to_owned());
            }
        }
        _ => {}
    }
    face
}

#[cfg(test)]
#[path = "describer/tests.rs"]
mod tests;
