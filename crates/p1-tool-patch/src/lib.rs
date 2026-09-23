//! The `apply_patch` tool: the GPT-family V4A patch format.
//!
//! A patch is parsed by hand into hunks, then *planned* completely in memory:
//! every path is confined with [`Workspace::resolve`], every hunk must locate
//! (exact, then trailing-whitespace-insensitive, then whitespace-insensitive),
//! and every resulting file content is computed before a single byte is
//! written. Only then are the planned writes applied with
//! [`p1_workspace::write_atomic`], so a patch that fails anywhere changes
//! nothing.
//!
//! `apply_patch` is exempt from read-before-mutate: the hunks must match the
//! file's CURRENT contents, which is its own staleness check. It still records
//! every file it writes in the shared [`ObservedFiles`].

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use p1_contracts::{
    BoxFuture, CallDescription, CancellationToken, DeclarationKind, Effect, Grammar, Tool,
    ToolCall, ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolStatus,
};
use p1_workspace::{ObservedFiles, ToolFace, Workspace, bound_output, write_atomic};
use serde::Deserialize;

const NAME: &str = "apply_patch";
const DESCRIPTION: &str = "Apply a V4A patch to files in the workspace.\nThe patch is validated completely before anything is written; if any hunk fails to match, nothing changes and the error names the file and hunk.\nUse `*** Add File:`, `*** Delete File:` and `*** Update File:` hunks inside `*** Begin Patch` / `*** End Patch`.";
const MAX_OUTPUT_BYTES: usize = 50_000;
const MAX_OUTPUT_LINES: usize = 2_000;

/// The published Codex V4A grammar, advertised with the freeform declaration.
const PATCH_GRAMMAR: &str = r#"start: begin_patch hunk+ end_patch
begin_patch: "*** Begin Patch" LF
end_patch: "*** End Patch" LF?
hunk: add_hunk | delete_hunk | update_hunk
add_hunk: "*** Add File: " filename LF add_line+
delete_hunk: "*** Delete File: " filename LF
update_hunk: "*** Update File: " filename LF change_move? change?
filename: /(.+)/
add_line: "+" /(.*)/ LF -> line
change_move: "*** Move to: " filename LF
change: (change_context | change_line)+ eof_line?
change_context: ("@@" | "@@ " /(.+)/) LF
change_line: ("+" | "-" | " ") /(.*)/ LF
eof_line: "*** End of File" LF
%import common.LF
"#;

/// The `apply_patch` tool. Holds one agent's workspace and observation store.
pub struct PatchTool {
    workspace: Workspace,
    observed: ObservedFiles,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
    /// Whether this instance is presented as a freeform or a function tool.
    freeform: bool,
}

impl PatchTool {
    /// Build the tool with the default (`apply_patch`, GPT-family) freeform face.
    pub fn new(workspace: Workspace, observed: ObservedFiles) -> Self {
        Self {
            workspace,
            observed,
            declaration: declaration(default_face(), true),
            identity: identity("gpt"),
            freeform: true,
        }
    }

    /// Present the same implementation under another name/description and
    /// variant, keeping the current freeform/function shape.
    pub fn with_face(self, face: ToolFace, variant: &str) -> Self {
        let freeform = self.freeform;
        Self {
            workspace: self.workspace,
            observed: self.observed,
            declaration: declaration(face, freeform),
            identity: identity(variant),
            freeform,
        }
    }

    /// Present the same implementation as a function tool taking
    /// `{"patch": string}`, for routes without freeform tools.
    pub fn function_face(self) -> Self {
        let face = ToolFace::new(
            self.declaration.name.clone(),
            self.declaration.description.clone(),
        );
        Self {
            workspace: self.workspace,
            observed: self.observed,
            declaration: declaration(face, false),
            identity: identity("function"),
            freeform: false,
        }
    }
}

fn default_face() -> ToolFace {
    ToolFace::new(NAME, DESCRIPTION)
}

fn declaration(face: ToolFace, freeform: bool) -> ToolDeclaration {
    let kind = if freeform {
        DeclarationKind::Freeform {
            grammar: Some(Grammar {
                syntax: "lark".to_string(),
                definition: PATCH_GRAMMAR.to_string(),
            }),
        }
    } else {
        DeclarationKind::Function {
            input_schema: function_schema(),
        }
    };
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind,
    }
}

fn function_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "patch": { "type": "string" }
        },
        "required": ["patch"],
        "additionalProperties": false
    })
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

/// One parsed patch hunk.
enum Hunk {
    Add {
        path: String,
        lines: Vec<String>,
    },
    Delete {
        path: String,
    },
    Update {
        path: String,
        moveto: Option<String>,
        groups: Vec<UpdateGroup>,
    },
}

/// The paths a parsed patch touches, in patch order (the source path of an
/// `Update File` hunk, even when it also moves).
fn hunk_paths(hunks: &[Hunk]) -> Vec<String> {
    hunks
        .iter()
        .map(|hunk| match hunk {
            Hunk::Add { path, .. } | Hunk::Delete { path } | Hunk::Update { path, .. } => {
                path.clone()
            }
        })
        .collect()
}

