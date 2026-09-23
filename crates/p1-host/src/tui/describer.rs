//! `HostDescriber`: the ONE place a tool name is matched (handoff §7.1, §7.3).
//! `p1-tui` never sees a tool name — it draws whatever `CallFace`/`ResultFace`
//! this module hands it. Every input shape below is copied from the matching
//! `p1-tool-*` crate's own `*Input` struct (private to that crate, so it is
//! mirrored here field-for-field rather than imported) and every output shape
//! from that crate's own `Ok(format!(...))`/error text, read from its source.
//!
//! A fact this module cannot derive from the call and the result alone is
//! OMITTED, never guessed (§7.3 last paragraph) — documented inline at each
//! such gap (`write`'s new/replaced, `shell`'s elapsed, `apply_patch`'s
//! removed-line count for a `Delete File` hunk, …).

use std::path::{Path, PathBuf};

use p1_contracts::{ToolCall, ToolInput, ToolResultItem, ToolStatus};
use p1_tui::face::{CallFace, FaceBody, GenericDescriber, ResultFace, TargetKind, ToolDescriber};
use p1_tui::render::diff::DiffRow;
use p1_tui::wrap::{cell_width, fit_cells};
use serde::Deserialize;
use serde::de::DeserializeOwned;

/// §7.1: outcome facts bounded to this many cells before `…` (the generic rule,
/// reused everywhere this module falls back to "the first output line").
const FACT_CELLS: usize = 40;
/// §7.3 edit: up to this many diff rows inline before folding.
/// `read`'s schema default for `limit` (`p1-tool-read::DEFAULT_LIMIT`, private to
/// that crate) — a known default, not a guess (§10's "adapter default" rule).
const READ_DEFAULT_LIMIT: i64 = 2_000;

pub struct HostDescriber {
    workspace: PathBuf,
    /// The shell sandbox posture (`TuiOptions::sandbox`), shown in band C when
    /// the run is sandboxed.
    sandbox: String,
}

impl HostDescriber {
    pub fn new(workspace: PathBuf, sandbox: String) -> Self {
        Self { workspace, sandbox }
    }
}

impl ToolDescriber for HostDescriber {
    fn call(&self, call: &ToolCall) -> CallFace {
        match call.name.as_str() {
            "read" => read_call(call),
            "write" | "edit" => path_call(call),
            "apply_patch" => patch_call(call),
            "grep" => grep_call(call),
            "shell" => shell_call(call),
            "finish" => finish_call(call),
            "worker_start" => worker_start_call(call),
            "worker_continue" => worker_continue_call(call),
            "worker_result" | "worker_cancel" => worker_id_call(call),
            _ => GenericDescriber.call(call),
        }
    }

    fn result(
        &self,
        call: &ToolCall,
        result: &ToolResultItem,
        elapsed_ms: Option<u64>,
    ) -> ResultFace {
        match call.name.as_str() {
            "read" => read_result(result),
            "write" => write_result(call, result),
            "edit" => edit_result(&self.workspace, call, result),
            "apply_patch" => patch_result(call, result),
            "grep" => grep_result(call, result),
            "shell" => shell_result(&self.workspace, &self.sandbox, result, elapsed_ms),
            "finish" => finish_result(call, result),
            "worker_start" => worker_start_result(call, result),
            "worker_continue" => worker_continue_result(call, result),
            "worker_result" => worker_result_result(result),
            "worker_cancel" => worker_cancel_result(result),
            _ => GenericDescriber.result(call, result, elapsed_ms),
        }
    }
}

// ---------------------------------------------------------------- shared helpers

fn parse<T: DeserializeOwned>(call: &ToolCall) -> Option<T> {
    match &call.input {
        ToolInput::Json(raw) => serde_json::from_str(raw).ok(),
        ToolInput::Text(_) => None,
    }
}

/// §7.1 Error row: "tool facts, else first output line (≤ 40 cells + …)".
fn first_line_bounded(content: &str) -> String {
    let line = content.lines().next().unwrap_or_default();
    if cell_width(line) > FACT_CELLS {
        format!("{}…", fit_cells(line, FACT_CELLS.saturating_sub(1)))
    } else {
        line.to_string()
    }
}

