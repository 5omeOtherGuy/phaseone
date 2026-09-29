//! The V4A patch itself: parsing it into hunks, and planning every resulting file in memory
//! over an abstract view of the workspace ([`Files`]).
//!
//! The native `p1-tool-patch` plans over the real filesystem under its write gate; the
//! component plans over its imported `workspace` capability outside the gate. Both call
//! [`plan`], so the order of checks — and therefore which error the model sees first — is
//! one piece of code.

use std::collections::HashMap;
use std::hash::Hash;

use crate::{already_exists, does_not_exist, not_valid_utf8};

/// One parsed patch hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
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

/// One `@@`-delimited search group inside an Update File hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateGroup {
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

/// The paths a parsed patch touches, in patch order (the source path of an
/// `Update File` hunk, even when it also moves).
pub fn hunk_paths(hunks: &[Hunk]) -> Vec<String> {
    hunks
        .iter()
        .map(|hunk| match hunk {
            Hunk::Add { path, .. } | Hunk::Delete { path } | Hunk::Update { path, .. } => {
                path.clone()
            }
        })
        .collect()
}

/// Whether any path the patch names — a move target included — leaves the workspace, as
/// `escapes` decides it.
pub fn patch_is_destructive(hunks: &[Hunk], mut escapes: impl FnMut(&str) -> bool) -> bool {
    hunks.iter().any(|hunk| match hunk {
        Hunk::Add { path, .. } | Hunk::Delete { path } => escapes(path),
        Hunk::Update { path, moveto, .. } => {
            escapes(path) || moveto.as_deref().is_some_and(&mut escapes)
        }
    })
}

/// Why applying a patch failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchFailure {
    /// A message the model can act on.
    Message(String),
    /// The cancellation token was set; nothing more was done.
    Cancelled,
}

fn invalid_patch(reason: impl std::fmt::Display, line: usize) -> PatchFailure {
    PatchFailure::Message(format!("Invalid patch: {reason} (line {line})."))
}