/// One `@@`-delimited search group inside an Update File hunk.
struct UpdateGroup {
    /// The `@@ <header>` seek line, if one was given.
    header: Option<String>,
    /// Whether `*** End of File` anchored this group at the end of the file.
    eof: bool,
    /// The source line the group starts on, for error messages.
    line: usize,
    /// Lines the group expects to find (context and `-` lines).
    old: Vec<String>,
    /// Lines the group leaves behind (context and `+` lines).
    new: Vec<String>,
}

/// A validated mutation, applied in patch order once planning has succeeded.
enum Op {
    Add {
        path: PathBuf,
        display: String,
        contents: Vec<u8>,
    },
    Modify {
        path: PathBuf,
        display: String,
        contents: Vec<u8>,
    },
    Delete {
        path: PathBuf,
        display: String,
    },
    Move {
        from: PathBuf,
        from_display: String,
        to: PathBuf,
        to_display: String,
        contents: Vec<u8>,
    },
}

impl Op {
    fn success_line(&self) -> String {
        match self {
            Op::Add { display, .. } => format!("A {display}"),
            Op::Modify { display, .. } => format!("M {display}"),
            Op::Delete { display, .. } => format!("D {display}"),
            Op::Move {
                from_display,
                to_display,
                ..
            } => format!("M {from_display} -> {to_display}"),
        }
    }
}

/// Why applying a patch failed.
enum PatchFailure {
    /// A message the model can act on.
    Message(String),
    /// The cancellation token was set; nothing was written.
    Cancelled,
}

impl Tool for PatchTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::WritesFiles
    }

    /// ADR-0057: parse the patch's own freeform (or function) input the same way
    /// `execute` does, and name the first file it touches — or the count, for a
    /// multi-file patch.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        let target = patch_text(&self.declaration.name, self.freeform, call)
            .ok()
            .and_then(|text| parse_patch(&text).ok())
            .map(|hunks| hunk_paths(&hunks));
        let target = match target.as_deref() {
            None | Some([]) => None,
            Some([only]) => Some(only.clone()),
            Some(paths) => Some(format!("{} files", paths.len())),
        };
        CallDescription {
            verb: "edit",
            target,
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            // Cancellation before any work: touch nothing.
            if context.cancel.is_cancelled() {
                return ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                };
            }
            let patch = match patch_text(&self.declaration.name, self.freeform, call) {
                Ok(patch) => patch,
                Err(message) => return ToolOutcome::error(message),
            };
            let workspace = self.workspace.clone();
            let observed = self.observed.clone();
            let cancel = context.cancel.clone();
            let tool = self.declaration.name.clone();
            match tokio::task::spawn_blocking(move || run(&workspace, &observed, &patch, &cancel))
                .await
            {
                Ok(Ok(content)) => {
                    ToolOutcome::ok(bound_output(&content, MAX_OUTPUT_BYTES, MAX_OUTPUT_LINES))
                }
                Ok(Err(PatchFailure::Message(message))) => ToolOutcome::error(message),
                Ok(Err(PatchFailure::Cancelled)) => ToolOutcome {
                    status: ToolStatus::Cancelled,
                    content: String::new(),
                },
                Err(error) => ToolOutcome::error(format!("{tool} failed: {error}")),
            }
        })
    }
}

/// Extract the patch text from the raw call, according to the current shape.
fn patch_text(tool: &str, freeform: bool, call: &ToolCall) -> Result<String, String> {
    if freeform {
        match &call.input {
            ToolInput::Text(raw) => Ok(raw.clone()),
            ToolInput::Json(_) => Err(invalid(
                tool,
                "expected freeform text input, got a JSON object",
            )),
        }
    } else {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct FunctionInput {
            patch: String,
        }
        match &call.input {
            ToolInput::Json(raw) => {
                let input: FunctionInput =
                    serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))?;
                Ok(input.patch)
            }
            ToolInput::Text(_) => Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            )),
        }
    }
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

fn invalid_patch(reason: impl std::fmt::Display, line: usize) -> PatchFailure {
    PatchFailure::Message(format!("Invalid patch: {reason} (line {line})."))
}

fn run(
    workspace: &Workspace,
    observed: &ObservedFiles,
    text: &str,
    cancel: &CancellationToken,
) -> Result<String, PatchFailure> {
    let hunks = parse_patch(text)?;
    // Planning reads the files the hunks are located in; applying writes them.
    // Both under the gate: another agent's write cannot land in between and be
    // overwritten by contents planned from the older state.
    let _mutation = workspace.begin_mutation();
    let ops = plan(workspace, &hunks, cancel)?;
    apply(&ops, observed, cancel)?;
    Ok(ops
        .iter()
        .map(Op::success_line)
        .collect::<Vec<_>>()
        .join("\n"))
}

/// Strip an optional heredoc or fenced-code wrapper, normalize CRLF line
/// endings, and drop surrounding blank lines.
fn strip_wrapper(text: &str) -> String {
    let normalized = text.replace("\r\n", "\n");
    let mut lines: Vec<&str> = normalized.lines().collect();
    while lines.first().is_some_and(|line| line.trim().is_empty()) {
        lines.remove(0);
    }
    if let Some(first) = lines.first().map(|line| line.trim().to_string()) {
        if first.starts_with("```") {
            lines.remove(0);
            while lines.last().is_some_and(|line| line.trim().is_empty()) {
                lines.pop();
            }
            if lines
                .last()
                .is_some_and(|line| line.trim_start().starts_with("```"))
            {
                lines.pop();
            }
        } else if let Some(delimiter) = heredoc_delimiter(&first) {
            lines.remove(0);
            if let Some(position) = lines.iter().position(|line| line.trim() == delimiter) {
                lines.truncate(position);
            }
        }
    }
    while lines.last().is_some_and(|line| line.trim().is_empty()) {
        lines.pop();
    }
    lines.join("\n")
}