/// The generic Error outcome every tool below falls back to: no per-tool facts
/// apply, so the fact is the bounded first line (§7.1).
fn error_outcome(result: &ToolResultItem) -> ResultFace {
    ResultFace {
        outcome: Some(first_line_bounded(&result.content)),
        body: FaceBody::None,
        meta: None,
        target: None,
    }
}

/// Denied/Cancelled/Unknown/Unavailable already have p1-tui defaults
/// (`render/block.rs`); nothing here can add a fact those statuses lack.
fn no_face() -> ResultFace {
    ResultFace {
        outcome: None,
        body: FaceBody::None,
        meta: None,
        target: None,
    }
}

/// The number right before `" <unit>"` inside the LAST `(...)` group of a
/// one-line success message (`"Wrote x (12 bytes)."`, `"Edited x (2
/// replacements)."`) — every `p1-tool-*` success line shapes its count this
/// way. `None` when the message does not have that shape (never guessed).
fn parenthesized_count(content: &str, unit: &str) -> Option<u64> {
    let open = content.rfind('(')?;
    let rest = &content[open + 1..];
    let digits_end = rest.find(|c: char| !c.is_ascii_digit())?;
    if digits_end == 0 {
        return None;
    }
    let count: u64 = rest[..digits_end].parse().ok()?;
    rest[digits_end..]
        .trim_start()
        .starts_with(unit)
        .then_some(count)
}

fn path_call(call: &ToolCall) -> CallFace {
    #[derive(Deserialize)]
    struct Args {
        file_path: String,
    }
    CallFace {
        target: parse::<Args>(call).map(|a| a.file_path).unwrap_or_default(),
        kind: TargetKind::Path,
    }
}

// ---------------------------------------------------------------------- read

