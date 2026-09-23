//! Presentation of tool-owned descriptions. The model-facing name is solely a
//! lookup key into the assembled tools, never a classifier of input or output.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{Tool, ToolCall, ToolResultItem, ToolStatus};
use p1_tui::face::{CallFace, FaceBody, GenericDescriber, ResultFace, TargetKind, ToolDescriber};
use p1_tui::render::diff::DiffRow;
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
    match description.detail {
        Some(ResultDetail::Diff {
            path,
            before,
            after,
        }) => {
            // The returned diff describes the replacement; the current file is
            // used only to anchor its surrounding context, never to infer the edit.
            if !before.is_empty() {
                face.body = FaceBody::Diff(diff_rows(workspace, &path, &before, &after));
            }
        }
        Some(ResultDetail::Files { paths }) => {
            face.body = FaceBody::Files(
                paths
                    .into_iter()
                    .map(|line| {
                        let (path, facts) = line.split_once('\t').unwrap_or((&line, ""));
                        (path.to_owned(), facts.to_owned())
                    })
                    .collect(),
            );
        }
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
            // Only explicitly marked presentation lines become an inline body;
            // raw read/workflow text stays available to callers of the contract.
            let mut lines = text.lines();
            if let Some(first) = lines.next() {
                if let Some(target) = first.strip_prefix("target\t") {
                    face.target = Some(target.to_owned());
                    face.body = FaceBody::Lines(lines.map(str::to_owned).collect());
                } else if let Some(line) = first.strip_prefix("lines\t") {
                    face.body = FaceBody::Lines(
                        std::iter::once(line.to_owned())
                            .chain(lines.map(str::to_owned))
                            .collect(),
                    );
                }
            }
        }
        _ => {}
    }
    face
}

fn diff_rows(workspace: &Path, path: &str, before: &str, after: &str) -> Vec<DiffRow> {
    let old_lines: Vec<&str> = before.lines().collect();
    let new_lines: Vec<&str> = after.lines().collect();
    let current = std::fs::read_to_string(workspace.join(path)).ok();
    let current_lines: Vec<&str> = current
        .as_deref()
        .map(str::lines)
        .map(Iterator::collect)
        .unwrap_or_default();
    let anchor = new_lines.first().and_then(|first| {
        current_lines
            .iter()
            .position(|line| line.trim_end() == first.trim_end())
    });
    let mut rows = Vec::new();
    if let Some(a) = anchor
        && a > 0
    {
        rows.push(DiffRow::Context {
            line: a as u32,
            text: current_lines[a - 1].to_string(),
        });
    }
    let first_line = anchor.map(|a| a as u32 + 1).unwrap_or(0);
    for (n, line) in old_lines.iter().enumerate() {
        rows.push(DiffRow::Del {
            line: first_line + n as u32,
            text: (*line).to_string(),
        });
    }
    for (n, line) in new_lines.iter().enumerate() {
        rows.push(DiffRow::Add {
            line: first_line + n as u32,
            text: (*line).to_string(),
        });
    }
    if let Some(a) = anchor
        && let Some(line) = current_lines.get(a + new_lines.len())
    {
        rows.push(DiffRow::Context {
            line: (a + new_lines.len()) as u32 + 1,
            text: (*line).to_string(),
        });
    }
    rows
}

#[cfg(test)]
#[path = "describer/tests.rs"]
mod tests;