/// The terminator word of a `<<'EOF'`-style first line, if any.
fn heredoc_delimiter(line: &str) -> Option<String> {
    let rest = line.strip_prefix("<<")?;
    let rest = rest.strip_prefix('-').unwrap_or(rest);
    let word = rest.trim_matches(|character| character == '\'' || character == '"');
    if word.is_empty() {
        None
    } else {
        Some(word.to_string())
    }
}

fn parse_patch(text: &str) -> Result<Vec<Hunk>, PatchFailure> {
    let stripped = strip_wrapper(text);
    let lines: Vec<&str> = stripped.lines().collect();
    if lines.first() != Some(&"*** Begin Patch") {
        return Err(invalid_patch(
            "the first line must be \"*** Begin Patch\"",
            1,
        ));
    }

    let mut hunks = Vec::new();
    let mut index = 1usize;
    let mut ended = false;
    while index < lines.len() {
        let line = lines[index];
        let line_number = index + 1;
        if line == "*** End Patch" {
            ended = true;
            index += 1;
            break;
        }
        if let Some(rest) = line.strip_prefix("*** Add File: ") {
            let path = rest.to_string();
            if path.is_empty() {
                return Err(invalid_patch("Add File has an empty path", line_number));
            }
            index += 1;
            let mut added = Vec::new();
            while index < lines.len() {
                if let Some(content) = lines[index].strip_prefix('+') {
                    added.push(content.to_string());
                    index += 1;
                } else {
                    break;
                }
            }
            if added.is_empty() {
                return Err(invalid_patch(
                    format!("Add File hunk for {path} has no lines"),
                    line_number,
                ));
            }
            hunks.push(Hunk::Add { path, lines: added });
        } else if let Some(rest) = line.strip_prefix("*** Delete File: ") {
            let path = rest.to_string();
            if path.is_empty() {
                return Err(invalid_patch("Delete File has an empty path", line_number));
            }
            hunks.push(Hunk::Delete { path });
            index += 1;
        } else if let Some(rest) = line.strip_prefix("*** Update File: ") {
            let path = rest.to_string();
            if path.is_empty() {
                return Err(invalid_patch("Update File has an empty path", line_number));
            }
            let hunk_line = line_number;
            index += 1;
            let mut moveto = None;
            if index < lines.len()
                && let Some(target) = lines[index].strip_prefix("*** Move to: ")
            {
                if target.is_empty() {
                    return Err(invalid_patch("Move to has an empty path", index + 1));
                }
                moveto = Some(target.to_string());
                index += 1;
            }
            let groups = parse_update_groups(&lines, &mut index)?;
            if groups.is_empty() && moveto.is_none() {
                return Err(invalid_patch(
                    format!("Update File hunk for {path} has no changes"),
                    hunk_line,
                ));
            }
            for group in &groups {
                if group.old.is_empty() && group.new.is_empty() {
                    return Err(invalid_patch(
                        format!("Update hunk for {path} contains no lines"),
                        group.line,
                    ));
                }
            }
            hunks.push(Hunk::Update {
                path,
                moveto,
                groups,
            });
        } else {
            return Err(invalid_patch(
                format!("expected a hunk header, found {line:?}"),
                line_number,
            ));
        }
    }

    if !ended {
        return Err(invalid_patch(
            "the patch must end with \"*** End Patch\"",
            lines.len().max(1),
        ));
    }
    while index < lines.len() {
        if !lines[index].trim().is_empty() {
            return Err(invalid_patch(
                "unexpected content after \"*** End Patch\"",
                index + 1,
            ));
        }
        index += 1;
    }
    if hunks.is_empty() {
        return Err(invalid_patch("the patch contains no hunks", 1));
    }
    Ok(hunks)
}

/// Parse the change groups of one Update File hunk, advancing `index` past them.
fn parse_update_groups(
    lines: &[&str],
    index: &mut usize,
) -> Result<Vec<UpdateGroup>, PatchFailure> {
    let mut groups: Vec<UpdateGroup> = Vec::new();
    let mut current: Option<UpdateGroup> = None;
    while *index < lines.len() {
        let line = lines[*index];
        let line_number = *index + 1;
        if line == "*** End of File" {
            let Some(group) = current.as_mut() else {
                return Err(invalid_patch(
                    "\"*** End of File\" without a preceding change",
                    line_number,
                ));
            };
            group.eof = true;
            *index += 1;
            if *index < lines.len() && !lines[*index].starts_with("*** ") {
                return Err(invalid_patch(
                    format!(
                        "unexpected line after \"*** End of File\": {:?}",
                        lines[*index]
                    ),
                    *index + 1,
                ));
            }
            continue;
        }
        // Any other `***` line starts the next hunk or ends the patch.
        if line.starts_with("*** ") {
            break;
        }
        if line == "@@" || line.starts_with("@@ ") {
            if let Some(group) = current.take() {
                groups.push(group);
            }
            let header = if line == "@@" {
                None
            } else {
                Some(line[3..].to_string())
            };
            current = Some(UpdateGroup {
                header,
                eof: false,
                line: line_number,
                old: Vec::new(),
                new: Vec::new(),
            });
            *index += 1;
            continue;
        }
        let Some((prefix, content)) = split_change_line(line) else {
            return Err(invalid_patch(
                format!("unexpected line in Update File hunk: {line:?}"),
                line_number,
            ));
        };
        let group = current.get_or_insert_with(|| UpdateGroup {
            header: None,
            eof: false,
            line: line_number,
            old: Vec::new(),
            new: Vec::new(),
        });
        if prefix != '+' {
            group.old.push(content.to_string());
        }
        if prefix != '-' {
            group.new.push(content.to_string());
        }
        *index += 1;
    }
    if let Some(group) = current.take() {
        groups.push(group);
    }
    Ok(groups)
}