#[derive(Deserialize)]
struct ReadArgs {
    file_path: String,
    #[serde(default)]
    offset: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

fn read_call(call: &ToolCall) -> CallFace {
    let Some(args) = parse::<ReadArgs>(call) else {
        return CallFace {
            target: String::new(),
            kind: TargetKind::Path,
        };
    };
    let mut target = args.file_path;
    if args.offset.is_some() || args.limit.is_some() {
        let start = args.offset.unwrap_or(1);
        let end = start + args.limit.unwrap_or(READ_DEFAULT_LIMIT) - 1;
        target = format!("{target}:{start}-{end}");
    }
    CallFace {
        target,
        kind: TargetKind::Path,
    }
}

fn read_result(result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => ResultFace {
            outcome: Some(format!(
                "{} lines · {}",
                result.content.lines().count(),
                kb(result.content.len())
            )),
            body: FaceBody::None,
            meta: None,
            target: None,
        },
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

fn kb(bytes: usize) -> String {
    format!("{:.1} kB", bytes as f64 / 1000.0)
}

// --------------------------------------------------------------------- write

#[derive(Deserialize)]
struct WriteArgs {
    #[serde(default)]
    content: String,
}

fn write_result(call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    match result.status {
        // `new` vs `replaced` (§7.3) needs whether the file already existed
        // BEFORE this call; the tool's success text never says ("Wrote x (N
        // bytes)."), and by the time the result settles the write already
        // happened — omitted, not guessed. Known defect: needs the tool to
        // report it, or a host-side pre-write existence check threaded
        // through `ToolStarted` (out of this task's scope).
        ToolStatus::Ok => {
            let lines = parse::<WriteArgs>(call).map(|a| a.content.lines().count());
            let bytes = parenthesized_count(&result.content, "bytes");
            let mut parts = Vec::new();
            if let Some(lines) = lines {
                parts.push(format!("{lines} lines"));
            }
            if let Some(bytes) = bytes {
                parts.push(kb(bytes as usize));
            }
            ResultFace {
                outcome: (!parts.is_empty()).then(|| parts.join(" · ")),
                body: FaceBody::None,
                meta: None,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

// ---------------------------------------------------------------------- edit

#[derive(Deserialize)]
struct EditArgs {
    file_path: String,
    old_string: String,
    new_string: String,
}

fn edit_result(workspace: &Path, call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => {
            let Some(args) = parse::<EditArgs>(call) else {
                return no_face();
            };
            // `replace_all` multiplies both counts; the count is in the success
            // text ("Edited x (N replacements)."), not the call, so it is read
            // from there rather than re-deriving replace_all's own match count.
            let replacements = parenthesized_count(&result.content, "replacement").unwrap_or(1);
            let added = args.new_string.lines().count() as u64 * replacements;
            let removed = args.old_string.lines().count() as u64 * replacements;
            let rows = edit_diff_rows(
                workspace,
                &args.file_path,
                &args.old_string,
                &args.new_string,
            );
            ResultFace {
                outcome: Some(format!("+{added} −{removed}")),
                // Full, uncapped rows (§7.3's 8-row inline cap is `render/block.rs`'s
                // own job, reading `row.line_count`); `p1-tui`'s `Transcript::settle`
                // is the ONE place that registers the fold `^O` opens, from the same
                // full rows, so this describer no longer caps or names a handle.
                body: FaceBody::Diff(rows),
                meta: None,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

/// The hunk as it landed: `old`/`new` anchored on where `new`'s first line now
/// sits in the file (already edited by the time the result settles — unlike
/// the pending-approval preview, which anchors `old` in the PRE-edit file).
/// One context line before and after, when the file has them.
fn edit_diff_rows(workspace: &Path, path: &str, old: &str, new: &str) -> Vec<DiffRow> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
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

// --------------------------------------------------------------- apply_patch

/// One file `apply_patch` names, with the lines added/removed the patch text
/// itself carries. `removed` is `None` for a `Delete File` hunk: the V4A
/// grammar names the path but never lists the file's content, so the removed
/// count is genuinely unknown (omitted, never guessed as the file's length or
/// as zero).
struct PatchFile {
    path: String,
    kind: char,
    added: u64,
    removed: Option<u64>,
}

fn patch_text(call: &ToolCall) -> Option<String> {
    match &call.input {
        ToolInput::Text(text) => Some(text.clone()),
        ToolInput::Json(_) => {
            #[derive(Deserialize)]
            struct Args {
                patch: String,
            }
            parse::<Args>(call).map(|a| a.patch)
        }
    }
}

fn parse_patch_files(patch: &str) -> Vec<PatchFile> {
    let mut files: Vec<PatchFile> = Vec::new();
    for line in patch.lines() {
        if let Some(path) = line.strip_prefix("*** Add File: ") {
            files.push(PatchFile {
                path: path.to_string(),
                kind: 'A',
                added: 0,
                removed: Some(0),
            });
        } else if let Some(path) = line.strip_prefix("*** Delete File: ") {
            files.push(PatchFile {
                path: path.to_string(),
                kind: 'D',
                added: 0,
                removed: None,
            });
        } else if let Some(path) = line.strip_prefix("*** Update File: ") {
            files.push(PatchFile {
                path: path.to_string(),
                kind: 'M',
                added: 0,
                removed: Some(0),
            });
        } else if line.starts_with("*** Move to: ")
            || line.starts_with("*** End of File")
            || line.starts_with("@@")
            || line.starts_with("*** Begin Patch")
            || line.starts_with("*** End Patch")
        {
            continue;
        } else if let Some(file) = files.last_mut() {
            match (file.kind, line.chars().next()) {
                ('A', _) => file.added += 1,
                ('M', Some('+')) => file.added += 1,
                ('M', Some('-')) => *file.removed.get_or_insert(0) += 1,
                _ => {}
            }
        }
    }
    files
}

fn patch_call(call: &ToolCall) -> CallFace {
    let Some(text) = patch_text(call) else {
        return CallFace {
            target: String::new(),
            kind: TargetKind::Plain,
        };
    };
    let files = parse_patch_files(&text);
    match files.len() {
        0 => CallFace {
            target: String::new(),
            kind: TargetKind::Plain,
        },
        1 => CallFace {
            target: files[0].path.clone(),
            kind: TargetKind::Path,
        },
        n => CallFace {
            target: format!("{n} files"),
            kind: TargetKind::Plain,
        },
    }
}

fn patch_result(call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => {
            let Some(text) = patch_text(call) else {
                return no_face();
            };
            let files = parse_patch_files(&text);
            let added: u64 = files.iter().map(|f| f.added).sum();
            // A delete's removed count is unknown, so the TOTAL removed is only
            // shown when every file's own count is known (never a silent
            // undercount presented as the true total).
            let removed: Option<u64> = files
                .iter()
                .map(|f| f.removed)
                .try_fold(0u64, |acc, r| r.map(|r| acc + r));
            let outcome = match removed {
                Some(removed) => format!("+{added} −{removed} · {} files", files.len()),
                None => format!("+{added} · {} files", files.len()),
            };
            // Full, uncapped rows: `Transcript::settle` reads `files.len()` to
            // decide the §7.3 cap and registers the fold `^O` opens from these
            // same rows — a pre-capped vec would hide the true count from it.
            let rows: Vec<(String, String)> = files
                .iter()
                .map(|f| {
                    let facts = match f.removed {
                        _ if f.kind == 'D' => "D".to_string(),
                        Some(r) => format!("+{} −{r}", f.added),
                        None => format!("+{}", f.added),
                    };
                    (f.path.clone(), facts)
                })
                .collect();
            ResultFace {
                outcome: Some(outcome),
                body: FaceBody::Files(rows),
                meta: None,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

// ----------------------------------------------------------------------- grep

#[derive(Deserialize)]
struct GrepArgs {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    mode: Option<String>,
}

fn grep_call(call: &ToolCall) -> CallFace {
    let Some(args) = parse::<GrepArgs>(call) else {
        return CallFace {
            target: String::new(),
            kind: TargetKind::Plain,
        };
    };
    let scope = args.path.unwrap_or_else(|| ".".to_string());
    CallFace {
        target: format!("{} {scope}", args.pattern),
        kind: TargetKind::Plain,
    }
}

/// §7.3: "0 hits is ok"; "until #46 the host counts result lines for grep".
/// `mode: "files"` returns bare paths with no hit markers at all, so a hit
/// count would be invented there — omitted, `F files` only.
fn grep_result(call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => {
            let files_mode =
                parse::<GrepArgs>(call).and_then(|a| a.mode).as_deref() == Some("files");
            let outcome = if result.content.trim() == "No matches." {
                if files_mode {
                    "0 files".to_string()
                } else {
                    "0 hits · 0 files".to_string()
                }
            } else if files_mode {
                format!("{} files", result.content.lines().count())
            } else {
                // Content mode: blocks separated by a blank line, each headed
                // by its file path; every other line in a block is a hit.
                let blocks: Vec<&str> = result.content.split("\n\n").collect();
                let files = blocks.len();
                let hits: usize = blocks
                    .iter()
                    .map(|block| block.lines().count().saturating_sub(1))
                    .sum();
                format!("{hits} hits · {files} files")
            };
            ResultFace {
                outcome: Some(outcome),
                body: FaceBody::None,
                meta: None,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

// ---------------------------------------------------------------------- shell

#[derive(Deserialize)]
struct ShellArgs {
    command: String,
}

fn shell_call(call: &ToolCall) -> CallFace {
    CallFace {
        target: parse::<ShellArgs>(call)
            .map(|a| a.command)
            .unwrap_or_default(),
        kind: TargetKind::Command,
    }
}

/// §7.3: "`D · exit C · N lines`". `D` and `exit C` are each omitted when
/// unknown — `D` when the call carried no stamp, `exit C` when the footer
/// (read from the tool's own `[exit code: N]` line, `p1-tool-shell`'s
/// `render`) is not that shape (a timeout's content has none).
fn shell_result(
    workspace: &Path,
    sandbox: &str,
    result: &ToolResultItem,
    elapsed_ms: Option<u64>,
) -> ResultFace {
    match result.status {
        ToolStatus::Ok => {
            let lines: Vec<&str> = result.content.lines().collect();
            let footer = lines.last().copied().unwrap_or_default();
            let body_lines = lines.len().saturating_sub(1);
            let mut parts = Vec::new();
            if let Some(ms) = elapsed_ms {
                parts.push(p1_tui::render::elapsed(ms));
            }
            if let Some(code) = footer
                .strip_prefix("[exit code: ")
                .and_then(|s| s.strip_suffix(']'))
            {
                parts.push(format!("exit {code}"));
            }
            parts.push(format!("{body_lines} lines"));
            let meta =
                (sandbox != "off").then(|| format!("cwd {} · {sandbox}", workspace.display()));
            ResultFace {
                outcome: Some(parts.join(" · ")),
                body: FaceBody::None,
                meta,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

// --------------------------------------------------------------------- finish

#[derive(Deserialize)]
struct FinishArgs {
    status: String,
    #[serde(default)]
    summary: String,
    #[serde(default)]
    verification: Vec<String>,
    #[serde(default)]
    needs: Option<String>,
}

fn finish_call(call: &ToolCall) -> CallFace {
    CallFace {
        target: parse::<FinishArgs>(call)
            .map(|a| a.status)
            .unwrap_or_default(),
        kind: TargetKind::Plain,
    }
}

/// §7.3: done → `verified · <cmds>`, one `✓ cmd` row each; blocked → `blocked ·
/// needs <x>` with the summary as body. Known gap: "worker without a command
/// tool" (a different `verified`/`not verified` wording) needs to know whether
/// THIS agent assembled a command tool, which `call`/`result` alone never say
/// — always the `verified` form here.
fn finish_result(call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    let Some(args) = parse::<FinishArgs>(call) else {
        return match result.status {
            ToolStatus::Error => error_outcome(result),
            _ => no_face(),
        };
    };
    match result.status {
        ToolStatus::Ok if args.status == "done" => ResultFace {
            outcome: Some(format!("verified · {}", args.verification.join(", "))),
            body: FaceBody::Lines(args.verification.iter().map(|c| format!("✓ {c}")).collect()),
            meta: None,
            target: None,
        },
        ToolStatus::Ok => ResultFace {
            outcome: Some(format!(
                "blocked · needs {}",
                args.needs.unwrap_or_default()
            )),
            body: FaceBody::Lines(vec![args.summary]),
            meta: None,
            target: None,
        },
        ToolStatus::Error => ResultFace {
            outcome: Some(format!(
                "rejected · {}",
                first_line_bounded(&result.content)
            )),
            body: FaceBody::None,
            meta: None,
            target: None,
        },
        _ => no_face(),
    }
}

// --------------------------------------------------------------- worker_start

#[derive(Deserialize)]
struct StartArgs {
    environment: String,
    #[serde(default)]
    task: String,
    #[serde(default)]
    tools: Vec<String>,
}

/// §7.3's target is `w1 · env/profile`, but `w1` (the id) and the resolved
/// `env/profile` description only exist once the service starts the worker
/// — after this call's own `CallFace` is built and frozen. The call-time
/// target is the one fact actually known then: the requested environment;
/// `worker_start_result` below supplies the result-time override.
fn worker_start_call(call: &ToolCall) -> CallFace {
    CallFace {
        target: parse::<StartArgs>(call)
            .map(|a| a.environment)
            .unwrap_or_default(),
        kind: TargetKind::Plain,
    }
}

fn worker_start_result(call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    let Some(args) = parse::<StartArgs>(call) else {
        return match result.status {
            ToolStatus::Error => error_outcome(result),
            _ => no_face(),
        };
    };
    match result.status {
        ToolStatus::Ok => {
            let mut grants = args.tools.clone();
            grants.push("finish".to_string());
            let task_line = args.task.lines().next().unwrap_or_default().to_string();
            ResultFace {
                outcome: Some(format!("started · {}", grants.join(", "))),
                body: FaceBody::Lines(vec![task_line, format!("grants  {}", grants.join(" "))]),
                meta: None,
                target: worker_start_target(&result.content),
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

/// `w1 · env/profile`, read from the tool's own success line ("Started worker
/// w1 on claude/sonnet with tools: …") — the ONE place `id` and the resolved
/// route exist together. `None` when the text does not have that shape
/// (never guessed).
fn worker_start_target(content: &str) -> Option<String> {
    let rest = content.strip_prefix("Started worker ")?;
    let (id, rest) = rest.split_once(" on ")?;
    let (route, _) = rest.split_once(" with tools")?;
    Some(format!("{id} · {route}"))
}

// ------------------------------------------------------------ worker_continue

#[derive(Deserialize)]
struct ContinueArgs {
    id: String,
    #[serde(default)]
    add_tools: Vec<String>,
}

fn worker_continue_call(call: &ToolCall) -> CallFace {
    let Some(args) = parse::<ContinueArgs>(call) else {
        return CallFace {
            target: String::new(),
            kind: TargetKind::Plain,
        };
    };
    let target = if args.add_tools.is_empty() {
        args.id
    } else {
        format!("{} +{}", args.id, args.add_tools.join(" +"))
    };
    CallFace {
        target,
        kind: TargetKind::Plain,
    }
}

fn worker_continue_result(call: &ToolCall, result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => {
            let add_tools = parse::<ContinueArgs>(call)
                .map(|a| a.add_tools)
                .unwrap_or_default();
            let outcome = if add_tools.is_empty() {
                "resumed".to_string()
            } else {
                format!("resumed · +{}", add_tools.join(" +"))
            };
            ResultFace {
                outcome: Some(outcome),
                body: FaceBody::None,
                meta: None,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

// -------------------------------------------------------- worker_result/cancel

#[derive(Deserialize)]
struct IdArgs {
    id: String,
}

fn worker_id_call(call: &ToolCall) -> CallFace {
    CallFace {
        target: parse::<IdArgs>(call).map(|a| a.id).unwrap_or_default(),
        kind: TargetKind::Plain,
    }
}

/// §7.3: `<finish status> · N lines`. The status word is read out of
/// `render_status`'s own text (`Worker id: running`/`: cancelled`, or a
/// `finish: <word>` line inside a finished worker's report) — never guessed
/// when the content does not have one of those shapes.
fn worker_result_result(result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => {
            let n_lines = result.content.lines().count();
            let status_word = if result.content.contains(": running") {
                Some("running".to_string())
            } else if result.content.contains(": cancelled") {
                Some("cancelled".to_string())
            } else if result.content.contains(": failed") {
                Some("failed".to_string())
            } else {
                result.content.lines().find_map(|line| {
                    line.strip_prefix("finish: ")
                        .map(|rest| rest.split([' ', '—']).next().unwrap_or(rest).to_string())
                })
            };
            let outcome = match status_word {
                Some(word) => format!("{word} · {n_lines} lines"),
                None => format!("{n_lines} lines"),
            };
            // The structured report (`tools:`/`finish:`/missing calls) precedes
            // the `---` separator only for a FINISHED worker's result.
            let body = match result.content.split_once("\n---\n") {
                Some((report, _)) => {
                    FaceBody::Lines(report.lines().take(8).map(str::to_string).collect())
                }
                None => FaceBody::None,
            };
            ResultFace {
                outcome: Some(outcome),
                body,
                meta: None,
                target: None,
            }
        }
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

fn worker_cancel_result(result: &ToolResultItem) -> ResultFace {
    match result.status {
        ToolStatus::Ok => ResultFace {
            outcome: Some("cancelled".to_string()),
            body: FaceBody::None,
            meta: None,
            target: None,
        },
        ToolStatus::Error => error_outcome(result),
        _ => no_face(),
    }
}

#[cfg(test)]
mod tests;