/// Strip an optional heredoc or fenced-code wrapper, normalize CRLF line
/// endings, and drop surrounding blank lines.
pub(crate) fn strip_wrapper(text: &str) -> String {
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

/// Parse a V4A patch; a failure is always an `Invalid patch: …` message.
pub fn parse_patch(text: &str) -> Result<Vec<Hunk>, PatchFailure> {
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

/// The workspace as planning sees it. `Key` names one file: the native tool's resolved path,
/// the component's root-relative path. Two requests that name one file must give one key,
/// so a later hunk sees what an earlier one staged.
pub trait Files {
    type Key: Clone + Eq + Hash;

    /// Whether the call was cancelled; checked before every hunk.
    fn cancelled(&mut self) -> bool;
    /// Confine `path`: its key and the root-relative form the model is shown.
    fn resolve(&mut self, path: &str) -> Result<(Self::Key, String), PatchFailure>;
    /// Whether anything is at `key` on disk (a file, a directory, …).
    fn exists(&mut self, key: &Self::Key) -> bool;
    /// The regular file at `key` on disk, or `None` when nothing is there. `display` is for
    /// the failure texts ([`crate::could_not_be_read`], [`crate::not_a_regular_file`]).
    fn read(&mut self, key: &Self::Key, display: &str) -> Result<Option<Vec<u8>>, PatchFailure>;
}

/// A validated mutation, applied in patch order once planning has succeeded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op<K> {
    Add {
        path: K,
        display: String,
        contents: Vec<u8>,
    },
    Modify {
        path: K,
        display: String,
        contents: Vec<u8>,
    },
    Delete {
        path: K,
        display: String,
    },
    Move {
        from: K,
        from_display: String,
        to: K,
        to_display: String,
        contents: Vec<u8>,
    },
}

impl<K> Op<K> {
    /// The line this change adds to the success output.
    pub fn success_line(&self) -> String {
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

/// The success output of a whole patch: one line per change, in patch order.
pub fn success_output<K>(ops: &[Op<K>]) -> String {
    ops.iter()
        .map(Op::success_line)
        .collect::<Vec<_>>()
        .join("\n")
}

/// One change of a whole patch after [`coalesce`]: a resolved path the flow changes at most
/// once, with the contents it ends up holding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Change<K> {
    /// Nothing was at the path before the patch (an `Add`, or a move's target): the host must
    /// refuse a path an ungated writer filled meanwhile rather than overwrite it.
    Create {
        path: K,
        display: String,
        contents: Vec<u8>,
    },
    /// The path held a file before the patch and holds these contents after it.
    Write {
        path: K,
        display: String,
        contents: Vec<u8>,
    },
    /// The path held a file before the patch and holds nothing after it.
    Remove { path: K, display: String },
}

/// Reduce `ops` to at most one change per resolved path, with the contents the path ends up
/// holding.
///
/// A patch may reach one file more than once — two `Update File` hunks on it, or a `Move`
/// after an `Update` — and the native tool, planning and applying under the gate, writes it
/// once per op in patch order. The component must not: the host rechecks every target under
/// the gate against what this call read (docs/design/modules/workspace-mutation.md, step 3),
/// so the first write would leave the second target's digest different from the one this call
/// read and the second change would be refused as stale after the first was applied. One
/// change per path means no change the host could refuse as stale, and it is the plan a batch
/// `commit` would take.
///
/// Every op keeps its success line ([`success_output`] reads the ops, not these changes). The
/// first op that touches a path decides whether the host treats it as new (create-only) or as
/// a file to replace or remove, so a path whose first op is a creation keeps the create-for-add
/// and move-target semantics. A path a patch creates and then removes again (an `Add` before a
/// `Delete`, or a staged file a later `Move` carries away) needs no change at all: it is absent
/// before and after the patch.
///
/// The changes come out in the order of the op that set each path's final state, with a
/// creation before a removal of the same op: a move still creates its target before it removes
/// its source, so a failure in between loses nothing, exactly as the native apply does.
pub fn coalesce<K: Clone + Eq>(ops: &[Op<K>]) -> Vec<Change<K>> {
    /// One path's state after the whole patch.
    struct Final<K> {
        path: K,
        display: String,
        /// The contents the path ends up with, or `None` when it is gone.
        contents: Option<Vec<u8>>,
        /// Whether the patch's first op on this path created it.
        created: bool,
        /// The op that set this final state, and whether it removes (1) or writes (0) the
        /// path; the change keeps that position in the patch's order.
        position: (usize, usize),
    }

    /// Record that `path` ends up holding `contents` (`None`: gone) at `position`. The first
    /// call for a path fixes `created`; a later one only moves the path's final state along.
    fn touch<K: Clone + Eq>(
        finals: &mut Vec<Final<K>>,
        path: &K,
        display: &str,
        contents: Option<Vec<u8>>,
        created: bool,
        position: (usize, usize),
    ) {
        match finals.iter_mut().find(|entry| entry.path == *path) {
            Some(entry) => {
                entry.contents = contents;
                entry.display = display.to_string();
                entry.position = position;
            }
            None => finals.push(Final {
                path: path.clone(),
                display: display.to_string(),
                contents,
                created,
                position,
            }),
        }
    }

    let mut finals: Vec<Final<K>> = Vec::new();
    for (index, op) in ops.iter().enumerate() {
        let write = (index, 0);
        match op {
            Op::Add {
                path,
                display,
                contents,
            } => touch(
                &mut finals,
                path,
                display,
                Some(contents.clone()),
                true,
                write,
            ),
            Op::Modify {
                path,
                display,
                contents,
            } => touch(
                &mut finals,
                path,
                display,
                Some(contents.clone()),
                false,
                write,
            ),
            Op::Delete { path, display } => touch(&mut finals, path, display, None, false, write),
            Op::Move {
                from,
                from_display,
                to,
                to_display,
                contents,
            } => {
                // The target first, then the source: the native move's own order within one
                // op, which a shared op index keeps.
                touch(
                    &mut finals,
                    to,
                    to_display,
                    Some(contents.clone()),
                    true,
                    write,
                );
                touch(&mut finals, from, from_display, None, false, (index, 1));
            }
        }
    }

    let mut changes: Vec<(usize, usize, Change<K>)> = finals
        .into_iter()
        .filter_map(|entry| {
            let change = match (entry.contents, entry.created) {
                (Some(contents), true) => Change::Create {
                    path: entry.path,
                    display: entry.display,
                    contents,
                },
                (Some(contents), false) => Change::Write {
                    path: entry.path,
                    display: entry.display,
                    contents,
                },
                (None, false) => Change::Remove {
                    path: entry.path,
                    display: entry.display,
                },
                // Created by this patch and absent before it: nothing to change.
                (None, true) => return None,
            };
            Some((entry.position.0, entry.position.1, change))
        })
        .collect();
    // Stable, so paths of one op keep the order they were first touched in.
    changes.sort_by_key(|(index, removal, _)| (*index, *removal));
    changes.into_iter().map(|(_, _, change)| change).collect()
}

/// Validate every hunk and compute every resulting file in memory.
pub fn plan<F: Files>(files: &mut F, hunks: &[Hunk]) -> Result<Vec<Op<F::Key>>, PatchFailure> {
    // Staged contents for the paths already touched by this patch, so a later
    // hunk sees the result of an earlier one without anything being written.
    let mut staged: HashMap<F::Key, Option<Vec<u8>>> = HashMap::new();
    let mut ops = Vec::new();

    for hunk in hunks {
        if files.cancelled() {
            return Err(PatchFailure::Cancelled);
        }
        match hunk {
            Hunk::Add { path, lines } => {
                let (resolved, display) = files.resolve(path)?;
                if is_present(files, &resolved, &staged) {
                    return Err(PatchFailure::Message(already_exists(&display)));
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
                let (resolved, display) = files.resolve(path)?;
                if current_contents(files, &resolved, &display, &staged)?.is_none() {
                    return Err(PatchFailure::Message(does_not_exist(&display)));
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
                let (resolved, display) = files.resolve(path)?;
                let bytes = current_contents(files, &resolved, &display, &staged)?
                    .ok_or_else(|| PatchFailure::Message(does_not_exist(&display)))?;
                let contents = apply_groups(&bytes, groups, &display)?;
                match moveto {
                    Some(target) => {
                        let (to, to_display) = files.resolve(target)?;
                        if is_present(files, &to, &staged) {
                            return Err(PatchFailure::Message(already_exists(&to_display)));
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

/// Whether `key` exists, counting a path staged earlier in this patch.
fn is_present<F: Files>(
    files: &mut F,
    key: &F::Key,
    staged: &HashMap<F::Key, Option<Vec<u8>>>,
) -> bool {
    match staged.get(key) {
        Some(entry) => entry.is_some(),
        None => files.exists(key),
    }
}

/// The current bytes of `key`: staged content when this patch touched it
/// already, otherwise the file on disk, which must be UTF-8.
fn current_contents<F: Files>(
    files: &mut F,
    key: &F::Key,
    display: &str,
    staged: &HashMap<F::Key, Option<Vec<u8>>>,
) -> Result<Option<Vec<u8>>, PatchFailure> {
    if let Some(entry) = staged.get(key) {
        return Ok(entry.clone());
    }
    let Some(bytes) = files.read(key, display)? else {
        return Ok(None);
    };
    if std::str::from_utf8(&bytes).is_err() {
        return Err(PatchFailure::Message(not_valid_utf8(display)));
    }
    Ok(Some(bytes))
}

/// Apply the parsed groups to a file's bytes, preserving its CRLF style and
/// trailing-newline state.
fn apply_groups(
    bytes: &[u8],
    groups: &[UpdateGroup],
    display: &str,
) -> Result<Vec<u8>, PatchFailure> {
    let text =
        std::str::from_utf8(bytes).map_err(|_| PatchFailure::Message(not_valid_utf8(display)))?;
    let crlf = detect_crlf(text);
    let default_ending = if crlf { "\r\n" } else { "\n" };
    let normalized = text.replace("\r\n", "\n");
    let trailing_newline = normalized.ends_with('\n');
    let mut lines = split_lines(&normalized);
    let mut endings: Vec<String> = text
        .split_inclusive('\n')
        .map(|part| {
            if part.ends_with("\r\n") {
                "\r\n"
            } else if part.ends_with('\n') {
                "\n"
            } else {
                ""
            }
            .to_owned()
        })
        .collect();

    let mut running = 0usize;
    for (index, group) in groups.iter().enumerate() {
        let position = locate(&lines, group, running).ok_or_else(|| {
            PatchFailure::Message(format!(
                "{display}: hunk {} did not match the file.",
                index + 1
            ))
        })?;
        let end = position + group.old.len();
        let previous = lines[position..end].to_vec();
        let previous_endings = endings[position..end].to_vec();
        let mut next_old = 0;
        let replacement_endings = group
            .new
            .iter()
            .enumerate()
            .map(|(index, line)| {
                if let Some(relative) = previous[next_old..].iter().position(|old| old == line) {
                    next_old += relative + 1;
                    previous_endings[next_old - 1].clone()
                } else {
                    previous_endings
                        .get(index)
                        .filter(|ending| !ending.is_empty())
                        .cloned()
                        .unwrap_or_else(|| default_ending.to_owned())
                }
            })
            .collect::<Vec<_>>();
        lines.splice(position..end, group.new.iter().cloned());
        endings.splice(position..end, replacement_endings);
        running = position + group.new.len();
    }

    let last = lines.len().saturating_sub(1);
    let mut joined = String::new();
    for (index, (line, ending)) in lines.iter().zip(endings.iter()).enumerate() {
        joined.push_str(line);
        if index < last || trailing_newline {
            joined.push_str(if ending.is_empty() {
                default_ending
            } else {
                ending
            });
        }
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

#[cfg(test)]
mod tests {
    use super::*;

    /// An in-memory workspace keyed by the request itself.
    #[derive(Default)]
    struct Memory {
        files: HashMap<String, Vec<u8>>,
        dirs: Vec<String>,
        cancel: bool,
    }

    impl Files for Memory {
        type Key = String;

        fn cancelled(&mut self) -> bool {
            self.cancel
        }

        fn resolve(&mut self, path: &str) -> Result<(String, String), PatchFailure> {
            if path.starts_with("..") {
                return Err(PatchFailure::Message(format!(
                    "path escapes workspace: {path}"
                )));
            }
            Ok((path.to_string(), path.to_string()))
        }

        fn exists(&mut self, key: &String) -> bool {
            self.files.contains_key(key) || self.dirs.contains(key)
        }

        fn read(&mut self, key: &String, display: &str) -> Result<Option<Vec<u8>>, PatchFailure> {
            if self.dirs.contains(key) {
                return Err(PatchFailure::Message(crate::not_a_regular_file(display)));
            }
            Ok(self.files.get(key).cloned())
        }
    }

    fn memory(files: &[(&str, &str)]) -> Memory {
        Memory {
            files: files
                .iter()
                .map(|(path, contents)| (path.to_string(), contents.as_bytes().to_vec()))
                .collect(),
            ..Memory::default()
        }
    }

    fn planned(files: &mut Memory, patch: &str) -> Result<Vec<Op<String>>, PatchFailure> {
        plan(files, &parse_patch(patch)?)
    }

    fn message(text: &str) -> PatchFailure {
        PatchFailure::Message(text.into())
    }

    #[test]
    fn a_later_hunk_sees_what_an_earlier_one_staged() {
        let mut files = memory(&[]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Add File: a\n+one\n*** Update File: a\n-one\n+two\n*** Update File: a\n*** Move to: b\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "A a\nM a\nM a -> b");
        assert_eq!(
            ops[2],
            Op::Move {
                from: "a".into(),
                from_display: "a".into(),
                to: "b".into(),
                to_display: "b".into(),
                contents: b"two\n".to_vec(),
            }
        );
        // Nothing reached the "disk": planning only computes.
        assert!(files.files.is_empty());
    }

    #[test]
    fn a_deleted_path_may_be_added_again_and_an_added_one_not_twice() {
        let mut files = memory(&[("a", "x\n")]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Delete File: a\n*** Add File: a\n+y\n*** End Patch\n",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "D a\nA a");
        assert_eq!(
            planned(
                &mut files,
                "*** Begin Patch\n*** Add File: n\n+y\n*** Add File: n\n+z\n*** End Patch\n",
            ),
            Err(message("n already exists."))
        );
    }

    fn write(path: &str, contents: &str) -> Change<String> {
        Change::Write {
            path: path.into(),
            display: path.into(),
            contents: contents.as_bytes().to_vec(),
        }
    }

    fn create(path: &str, contents: &str) -> Change<String> {
        Change::Create {
            path: path.into(),
            display: path.into(),
            contents: contents.as_bytes().to_vec(),
        }
    }

    fn remove(path: &str) -> Change<String> {
        Change::Remove {
            path: path.into(),
            display: path.into(),
        }
    }

    #[test]
    fn one_path_changed_twice_becomes_one_change_with_its_final_contents() {
        let mut files = memory(&[("f", "a\n")]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Update File: f\n-a\n+b\n*** Update File: f\n-b\n+c\n*** End Patch",
        )
        .unwrap();
        // Every op keeps its success line; only the changes are coalesced.
        assert_eq!(success_output(&ops), "M f\nM f");
        assert_eq!(coalesce(&ops), [write("f", "c\n")]);
    }

    #[test]
    fn a_change_a_later_move_carries_away_leaves_one_change() {
        // `f` is updated and then moved to `g`: it is replaced once and removed once, and
        // the destination is created once, whatever the ops' order.
        let mut files = memory(&[("f", "a\n")]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Update File: f\n-a\n+b\n*** Update File: f\n*** Move to: g\n*** End Patch",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "M f\nM f -> g");
        // The target is created before the source is removed, as the native move writes it.
        assert_eq!(coalesce(&ops), [create("g", "b\n"), remove("f")]);
    }

    #[test]
    fn a_path_created_then_removed_needs_no_change() {
        let mut files = memory(&[]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Add File: n\n+y\n*** Delete File: n\n*** End Patch",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "A n\nD n");
        assert_eq!(coalesce(&ops), []);

        // The same through a move: the staged file `a` ends up as `b`, so `a` is absent
        // before the patch and absent after it, and only `b` is created.
        let mut files = memory(&[]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Add File: a\n+one\n*** Update File: a\n-one\n+two\n*** Update File: a\n*** Move to: b\n*** End Patch",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "A a\nM a\nM a -> b");
        assert_eq!(coalesce(&ops), [create("b", "two\n")]);
    }

    #[test]
    fn a_removed_path_added_again_is_one_replacement() {
        // `a` held a file before the patch and holds one after it: one replacement, never a
        // unlink and a create (which would be two changes to one path).
        let mut files = memory(&[("a", "x\n")]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Delete File: a\n*** Add File: a\n+y\n*** End Patch",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "D a\nA a");
        assert_eq!(coalesce(&ops), [write("a", "y\n")]);
    }

    #[test]
    fn a_created_path_keeps_the_create_only_semantics() {
        // Nothing was at `a`; a later hunk only changed what the patch itself put there, so
        // the host is still asked to create it (refusing a path filled meanwhile) rather
        // than to replace it.
        let mut files = memory(&[]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Add File: a\n+one\n*** Update File: a\n-one\n+two\n*** End Patch",
        )
        .unwrap();
        assert_eq!(success_output(&ops), "A a\nM a");
        assert_eq!(coalesce(&ops), [create("a", "two\n")]);
    }

    #[test]
    fn a_patch_preserves_unmatched_mixed_line_endings() {
        let mut files = memory(&[("mixed", "a\r\nb\nc\r\n")]);
        let ops = planned(
            &mut files,
            "*** Begin Patch\n*** Update File: mixed\n-b\n+B\n*** End Patch",
        )
        .unwrap();
        assert_eq!(
            ops[0],
            Op::Modify {
                path: "mixed".into(),
                display: "mixed".into(),
                contents: b"a\r\nB\nc\r\n".to_vec()
            }
        );
    }

    #[test]
    fn planning_failures_are_the_native_texts_in_the_native_order() {
        let mut files = memory(&[("f", "a\n"), ("bin", "")]);
        files.files.insert("bin".into(), vec![0xff, 0xfe]);
        files.dirs.push("d".into());
        let cases = [
            (
                "*** Begin Patch\n*** Update File: gone\n-a\n+b\n*** End Patch\n",
                "gone does not exist.",
            ),
            (
                "*** Begin Patch\n*** Delete File: gone\n*** End Patch\n",
                "gone does not exist.",
            ),
            (
                "*** Begin Patch\n*** Update File: f\n-zzz\n+b\n*** End Patch\n",
                "f: hunk 1 did not match the file.",
            ),
            (
                "*** Begin Patch\n*** Update File: bin\n-a\n+b\n*** End Patch\n",
                "bin is not valid UTF-8.",
            ),
            (
                "*** Begin Patch\n*** Update File: d\n-a\n+b\n*** End Patch\n",
                "d is not a regular file.",
            ),
            (
                "*** Begin Patch\n*** Add File: d\n+a\n*** End Patch\n",
                "d already exists.",
            ),
            (
                "*** Begin Patch\n*** Add File: ../x\n+a\n*** End Patch\n",
                "path escapes workspace: ../x",
            ),
            (
                "*** Begin Patch\n*** Update File: f\n*** Move to: d\n*** End Patch\n",
                "d already exists.",
            ),
        ];
        for (patch, expected) in cases {
            assert_eq!(
                planned(&mut files, patch),
                Err(message(expected)),
                "{patch}"
            );
        }
    }

    #[test]
    fn cancellation_is_checked_before_every_hunk() {
        let mut files = memory(&[]);
        files.cancel = true;
        assert_eq!(
            planned(
                &mut files,
                "*** Begin Patch\n*** Add File: a\n+x\n*** End Patch\n"
            ),
            Err(PatchFailure::Cancelled)
        );
    }

    #[test]
    fn garbage_is_an_invalid_patch_with_its_line() {
        let cases = [
            (
                "",
                "Invalid patch: the first line must be \"*** Begin Patch\" (line 1).",
            ),
            (
                "*** Begin Patch\n*** Add File: z.txt\n+x\n",
                "Invalid patch: the patch must end with \"*** End Patch\" (line 3).",
            ),
            (
                "*** Begin Patch\n*** End Patch\n",
                "Invalid patch: the patch contains no hunks (line 1).",
            ),
            (
                "*** Begin Patch\n*** Frobnicate: x\n*** End Patch\n",
                "Invalid patch: expected a hunk header, found \"*** Frobnicate: x\" (line 2).",
            ),
            (
                "*** Begin Patch\n*** Update File: f\n@@ a\n*** End Patch\n",
                "Invalid patch: Update hunk for f contains no lines (line 3).",
            ),
            (
                "*** Begin Patch\n*** Add File: a\n+x\n*** End Patch\ntrailing\n",
                "Invalid patch: unexpected content after \"*** End Patch\" (line 5).",
            ),
        ];
        for (patch, expected) in cases {
            assert_eq!(parse_patch(patch), Err(message(expected)), "{patch:?}");
        }
    }

    #[test]
    fn destructive_asks_about_every_path_including_a_move_target() {
        let hunks =
            parse_patch("*** Begin Patch\n*** Update File: a\n*** Move to: ../b\n*** End Patch\n")
                .unwrap();
        assert!(patch_is_destructive(&hunks, |path| path.starts_with("..")));
        assert!(!patch_is_destructive(&hunks, |path| path == "c"));
        assert_eq!(hunk_paths(&hunks), ["a"]);
    }
}