/// Split a change line. An empty line is a context line with empty content.
fn split_change_line(line: &str) -> Option<(char, &str)> {
    match line.chars().next() {
        None => Some((' ', "")),
        Some(prefix @ (' ' | '+' | '-')) => Some((prefix, &line[1..])),
        Some(_) => None,
    }
}

/// Validate every hunk and compute every resulting file in memory.
fn plan(
    workspace: &Workspace,
    hunks: &[Hunk],
    cancel: &CancellationToken,
) -> Result<Vec<Op>, PatchFailure> {
    // Staged contents for the paths already touched by this patch, so a later
    // hunk sees the result of an earlier one without anything being written.
    let mut staged: HashMap<PathBuf, Option<Vec<u8>>> = HashMap::new();
    let mut ops = Vec::new();

    for hunk in hunks {
        if cancel.is_cancelled() {
            return Err(PatchFailure::Cancelled);
        }
        match hunk {
            Hunk::Add { path, lines } => {
                let (resolved, display) = resolve(workspace, path)?;
                if is_present(&resolved, &staged) {
                    return Err(PatchFailure::Message(format!("{display} already exists.")));
                }
                let mut contents = lines.join("\n");
                if !lines.is_empty() {
                    contents.push('\n');
                }
                let contents = contents.into_bytes();
                staged.insert(resolved.clone(), Some(contents.clone()));
                ops.push(Op::Add {
                    path: resolved,
                    display,
                    contents,
                });
            }
            Hunk::Delete { path } => {
                let (resolved, display) = resolve(workspace, path)?;
                if current_contents(&resolved, &display, &staged)?.is_none() {
                    return Err(PatchFailure::Message(format!("{display} does not exist.")));
                }
                staged.insert(resolved.clone(), None);
                ops.push(Op::Delete {
                    path: resolved,
                    display,
                });
            }
            Hunk::Update {
                path,
                moveto,
                groups,
            } => {
                let (resolved, display) = resolve(workspace, path)?;
                let bytes = current_contents(&resolved, &display, &staged)?
                    .ok_or_else(|| PatchFailure::Message(format!("{display} does not exist.")))?;
                let contents = apply_groups(&bytes, groups, &display)?;
                match moveto {
                    Some(target) => {
                        let (to, to_display) = resolve(workspace, target)?;
                        if is_present(&to, &staged) {
                            return Err(PatchFailure::Message(format!(
                                "{to_display} already exists."
                            )));
                        }
                        staged.insert(resolved.clone(), None);
                        staged.insert(to.clone(), Some(contents.clone()));
                        ops.push(Op::Move {
                            from: resolved,
                            from_display: display,
                            to,
                            to_display,
                            contents,
                        });
                    }
                    None => {
                        staged.insert(resolved.clone(), Some(contents.clone()));
                        ops.push(Op::Modify {
                            path: resolved,
                            display,
                            contents,
                        });
                    }
                }
            }
        }
    }
    Ok(ops)
}

fn resolve(workspace: &Workspace, path: &str) -> Result<(PathBuf, String), PatchFailure> {
    let resolved = workspace
        .resolve(path)
        .map_err(|error| PatchFailure::Message(error.to_string()))?;
    let display = workspace.display(&resolved);
    Ok((resolved, display))
}

/// Whether `path` exists, counting a path staged earlier in this patch.
fn is_present(path: &Path, staged: &HashMap<PathBuf, Option<Vec<u8>>>) -> bool {
    match staged.get(path) {
        Some(entry) => entry.is_some(),
        None => path.exists(),
    }
}

/// The current bytes of `path`: staged content when this patch touched it
/// already, otherwise the file on disk.
fn current_contents(
    path: &Path,
    display: &str,
    staged: &HashMap<PathBuf, Option<Vec<u8>>>,
) -> Result<Option<Vec<u8>>, PatchFailure> {
    if let Some(entry) = staged.get(path) {
        return Ok(entry.clone());
    }
    match std::fs::metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(PatchFailure::Message(format!(
            "{display} could not be read: {error}"
        ))),
        Ok(metadata) => {
            if !metadata.is_file() {
                return Err(PatchFailure::Message(format!(
                    "{display} is not a regular file."
                )));
            }
            let bytes = std::fs::read(path).map_err(|error| {
                PatchFailure::Message(format!("{display} could not be read: {error}"))
            })?;
            if std::str::from_utf8(&bytes).is_err() {
                return Err(PatchFailure::Message(format!(
                    "{display} is not valid UTF-8."
                )));
            }
            Ok(Some(bytes))
        }
    }
}

/// Apply the parsed groups to a file's bytes, preserving its CRLF style and
/// trailing-newline state.
fn apply_groups(
    bytes: &[u8],
    groups: &[UpdateGroup],
    display: &str,
) -> Result<Vec<u8>, PatchFailure> {
    let text = std::str::from_utf8(bytes)
        .map_err(|_| PatchFailure::Message(format!("{display} is not valid UTF-8.")))?;
    let crlf = detect_crlf(text);
    let normalized = text.replace("\r\n", "\n");
    let trailing_newline = normalized.ends_with('\n');
    let mut lines = split_lines(&normalized);

    let mut running = 0usize;
    for (index, group) in groups.iter().enumerate() {
        let position = locate(&lines, group, running).ok_or_else(|| {
            PatchFailure::Message(format!(
                "{display}: hunk {} did not match the file.",
                index + 1
            ))
        })?;
        let end = position + group.old.len();
        lines.splice(position..end, group.new.iter().cloned());
        running = position + group.new.len();
    }

    let mut joined = lines.join("\n");
    if trailing_newline && !lines.is_empty() {
        joined.push('\n');
    }
    if crlf {
        joined = joined.replace('\n', "\r\n");
    }
    Ok(joined.into_bytes())
}

fn split_lines(normalized: &str) -> Vec<String> {
    if normalized.is_empty() {
        return Vec::new();
    }
    let mut lines: Vec<String> = normalized.split('\n').map(String::from).collect();
    if normalized.ends_with('\n') {
        lines.pop();
    }
    lines
}

fn detect_crlf(text: &str) -> bool {
    match text.find('\n') {
        Some(index) => index > 0 && text.as_bytes()[index - 1] == b'\r',
        None => false,
    }
}

/// The whitespace-insensitivity ladder, tried in order.
#[derive(Clone, Copy, PartialEq, Eq)]
enum MatchTier {
    Exact,
    TrimEnd,
    TrimBoth,
}

/// Find where a group's expected lines match, at or after `running`.
fn locate(lines: &[String], group: &UpdateGroup, running: usize) -> Option<usize> {
    let start = match &group.header {
        Some(header) => find_header(lines, header, running)? + 1,
        None => running,
    };
    let pattern = &group.old;
    if group.eof {
        if start > lines.len() {
            return None;
        }
        let position = lines.len().checked_sub(pattern.len())?;
        if position < start {
            return None;
        }
        return [MatchTier::Exact, MatchTier::TrimEnd, MatchTier::TrimBoth]
            .into_iter()
            .find(|&tier| matches_at(lines, position, pattern, tier))
            .map(|_| position);
    }
    if pattern.is_empty() {
        // Codex semantics, which GPT models are trained on: a hunk with nothing to
        // match (additions only) is appended at the END of the file, with or without
        // a header. (Lead ruling; the first implementation inserted at the search
        // position.)
        return Some(lines.len());
    }
    if start + pattern.len() > lines.len() {
        return None;
    }
    for tier in [MatchTier::Exact, MatchTier::TrimEnd, MatchTier::TrimBoth] {
        for position in start..=(lines.len() - pattern.len()) {
            if matches_at(lines, position, pattern, tier) {
                return Some(position);
            }
        }
    }
    None
}

/// The `@@ <header>` seek: exact first, then trimmed.
fn find_header(lines: &[String], header: &str, from: usize) -> Option<usize> {
    (from..lines.len())
        .find(|&index| lines[index] == header)
        .or_else(|| (from..lines.len()).find(|&index| lines[index].trim() == header.trim()))
}

fn matches_at(lines: &[String], position: usize, pattern: &[String], tier: MatchTier) -> bool {
    pattern.iter().enumerate().all(|(offset, expected)| {
        let Some(actual) = lines.get(position + offset) else {
            return false;
        };
        match tier {
            MatchTier::Exact => actual == expected,
            MatchTier::TrimEnd => actual.trim_end() == expected.trim_end(),
            MatchTier::TrimBoth => actual.trim() == expected.trim(),
        }
    })
}

fn apply(
    ops: &[Op],
    observed: &ObservedFiles,
    cancel: &CancellationToken,
) -> Result<(), PatchFailure> {
    for op in ops {
        if cancel.is_cancelled() {
            return Err(PatchFailure::Cancelled);
        }
        match op {
            Op::Add {
                path,
                display,
                contents,
            }
            | Op::Modify {
                path,
                display,
                contents,
            } => {
                write_atomic(path, contents).map_err(|error| {
                    PatchFailure::Message(format!("failed to write {display}: {error}"))
                })?;
                observed.record(path, contents);
            }
            Op::Delete { path, display } => {
                std::fs::remove_file(path).map_err(|error| {
                    PatchFailure::Message(format!("failed to delete {display}: {error}"))
                })?;
            }
            Op::Move {
                from,
                from_display,
                to,
                contents,
                ..
            } => {
                write_atomic(to, contents).map_err(|error| {
                    PatchFailure::Message(format!("failed to write {}: {error}", to.display()))
                })?;
                observed.record(to, contents);
                std::fs::remove_file(from).map_err(|error| {
                    PatchFailure::Message(format!("failed to delete {from_display}: {error}"))
                })?;
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{PATCH_GRAMMAR, PatchTool};
    use p1_contracts::{
        CancellationToken, DeclarationKind, Effect, Grammar, Tool, ToolCall, ToolContext,
        ToolInput, ToolOutcome, ToolStatus,
    };
    use p1_workspace::{Observation, ObservedFiles, ToolFace, Workspace};
    use std::path::Path;

    fn tool(root: &Path) -> (PatchTool, ObservedFiles) {
        let observed = ObservedFiles::new();
        (
            PatchTool::new(Workspace::new(root).unwrap(), observed.clone()),
            observed,
        )
    }

    fn text_call(patch: &str) -> ToolCall {
        ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Text(patch.to_string()),
        }
    }

    async fn execute(tool: &PatchTool, patch: &str) -> ToolOutcome {
        let call = text_call(patch);
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        tool.execute(&call, context).await
    }

    fn read(root: &Path, path: &str) -> String {
        std::fs::read_to_string(root.join(path)).unwrap()
    }

    fn write(root: &Path, path: &str, contents: &str) {
        let target = root.join(path);
        if let Some(parent) = target.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(target, contents).unwrap();
    }

    /// (a) The three-file example from `environments/gpt/prompt.md`.
    #[tokio::test]
    async fn prompt_md_three_file_example_applies() {
        let dir = tempfile::tempdir().unwrap();
        write(
            dir.path(),
            "path/to/file.rs",
            "fn existing_function\nunchanged context line\nremoved line\n",
        );
        write(dir.path(), "path/to/old_file.rs", "old\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: path/to/file.rs\n@@ fn existing_function\n unchanged context line\n-removed line\n+added line\n*** Add File: path/to/new_file.rs\n+first line\n*** Delete File: path/to/old_file.rs\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(
            outcome.content,
            "M path/to/file.rs\nA path/to/new_file.rs\nD path/to/old_file.rs"
        );
        assert_eq!(
            read(dir.path(), "path/to/file.rs"),
            "fn existing_function\nunchanged context line\nadded line\n"
        );
        assert_eq!(read(dir.path(), "path/to/new_file.rs"), "first line\n");
        assert!(!dir.path().join("path/to/old_file.rs").exists());
    }

    /// (b) The second hunk's context also occurs earlier; it must apply later.
    #[tokio::test]
    async fn second_hunk_applies_at_the_later_position() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "ctx\nA\nctx\nA\nB\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@ ctx\n-A\n+first\n@@ ctx\n-A\n+second\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "ctx\nfirst\nctx\nsecond\nB\n");
    }

    /// (c) A later failure must leave the first file byte-identical.
    #[tokio::test]
    async fn a_failed_second_file_leaves_the_first_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let original = "one\ntwo\nthree\n";
        write(dir.path(), "first.txt", original);
        write(dir.path(), "second.txt", "alpha\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: first.txt\n-one\n+ONE\n*** Update File: second.txt\n-missing\n+other\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(
            outcome.content,
            "second.txt: hunk 1 did not match the file."
        );
        assert_eq!(read(dir.path(), "first.txt"), original);
        assert_eq!(read(dir.path(), "second.txt"), "alpha\n");
    }

    /// (d) The whitespace ladder matches a tab-indented file whose patch
    /// context line carries trailing spaces.
    #[tokio::test]
    async fn whitespace_ladder_applies_a_tab_indented_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "ctx\n\talpha\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n ctx\n-\talpha   \n+\tbeta\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "ctx\n\tbeta\n");
    }

    /// (e) An escaping path or a symlink out of the workspace is rejected and
    /// nothing is written.
    #[cfg(unix)]
    #[tokio::test]
    async fn escaping_paths_and_symlinks_are_rejected() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "secret\n").unwrap();
        symlink(outside.path(), dir.path().join("link")).unwrap();
        let (tool, _) = tool(dir.path());

        let add = execute(
            &tool,
            "*** Begin Patch\n*** Add File: ../x\n+hello\n*** End Patch\n",
        )
        .await;
        assert_eq!(add.status, ToolStatus::Error, "{add:?}");
        assert!(add.content.contains("escapes workspace"), "{add:?}");

        let update = execute(
            &tool,
            "*** Begin Patch\n*** Update File: link/secret\n-old\n+new\n*** End Patch\n",
        )
        .await;
        assert_eq!(update.status, ToolStatus::Error, "{update:?}");
        assert!(update.content.contains("escapes workspace"), "{update:?}");
        assert_eq!(
            std::fs::read_to_string(outside.path().join("secret")).unwrap(),
            "secret\n"
        );
    }

    /// (f) Adding a file that already exists is an error.
    #[tokio::test]
    async fn add_of_an_existing_file_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "exists.txt", "original\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Add File: exists.txt\n+replacement\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(outcome.content, "exists.txt already exists.");
        assert_eq!(read(dir.path(), "exists.txt"), "original\n");
    }

    /// (g) Garbage is always `Invalid patch: …`, never a panic.
    #[tokio::test]
    async fn garbage_patches_are_invalid_patches() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let garbage = [
            "",
            "*** Update File: f.txt\n@@\n-a\n+b\n*** End Patch\n",
            "*** Begin Patch\n*** Add File: z.txt\n+x\n",
            "*** Begin Patch\n@@\n-a\n+b\n*** End Patch\n",
            "*** Begin Patch\n*** End Patch\n",
            "*** Begin Patch\n*** Frobnicate: x\n*** End Patch\n",
            "\u{0}\u{1}not a patch at all",
        ];
        for patch in garbage {
            let outcome = execute(&tool, patch).await;
            assert_eq!(outcome.status, ToolStatus::Error, "patch: {patch:?}");
            assert!(
                outcome.content.starts_with("Invalid patch: "),
                "patch: {patch:?} -> {outcome:?}"
            );
        }
        assert!(!dir.path().join("z.txt").exists());
    }

    /// (h) A CRLF file stays CRLF.
    #[tokio::test]
    async fn a_crlf_file_stays_crlf() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\r\nb\r\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n@@\n-b\n+c\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\r\nc\r\n");
    }

    /// (i) A move and an edit in one hunk.
    #[tokio::test]
    async fn move_and_edit_in_one_hunk() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old.txt", "x\ny\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: sub/new.txt\n@@\n-y\n+z\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "M old.txt -> sub/new.txt");
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(read(dir.path(), "sub/new.txt"), "x\nz\n");
    }

    #[tokio::test]
    async fn a_pure_move_needs_no_change_lines() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old.txt", "x\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: new.txt\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "M old.txt -> new.txt");
        assert!(!dir.path().join("old.txt").exists());
        assert_eq!(read(dir.path(), "new.txt"), "x\n");
    }

    #[tokio::test]
    async fn a_move_onto_an_existing_path_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "old.txt", "x\n");
        write(dir.path(), "taken.txt", "y\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: taken.txt\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(outcome.content, "taken.txt already exists.");
        assert_eq!(read(dir.path(), "old.txt"), "x\n");
        assert_eq!(read(dir.path(), "taken.txt"), "y\n");
    }

    /// (j) `*** End of File` appends at the end of the file.
    #[tokio::test]
    async fn end_of_file_appends_at_the_end() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n@@\n+c\n*** End of File\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nb\nc\n");
    }

    #[tokio::test]
    async fn a_plain_addition_without_context_appends_like_codex() {
        // A hunk with nothing to match has no anchor: Codex appends it at the end
        // of the file, and so does p1.
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n+x\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nb\nx\n");
    }

    #[tokio::test]
    async fn a_change_without_lines_is_invalid() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: f.txt\n@@ a\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert!(
            outcome.content.starts_with("Invalid patch: "),
            "{outcome:?}"
        );
        assert_eq!(read(dir.path(), "f.txt"), "a\n");
    }

    /// (k) The function face does the same as the freeform face.
    #[tokio::test]
    async fn function_face_behaves_like_the_freeform_face() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let observed = ObservedFiles::new();
        let tool = PatchTool::new(Workspace::new(dir.path()).unwrap(), observed).function_face();
        let patch = "*** Begin Patch\n*** Update File: f.txt\n-b\n+B\n*** End Patch\n";
        let call = ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Json(serde_json::json!({ "patch": patch }).to_string()),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };

        let outcome = tool.execute(&call, context).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "M f.txt");
        assert_eq!(read(dir.path(), "f.txt"), "a\nB\n");
    }

    #[tokio::test]
    async fn a_heredoc_wrapper_and_crlf_patch_are_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());
        let wrapped = "<<'EOF'\r\n*** Begin Patch\r\n*** Update File: f.txt\r\n@@\r\n-b\r\n+c\r\n*** End Patch\r\nEOF\r\n";

        let outcome = execute(&tool, wrapped).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nc\n");
    }

    #[tokio::test]
    async fn a_fenced_code_block_wrapper_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());
        let fenced =
            "```diff\n*** Begin Patch\n*** Update File: f.txt\n-b\n+c\n*** End Patch\n```\n";

        let outcome = execute(&tool, fenced).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nc\n");
    }

    #[tokio::test]
    async fn a_missing_final_newline_in_the_patch_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\nb\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n-b\n+c\n*** End Patch";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\nc\n");
    }

    #[tokio::test]
    async fn an_empty_context_line_is_a_context_line() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "f.txt", "a\n\nb\n");
        let (tool, _) = tool(dir.path());
        let patch = "*** Begin Patch\n*** Update File: f.txt\n@@\n a\n\n-b\n+c\n*** End Patch\n";

        let outcome = execute(&tool, patch).await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(read(dir.path(), "f.txt"), "a\n\nc\n");
    }

    #[tokio::test]
    async fn a_missing_update_target_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Update File: gone.txt\n-a\n+b\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Error, "{outcome:?}");
        assert_eq!(outcome.content, "gone.txt does not exist.");
    }

    #[tokio::test]
    async fn delete_removes_the_file() {
        let dir = tempfile::tempdir().unwrap();
        write(dir.path(), "gone.txt", "bye\n");
        let (tool, _) = tool(dir.path());

        let outcome = execute(
            &tool,
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n",
        )
        .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{outcome:?}");
        assert_eq!(outcome.content, "D gone.txt");
        assert!(!dir.path().join("gone.txt").exists());
    }

    #[tokio::test]
    async fn written_files_are_recorded_as_observed() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, observed) = tool(dir.path());

        execute(
            &tool,
            "*** Begin Patch\n*** Add File: new.txt\n+contents\n*** End Patch\n",
        )
        .await;

        assert_eq!(
            observed.check_unchanged(&dir.path().join("new.txt"), b"contents\n"),
            Observation::Unchanged
        );
    }

    #[test]
    fn declaration_is_freeform_with_the_v4a_grammar() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.declaration().name, "apply_patch");
        match &tool.declaration().kind {
            DeclarationKind::Freeform {
                grammar: Some(Grammar { syntax, definition }),
            } => {
                assert_eq!(syntax, "lark");
                assert_eq!(definition, PATCH_GRAMMAR);
            }
            other => panic!("expected a freeform declaration, got {other:?}"),
        }
    }

    #[test]
    fn function_face_declares_the_exact_schema() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let tool = tool.function_face();

        assert_eq!(
            tool.declaration().kind,
            DeclarationKind::Function {
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": { "patch": { "type": "string" } },
                    "required": ["patch"],
                    "additionalProperties": false
                })
            }
        );
        assert_eq!(tool.identity().variant, "function");
    }

    #[test]
    fn identity_defaults_to_gpt_and_survives_a_face_change() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.identity().implementation, "p1-tool-patch");
        assert_eq!(tool.identity().variant, "gpt");

        let reshaped = tool.with_face(ToolFace::new("Patch", "custom"), "claude");
        assert_eq!(reshaped.declaration().name, "Patch");
        assert_eq!(reshaped.declaration().description, "custom");
        assert_eq!(reshaped.identity().variant, "claude");
        assert!(matches!(
            reshaped.declaration().kind,
            DeclarationKind::Freeform { .. }
        ));
    }

    #[test]
    fn effect_is_writes_files() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        assert_eq!(tool.effect(&text_call("")), Effect::WritesFiles);
    }

    /// ADR-0057: the freeform patch is parsed the same way `execute` parses it, so
    /// `describe` names the first file it touches — or the count. A renamed face
    /// changes nothing.
    #[test]
    fn describe_parses_the_freeform_text_for_the_file_or_the_count() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let one = tool.describe(&text_call(
            "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** End Patch\n",
        ));
        assert_eq!(one.verb, "edit");
        assert_eq!(one.target.as_deref(), Some("src/a.rs"));

        let two = tool.describe(&text_call(
            "*** Begin Patch\n*** Add File: a\n+x\n*** Add File: b\n+y\n*** End Patch\n",
        ));
        assert_eq!(two.target.as_deref(), Some("2 files"));

        let renamed = tool.with_face(ToolFace::new("Patch", "custom"), "claude");
        assert_eq!(
            renamed
                .describe(&text_call(
                    "*** Begin Patch\n*** Delete File: old.rs\n*** End Patch\n"
                ))
                .target
                .as_deref(),
            Some("old.rs")
        );
    }

    #[tokio::test]
    async fn invalid_function_input_reports_a_prefix_and_never_panics() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let tool = tool.function_face();
        let garbage = [
            "",
            "null",
            "[]",
            "{}",
            "{\"patch\": 5}",
            "{\"patch\":\"x\",\"extra\":1}",
        ];
        for raw in garbage {
            let call = ToolCall {
                call_id: "call-1".into(),
                name: "apply_patch".into(),
                input: ToolInput::Json(raw.to_string()),
            };
            let context = ToolContext {
                cancel: CancellationToken::new(),
            };
            let outcome = tool.execute(&call, context).await;
            assert_eq!(outcome.status, ToolStatus::Error, "input: {raw:?}");
            assert!(
                outcome
                    .content
                    .starts_with("Invalid input for apply_patch: "),
                "input: {raw:?} -> {outcome:?}"
            );
        }
    }

    #[tokio::test]
    async fn the_wrong_input_kind_is_invalid_input() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let json_call = ToolCall {
            call_id: "call-1".into(),
            name: "apply_patch".into(),
            input: ToolInput::Json("{}".into()),
        };
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        let outcome = tool.execute(&json_call, context).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome
                .content
                .starts_with("Invalid input for apply_patch: ")
        );

        let function_tool = tool.function_face();
        let patch_call = text_call("*** Begin Patch\n*** End Patch\n");
        let context = ToolContext {
            cancel: CancellationToken::new(),
        };
        let outcome = function_tool.execute(&patch_call, context).await;
        assert_eq!(outcome.status, ToolStatus::Error);
        assert!(
            outcome
                .content
                .starts_with("Invalid input for apply_patch: ")
        );
    }

    #[tokio::test]
    async fn execute_returns_cancelled_without_touching_the_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let (tool, _) = tool(dir.path());
        let call = text_call("*** Begin Patch\n*** Add File: new.txt\n+x\n*** End Patch\n");
        let cancel = CancellationToken::new();
        cancel.cancel();

        let outcome = tool.execute(&call, ToolContext { cancel }).await;

        assert_eq!(outcome.status, ToolStatus::Cancelled);
        assert!(!dir.path().join("new.txt").exists());
    }
}
