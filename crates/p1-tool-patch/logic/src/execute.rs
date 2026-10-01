//! The `execute` flow of the `apply_patch` component over an abstract [`Host`]: its
//! imported capabilities, or a fake in tests.
//!
//! The order is the point: the input is validated, the patch parsed, every file it needs
//! read through `workspace` and every resulting file computed ([`crate::plan`]) **before**
//! the write gate is taken; the gate is held only for the changes themselves. No
//! observation is checked: the patch exemption (ADR-0025) — the hunks must match the files'
//! current contents, which is the patch's own staleness check — and the host's mutation is
//! assembled patch-authorized, so it does not check one either. Nothing is recorded here:
//! the host records every written file in the agent's observations as it commits the
//! change (docs/design/modules/workspace-mutation.md, step 6), which is why this flow
//! imports no `snapshot` (ADR-0088 point 4, workspace-mutation.md: "patch links no
//! snapshot").
//!
//! The planned ops are [coalesced](crate::coalesce) to one change per resolved path, so a
//! path a patch writes twice (two hunks on one file, or a move after an update) is written
//! once with its final contents and the host never rechecks a target this call already
//! changed — where the native tool, planning under the gate, writes it once per op.
//!
//! Atomicity is per file, exactly as natively: an invalid patch changes nothing, because
//! every hunk is planned before the first change, but a failure or a cancellation between
//! two changes leaves the earlier ones applied.

use std::collections::{HashMap, HashSet};

use crate::patch::{Change, Files, Op, PatchFailure, coalesce, parse_patch, plan, success_output};
use crate::{
    RawInput, already_exists, bounded, could_not_be_read, display_of, does_not_exist,
    failed_to_delete, failed_to_write, not_a_regular_file, patch_text, relative_display,
};

/// How many bytes one `workspace.read` asks for. A window, not a limit: a file the patch
/// needs is read whole, window by window, checking cancellation between windows.
pub const READ_WINDOW: u64 = 1 << 20;

/// Why a workspace operation failed; the WIT `fs-error`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FsError {
    OutsideWorkspace,
    NotFound,
    WrongKind,
    AlreadyExists,
    InvalidPattern(String),
    Cancelled,
    Io(String),
}

/// What a path is; the WIT `entry-kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

/// What `stat` reports; the WIT `entry`. `path` is root-relative, as the model sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
    pub size: u64,
}

/// How a call ended; the `apply_patch` tool reaches only these three `ToolStatus`es.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Ok,
    Error,
    Cancelled,
}

/// A call's outcome: `content` is exactly what the model is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub status: Status,
    pub content: String,
}

impl Outcome {
    fn ok(content: String) -> Self {
        Self {
            status: Status::Ok,
            content,
        }
    }

    fn error(content: String) -> Self {
        Self {
            status: Status::Error,
            content,
        }
    }

    /// Empty content, as the native tools report a cancellation.
    fn cancelled() -> Self {
        Self {
            status: Status::Cancelled,
            content: String::new(),
        }
    }
}

/// The capabilities the flow uses: `control`, `workspace` and `workspace-mutation`, one
/// method per import. Not `snapshot`: the host records what a change wrote itself.
pub trait Host {
    /// The held write gate `begin` returns.
    type Mutation: Mutation;

    /// `control.cancelled`.
    fn cancelled(&mut self) -> bool;
    /// `workspace.stat`.
    fn stat(&mut self, path: &str) -> Result<Entry, FsError>;
    /// `workspace.read`: up to `length` bytes from `offset`, fewer only at the end.
    fn read(&mut self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError>;
    /// `workspace-mutation.begin`: waits for the write gate. Dropping the value releases it.
    fn begin(&mut self) -> Self::Mutation;
}

/// The WIT `mutation` resource: the write gate, held.
pub trait Mutation {
    /// `mutation.write`: atomic replacement, missing parents created by the host.
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError>;
    /// `mutation.create`: as `write`, but `already-exists` when anything is at `path`.
    fn create(&self, path: &str, contents: &[u8]) -> Result<(), FsError>;
    /// `mutation.remove`.
    fn remove(&self, path: &str) -> Result<(), FsError>;
}

/// Run one `apply_patch` call named `tool` (the name the model called it by, which invalid
/// input names) over `host`; `freeform` is the declaration form the call is read in.
pub fn execute<H: Host>(host: &mut H, tool: &str, freeform: bool, input: RawInput<'_>) -> Outcome {
    // Cancellation before any work: touch nothing, not even a stat.
    if host.cancelled() {
        return Outcome::cancelled();
    }
    let text = match patch_text(tool, freeform, input) {
        Ok(text) => text,
        Err(message) => return Outcome::error(message),
    };
    match run(host, &text) {
        Ok(content) => Outcome::ok(bounded(&content)),
        Err(PatchFailure::Cancelled) => Outcome::cancelled(),
        Err(PatchFailure::Message(message)) => Outcome::error(message),
    }
}

fn run<H: Host>(host: &mut H, text: &str) -> Result<String, PatchFailure> {
    let hunks = parse_patch(text)?;
    // Planning reads every file the hunks are located in and computes every result, all
    // outside the gate. The host rechecks each target under the gate against what this
    // call read, so a file another agent changes in between is refused as stale rather
    // than overwritten (docs/design/modules/workspace-mutation.md, step 3).
    let ops = plan(
        &mut HostFiles {
            host: &mut *host,
            found: HashMap::new(),
            absent: HashMap::new(),
        },
        &hunks,
    )?;
    // One change per resolved path, with its final contents: a path the patch reaches
    // twice must not be written twice, or the host's recheck would refuse the second
    // write as stale against what this call read (see `coalesce`).
    let changes = coalesce(&ops);
    let transient = created_and_removed(&ops, &changes);

    // The last point where stopping leaves the workspace untouched.
    stop_if_cancelled(host)?;
    let mutation = host.begin();
    // A path the patch creates and removes again gets no change, but planning found it
    // empty outside the gate: it is checked once more under the gate, before any change,
    // so a path another writer filled meanwhile refuses the whole patch as the native
    // planning under the gate would (owner decision 2026-10-01).
    for (path, display) in transient {
        check_absent(host, path, display)?;
    }
    for change in &changes {
        // As natively, a cancellation between two changes keeps the earlier ones.
        stop_if_cancelled(host)?;
        apply(host, &mutation, change)?;
    }
    drop(mutation);
    Ok(success_output(&ops))
}

/// One coalesced change under the held gate. Paths are the root-relative keys planning
/// resolved, which the host resolves again.
fn apply<H: Host>(
    host: &mut H,
    mutation: &H::Mutation,
    change: &Change<String>,
) -> Result<(), PatchFailure> {
    match change {
        // A path that held nothing before the patch: `create` keeps it that way under the
        // gate, so a path an ungated writer filled meanwhile is refused, not overwritten.
        Change::Create {
            path,
            display,
            contents,
        } => mutation
            .create(path, contents)
            .map_err(|error| mutation_failure(host, error, display, failed_to_write)),
        Change::Write {
            path,
            display,
            contents,
        } => mutation
            .write(path, contents)
            .map_err(|error| mutation_failure(host, error, display, failed_to_write)),
        Change::Remove { path, display } => mutation
            .remove(path)
            .map_err(|error| mutation_failure(host, error, display, failed_to_delete)),
    }
}

/// The paths the patch creates and then removes again (an `Add` before a `Delete`, or a
/// staged file a later `Move` carries away), each with the display of the op that created
/// it: absent before the patch and after it, so [`coalesce`] gives them no change. As there,
/// the first op on a path decides whether the patch created it.
fn created_and_removed<'o>(
    ops: &'o [Op<String>],
    changes: &[Change<String>],
) -> Vec<(&'o str, &'o str)> {
    let mut seen: HashSet<&str> = HashSet::new();
    let mut created = Vec::new();
    let mut touch = |path: &'o str, display: &'o str, creates: bool| {
        if seen.insert(path) && creates {
            created.push((path, display));
        }
    };
    for op in ops {
        match op {
            Op::Add { path, display, .. } => touch(path, display, true),
            Op::Modify { path, display, .. } | Op::Delete { path, display } => {
                touch(path, display, false);
            }
            Op::Move {
                from,
                from_display,
                to,
                to_display,
                ..
            } => {
                touch(to, to_display, true);
                touch(from, from_display, false);
            }
        }
    }
    let changed: HashSet<&str> = changes
        .iter()
        .map(|change| match change {
            Change::Create { path, .. }
            | Change::Write { path, .. }
            | Change::Remove { path, .. } => path.as_str(),
        })
        .collect();
    created.retain(|(path, _)| !changed.contains(path));
    created
}

/// Under the held gate: `path`, which the patch creates and removes again, still holds
/// nothing. Anything there now is refused with the native planning text, never written.
fn check_absent<H: Host>(host: &mut H, path: &str, display: &str) -> Result<(), PatchFailure> {
    match host.stat(path) {
        Ok(_) => Err(PatchFailure::Message(already_exists(display))),
        Err(FsError::Cancelled) => Err(PatchFailure::Cancelled),
        Err(FsError::OutsideWorkspace) => Err(PatchFailure::Message(escapes_workspace(display))),
        // Nothing there, or nothing that could be (a path through a file), as planning found.
        Err(_) => Ok(()),
    }
}

/// The model-facing text of a refused change. The host's `io` message is already worded as
/// the native tool words it ("failed to write …: <io error>", "failed to delete …", the
/// stale refusal); the typed cases get the native planning texts, which is what the native
/// tool, planning under the gate, would have said about the file as it is now.
fn mutation_failure<H: Host>(
    host: &mut H,
    error: FsError,
    display: &str,
    wrap: fn(&str, &str) -> String,
) -> PatchFailure {
    match error {
        FsError::Cancelled => PatchFailure::Cancelled,
        FsError::Io(message) | FsError::InvalidPattern(message) => PatchFailure::Message(message),
        FsError::AlreadyExists => PatchFailure::Message(already_exists(display)),
        FsError::NotFound => PatchFailure::Message(does_not_exist(display)),
        FsError::OutsideWorkspace => PatchFailure::Message(escapes_workspace(display)),
        FsError::WrongKind => {
            PatchFailure::Message(wrap(display, wrong_kind_reason(host, display)))
        }
    }
}

/// The operating system's text for a change the host refused as `wrong-kind`, as the native
/// `write_atomic` or `remove_file` prints it. The host folds three native errors into that
/// one case, so the kinds around the target decide which it is: a directory at the target
/// is `EISDIR`; a regular file as the target's direct parent is `EEXIST` (the native
/// `create_dir_all` finds a file where the parent directory should be); a file deeper up
/// is `ENOTDIR`. Asked only on this failure path, under the gate the change already holds.
fn wrong_kind_reason<H: Host>(host: &mut H, display: &str) -> &'static str {
    if host
        .stat(display)
        .is_ok_and(|entry| entry.kind == EntryKind::Directory)
    {
        return "Is a directory (os error 21)";
    }
    let parent_is_file = display.rsplit_once('/').is_some_and(|(parent, _)| {
        host.stat(parent)
            .is_ok_and(|entry| entry.kind == EntryKind::File)
    });
    if parent_is_file {
        "File exists (os error 17)"
    } else {
        "Not a directory (os error 20)"
    }
}

/// The native `WorkspaceError::OutsideWorkspace` text.
fn escapes_workspace(request: &str) -> String {
    format!("path escapes workspace: {request}")
}

fn stop_if_cancelled<H: Host>(host: &mut H) -> Result<(), PatchFailure> {
    if host.cancelled() {
        Err(PatchFailure::Cancelled)
    } else {
        Ok(())
    }
}

/// What `stat` said about a resolved key, kept for the `exists` and `read` planning asks
/// next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Found {
    /// A regular file.
    File,
    /// A directory, or anything else that is not a regular file.
    NotFile,
    /// Nothing is there.
    Absent,
    /// The host could not stat the path (a file used as a directory, a denied directory);
    /// the text is the reason the native read would print.
    Unreadable(String),
}

/// The workspace as planning sees it, through the imported `workspace`. A key is the
/// root-relative path: the host's own for an existing entry (symlinks resolved), the
/// native display form for a path where nothing is yet, unless an earlier absent key names
/// the same file through an in-root directory symlink (see [`HostFiles::absent_key`]) — the
/// same file the native tool's key names, so staging works across two spellings of one file
/// as it does there.
struct HostFiles<'h, H> {
    host: &'h mut H,
    found: HashMap<String, Found>,
    /// The keys of the absent paths resolved so far, by their last component.
    absent: HashMap<String, Vec<String>>,
}

impl<H: Host> HostFiles<'_, H> {
    /// The key of `display`, a path where nothing is yet. The host resolves an existing
    /// entry's symlinks, but not an absent one's: `link/x` and `real/x` (with
    /// `link -> real`) are one file, so a later spelling takes the key of an earlier absent
    /// path whose deepest existing ancestor the host resolves to the same directory (owner
    /// decision 2026-10-01: a second addition of it is refused while planning, before any
    /// write). Two spellings of one absent file share its last component, so only such a
    /// pair costs the stats that resolve its ancestors.
    fn absent_key(&mut self, display: String) -> Result<String, PatchFailure> {
        let leaf = display.rsplit('/').next().unwrap_or_default().to_string();
        let rivals: Vec<String> = self
            .absent
            .get(&leaf)
            .into_iter()
            .flatten()
            .filter(|key| **key != display)
            .cloned()
            .collect();
        if !rivals.is_empty() {
            let canonical = canonical_absent(self.host, &display)?;
            for rival in rivals {
                if canonical_absent(self.host, &rival)? == canonical {
                    return Ok(rival);
                }
            }
        }
        let keys = self.absent.entry(leaf).or_default();
        if !keys.contains(&display) {
            keys.push(display.clone());
        }
        Ok(display)
    }
}

/// `display`, a root-relative path where nothing is, with its deepest existing ancestor
/// replaced by the path the host resolves that ancestor to; unchanged when no ancestor below
/// the root exists or one cannot be stat'd.
fn canonical_absent<H: Host>(host: &mut H, display: &str) -> Result<String, PatchFailure> {
    let mut end = display.len();
    while let Some(slash) = display[..end].rfind('/') {
        match host.stat(&display[..slash]) {
            Ok(entry) if entry.path.is_empty() => return Ok(display[slash + 1..].to_string()),
            Ok(entry) => return Ok(format!("{}{}", entry.path, &display[slash..])),
            Err(FsError::NotFound) => end = slash,
            Err(FsError::Cancelled) => return Err(PatchFailure::Cancelled),
            Err(_) => break,
        }
    }
    Ok(display.to_string())
}

impl<H: Host> Files for HostFiles<'_, H> {
    type Key = String;

    fn cancelled(&mut self) -> bool {
        self.host.cancelled()
    }

    fn resolve(&mut self, path: &str) -> Result<(String, String), PatchFailure> {
        let (display, found) = match self.host.stat(path) {
            Ok(entry) => {
                let found = match entry.kind {
                    EntryKind::File => Found::File,
                    // A dangling symlink is `other` too: the native tool follows it and
                    // finds nothing, while here it is something that is not a file. Reading
                    // an `other` entry to tell the two apart could block on a fifo.
                    EntryKind::Directory | EntryKind::Other => Found::NotFile,
                };
                (entry.path, found)
            }
            Err(FsError::NotFound) => (new_file_display(self.host, path)?, Found::Absent),
            Err(FsError::OutsideWorkspace) => {
                return Err(PatchFailure::Message(escapes_workspace(path)));
            }
            Err(FsError::Cancelled) => return Err(PatchFailure::Cancelled),
            // The native resolve succeeds on such a path (nothing exists there) and its read
            // fails: a `wrong-kind` from a stat is a file used as a directory component,
            // `ENOTDIR` to the native read; any other failure carries its own text.
            Err(FsError::WrongKind) => (
                new_file_display(self.host, path)?,
                Found::Unreadable("Not a directory (os error 20)".to_string()),
            ),
            Err(FsError::Io(message) | FsError::InvalidPattern(message)) => (
                new_file_display(self.host, path)?,
                Found::Unreadable(message),
            ),
            Err(FsError::AlreadyExists) => (
                new_file_display(self.host, path)?,
                Found::Unreadable("already exists".to_string()),
            ),
        };
        let key = if found == Found::Absent {
            self.absent_key(display.clone())?
        } else {
            display.clone()
        };
        self.found.insert(key.clone(), found);
        Ok((key, display))
    }

    fn exists(&mut self, key: &String) -> bool {
        // As the native `Path::exists`: a path that cannot be stat'd does not exist.
        matches!(self.found.get(key), Some(Found::File | Found::NotFile))
    }

    fn read(&mut self, key: &String, display: &str) -> Result<Option<Vec<u8>>, PatchFailure> {
        match self.found.get(key).cloned() {
            None | Some(Found::Absent) => Ok(None),
            Some(Found::NotFile) => Err(PatchFailure::Message(not_a_regular_file(display))),
            Some(Found::Unreadable(reason)) => {
                Err(PatchFailure::Message(could_not_be_read(display, &reason)))
            }
            Some(Found::File) => match read_all(self.host, key) {
                Ok(bytes) => Ok(Some(bytes)),
                // Gone since the stat: the native read would have found nothing either.
                Err(FsError::NotFound) => Ok(None),
                Err(FsError::Cancelled) => Err(PatchFailure::Cancelled),
                Err(FsError::OutsideWorkspace) => {
                    Err(PatchFailure::Message(escapes_workspace(key)))
                }
                // The host's `read` reports `wrong-kind` only for a directory, which the
                // native `std::fs::read` prints as `EISDIR`.
                Err(FsError::WrongKind) => Err(PatchFailure::Message(could_not_be_read(
                    display,
                    "Is a directory (os error 21)",
                ))),
                Err(FsError::Io(message) | FsError::InvalidPattern(message)) => {
                    Err(PatchFailure::Message(could_not_be_read(display, &message)))
                }
                Err(FsError::AlreadyExists) => Err(PatchFailure::Message(could_not_be_read(
                    display,
                    "already exists",
                ))),
            },
        }
    }
}

/// The whole file, window by window; a cancellation between windows is `Cancelled`.
fn read_all<H: Host>(host: &mut H, request: &str) -> Result<Vec<u8>, FsError> {
    let mut bytes = Vec::new();
    loop {
        let chunk = host.read(request, bytes.len() as u64, READ_WINDOW)?;
        let short = (chunk.len() as u64) < READ_WINDOW;
        bytes.extend_from_slice(&chunk);
        if short {
            return Ok(bytes);
        }
        if host.cancelled() {
            return Err(FsError::Cancelled);
        }
    }
}

/// The display form of a path where nothing exists yet, as the native
/// `Workspace::display` prints the lexically normalized path under the root.
///
/// A relative request that never climbs above its start is normalized here alone. An
/// absolute request, or one whose `..` climbs out and back in, names the root in its own
/// spelling, which only the host can recognize: the shortest prefix of the normalized
/// request that `stat` places inside the workspace is the root (every shorter prefix of a
/// canonical root is outside it), and the rest of the request is the display form.
fn new_file_display<H: Host>(host: &mut H, request: &str) -> Result<String, PatchFailure> {
    if let Some(display) = relative_display(request) {
        return Ok(display);
    }
    let absolute = request.starts_with('/');
    let parts = normalized_parts(request, absolute);
    // A relative prefix of length zero is the root itself, which the request climbed
    // out of; it is not where the request re-enters.
    let first = usize::from(!absolute);
    for split in first..=parts.len() {
        let prefix = if absolute {
            format!("/{}", parts[..split].join("/"))
        } else {
            parts[..split].join("/")
        };
        match host.stat(&prefix) {
            Ok(entry) => {
                let mut display: Vec<&str> = Vec::new();
                if !entry.path.is_empty() {
                    display.push(&entry.path);
                }
                display.extend_from_slice(&parts[split..]);
                return Ok(display_of(&display));
            }
            Err(FsError::Cancelled) => return Err(PatchFailure::Cancelled),
            Err(_) => {}
        }
    }
    // Unreachable while the host said the request itself is inside the workspace; the
    // normalized request is still the most faithful text.
    Ok(display_of(&parts))
}

/// `request` normalized lexically: `.` dropped, `..` removing the part before it. Above
/// the start, an absolute path stays at `/` and a relative one keeps its leading `..`.
fn normalized_parts(request: &str, absolute: bool) -> Vec<&str> {
    let mut parts: Vec<&str> = Vec::new();
    for part in request.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else if !absolute {
                    parts.push("..");
                }
            }
            name => parts.push(name),
        }
    }
    parts
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// One fake workspace rooted at `/ws`: files by root-relative path, this agent's
    /// observations, whether the gate is held, and every call in order.
    #[derive(Default)]
    struct State {
        files: BTreeMap<String, Vec<u8>>,
        dirs: Vec<String>,
        observed: BTreeMap<String, Vec<u8>>,
        gate_held: bool,
        log: Vec<String>,
        cancel_after: Option<usize>,
        refuse_stat: Option<FsError>,
        refuse_change: Option<FsError>,
        /// A file an ungated writer creates as the gate is taken, after the plan.
        fill_at_begin: Option<(String, Vec<u8>)>,
    }

    #[derive(Default, Clone)]
    struct Fake(Rc<RefCell<State>>);

    struct FakeMutation(Rc<RefCell<State>>);

    const ROOT: &str = "/ws";

    /// The root-relative path of `request`, or `None` outside the fake root.
    fn resolve(request: &str) -> Option<String> {
        if let Some(display) = relative_display(request) {
            return Some(display);
        }
        let joined = if request.starts_with('/') {
            request.to_string()
        } else {
            format!("{ROOT}/{request}")
        };
        let full = format!("/{}", normalized_parts(&joined, true).join("/"));
        if full == ROOT {
            Some(String::new())
        } else {
            full.strip_prefix(&format!("{ROOT}/")).map(str::to_string)
        }
    }

    impl Fake {
        fn with_file(self, path: &str, contents: &str) -> Self {
            self.0
                .borrow_mut()
                .files
                .insert(path.into(), contents.as_bytes().to_vec());
            self
        }

        fn log(&self) -> Vec<String> {
            self.0.borrow().log.clone()
        }

        fn file(&self, path: &str) -> Option<String> {
            self.0
                .borrow()
                .files
                .get(path)
                .map(|bytes| String::from_utf8(bytes.clone()).unwrap())
        }

        fn push(&self, entry: String) {
            self.0.borrow_mut().log.push(entry);
        }

        fn gate_taken(&self) -> bool {
            self.log().iter().any(|entry| entry == "begin")
        }
    }

    impl Host for Fake {
        type Mutation = FakeMutation;

        fn cancelled(&mut self) -> bool {
            let mut state = self.0.borrow_mut();
            let checks = state.log.iter().filter(|e| *e == "cancelled").count();
            state.log.push("cancelled".into());
            state.cancel_after.is_some_and(|after| checks >= after)
        }

        fn stat(&mut self, path: &str) -> Result<Entry, FsError> {
            self.push(format!("stat {path}"));
            if let Some(error) = self.0.borrow_mut().refuse_stat.take() {
                return Err(error);
            }
            let state = self.0.borrow();
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            if resolved.is_empty() || state.dirs.contains(&resolved) {
                return Ok(Entry {
                    path: resolved,
                    kind: EntryKind::Directory,
                    size: 0,
                });
            }
            match state.files.get(&resolved) {
                Some(bytes) => Ok(Entry {
                    path: resolved,
                    kind: EntryKind::File,
                    size: bytes.len() as u64,
                }),
                None => Err(FsError::NotFound),
            }
        }

        fn read(&mut self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
            let held = self.0.borrow().gate_held;
            self.push(format!("read {path} {offset} gate={held}"));
            let state = self.0.borrow();
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            if resolved.is_empty() || state.dirs.contains(&resolved) {
                return Err(FsError::WrongKind);
            }
            let bytes = state.files.get(&resolved).ok_or(FsError::NotFound)?;
            let start = (offset as usize).min(bytes.len());
            let end = start.saturating_add(length as usize).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        }

        fn begin(&mut self) -> FakeMutation {
            self.push("begin".into());
            let mut state = self.0.borrow_mut();
            state.gate_held = true;
            if let Some((path, contents)) = state.fill_at_begin.take() {
                state.files.insert(path, contents);
            }
            drop(state);
            FakeMutation(self.0.clone())
        }
    }

    impl FakeMutation {
        fn change(&self, entry: String) -> Result<(), FsError> {
            let mut state = self.0.borrow_mut();
            assert!(state.gate_held, "{entry} without the gate");
            state.log.push(entry);
            match state.refuse_change.take() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    impl Mutation for FakeMutation {
        fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
            self.change(format!("write {path}"))?;
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            self.0
                .borrow_mut()
                .files
                .insert(resolved, contents.to_vec());
            Ok(())
        }

        fn create(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
            self.change(format!("create {path}"))?;
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            let mut state = self.0.borrow_mut();
            if state.files.contains_key(&resolved) || state.dirs.contains(&resolved) {
                return Err(FsError::AlreadyExists);
            }
            state.files.insert(resolved, contents.to_vec());
            Ok(())
        }

        fn remove(&self, path: &str) -> Result<(), FsError> {
            self.change(format!("remove {path}"))?;
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            self.0
                .borrow_mut()
                .files
                .remove(&resolved)
                .map(|_| ())
                .ok_or(FsError::NotFound)
        }
    }

    impl Drop for FakeMutation {
        fn drop(&mut self) {
            let mut state = self.0.borrow_mut();
            state.gate_held = false;
            state.log.push("release".into());
        }
    }

    fn patch(host: &mut Fake, text: &str) -> Outcome {
        execute(host, "apply_patch", true, RawInput::Text(text))
    }

    fn error(content: &str) -> Outcome {
        Outcome {
            status: Status::Error,
            content: content.into(),
        }
    }

    const THREE_FILES: &str = "*** Begin Patch\n*** Update File: path/to/file.rs\n@@ fn existing_function\n unchanged context line\n-removed line\n+added line\n*** Add File: path/to/new_file.rs\n+first line\n*** Delete File: path/to/old_file.rs\n*** End Patch\n";

    #[test]
    fn every_file_is_read_and_computed_before_the_gate_and_changed_under_it() {
        let mut host = Fake::default()
            .with_file(
                "path/to/file.rs",
                "fn existing_function\nunchanged context line\nremoved line\n",
            )
            .with_file("path/to/old_file.rs", "old\n");
        let outcome = patch(&mut host, THREE_FILES);
        assert_eq!(
            outcome,
            Outcome {
                status: Status::Ok,
                content: "M path/to/file.rs\nA path/to/new_file.rs\nD path/to/old_file.rs".into()
            }
        );
        assert_eq!(
            host.file("path/to/file.rs").as_deref(),
            Some("fn existing_function\nunchanged context line\nadded line\n")
        );
        assert_eq!(
            host.file("path/to/new_file.rs").as_deref(),
            Some("first line\n")
        );
        assert_eq!(host.file("path/to/old_file.rs"), None);
        // No observation is checked (the patch exemption), nothing is recorded here: the
        // host records what a change wrote as it commits it (workspace-mutation.md, step
        // 6), a deletion records nothing, and every change is asked for once.
        assert_eq!(
            host.log(),
            [
                "cancelled",
                "cancelled",
                "stat path/to/file.rs",
                "read path/to/file.rs 0 gate=false",
                "cancelled",
                "stat path/to/new_file.rs",
                "cancelled",
                "stat path/to/old_file.rs",
                "read path/to/old_file.rs 0 gate=false",
                "cancelled",
                "begin",
                "cancelled",
                "write path/to/file.rs",
                "cancelled",
                "create path/to/new_file.rs",
                "cancelled",
                "remove path/to/old_file.rs",
                "release",
            ]
        );
    }

    #[test]
    fn a_change_to_one_path_twice_is_one_change_with_its_final_contents() {
        // Two hunks on one file: the under-the-gate recheck must never see a target this
        // call has already written, so the flow writes `f.txt` once, with the result of
        // both hunks, while the model still sees one line per op.
        let mut host = Fake::default().with_file("f.txt", "a\n");
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: f.txt\n-a\n+b\n*** Update File: f.txt\n-b\n+c\n*** End Patch\n",
        );
        assert_eq!(outcome.content, "M f.txt\nM f.txt");
        assert_eq!(host.file("f.txt").as_deref(), Some("c\n"));
        assert_eq!(
            host.log()
                .into_iter()
                .filter(|entry| ["write ", "create ", "remove "]
                    .iter()
                    .any(|verb| entry.starts_with(verb)))
                .collect::<Vec<String>>(),
            ["write f.txt"]
        );
    }

    #[test]
    fn a_path_created_and_removed_by_one_patch_is_checked_absent_under_the_gate_never_written() {
        // `n.txt` is absent before and after the patch: no change reaches the host, but the
        // path is stat'd once while the gate is held, as the native tool's planning under the
        // gate would find it (owner decision 2026-10-01, X3).
        let mut host = Fake::default();
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Add File: n.txt\n+x\n*** Delete File: n.txt\n*** End Patch\n",
        );
        assert_eq!(outcome.content, "A n.txt\nD n.txt");
        assert_eq!(host.file("n.txt"), None);
        assert_eq!(
            host.log(),
            [
                "cancelled",
                "cancelled",
                "stat n.txt",
                "cancelled",
                "stat n.txt",
                "cancelled",
                "begin",
                "stat n.txt",
                "release",
            ]
        );
    }

    #[test]
    fn a_path_created_and_removed_that_an_ungated_writer_filled_after_the_plan_is_refused() {
        // Another writer creates `n.txt` between the plan and the gate: the check under the
        // gate refuses with the native planning text, and nothing of the patch is applied.
        let mut host = Fake::default().with_file("f.txt", "a\n");
        host.0.borrow_mut().fill_at_begin = Some(("n.txt".into(), b"theirs\n".to_vec()));
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: f.txt\n-a\n+b\n*** Add File: n.txt\n+x\n*** Delete File: n.txt\n*** End Patch\n",
        );
        assert_eq!(outcome, error("n.txt already exists."));
        assert_eq!(host.file("n.txt").as_deref(), Some("theirs\n"));
        assert_eq!(host.file("f.txt").as_deref(), Some("a\n"));
        let log = host.log();
        let begin = log.iter().position(|e| e == "begin").unwrap();
        assert_eq!(log[begin + 1..], ["stat n.txt", "release"], "{log:?}");

        // The same through a move: a staged file carried away is checked as well.
        let mut host = Fake::default();
        host.0.borrow_mut().fill_at_begin = Some(("n.txt".into(), b"theirs\n".to_vec()));
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Add File: n.txt\n+x\n*** Update File: n.txt\n*** Move to: m.txt\n*** End Patch\n",
        );
        assert_eq!(outcome, error("n.txt already exists."));
        assert_eq!(host.file("m.txt"), None);
        assert_eq!(host.file("n.txt").as_deref(), Some("theirs\n"));
    }

    #[test]
    fn a_file_never_observed_is_patched_all_the_same() {
        let mut host = Fake::default().with_file("f.txt", "a\nb\n");
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: f.txt\n-b\n+c\n*** End Patch\n",
        );
        assert_eq!(outcome.content, "M f.txt");
        assert_eq!(host.file("f.txt").as_deref(), Some("a\nc\n"));
        // The exemption means no observation is needed, and this flow records none: it
        // imports no `snapshot`, and the host records the change it applied itself
        // (workspace-mutation.md, step 6).
        assert!(host.0.borrow().observed.is_empty());
    }

    #[test]
    fn a_failing_later_hunk_changes_nothing_and_never_takes_the_gate() {
        let mut host = Fake::default()
            .with_file("first.txt", "one\ntwo\nthree\n")
            .with_file("second.txt", "alpha\n");
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: first.txt\n-one\n+ONE\n*** Update File: second.txt\n-missing\n+other\n*** End Patch\n",
        );
        assert_eq!(outcome, error("second.txt: hunk 1 did not match the file."));
        assert_eq!(host.file("first.txt").as_deref(), Some("one\ntwo\nthree\n"));
        assert!(!host.gate_taken());
    }

    #[test]
    fn a_move_writes_the_target_before_removing_the_source() {
        let mut host = Fake::default().with_file("old.txt", "x\ny\n");
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: old.txt\n*** Move to: sub/new.txt\n@@\n-y\n+z\n*** End Patch\n",
        );
        assert_eq!(outcome.content, "M old.txt -> sub/new.txt");
        assert_eq!(host.file("sub/new.txt").as_deref(), Some("x\nz\n"));
        assert_eq!(host.file("old.txt"), None);
        let log = host.log();
        let create = log.iter().position(|e| e == "create sub/new.txt").unwrap();
        let remove = log.iter().position(|e| e == "remove old.txt").unwrap();
        assert!(create < remove, "{log:?}");
    }

    #[test]
    fn planning_refusals_are_the_native_texts() {
        let mut host = Fake::default().with_file("exists.txt", "original\n");
        host.0.borrow_mut().dirs.push("dir".into());
        let cases = [
            (
                "*** Begin Patch\n*** Add File: exists.txt\n+x\n*** End Patch\n",
                "exists.txt already exists.",
            ),
            (
                "*** Begin Patch\n*** Update File: gone.txt\n-a\n+b\n*** End Patch\n",
                "gone.txt does not exist.",
            ),
            (
                "*** Begin Patch\n*** Update File: dir\n-a\n+b\n*** End Patch\n",
                "dir is not a regular file.",
            ),
            (
                "*** Begin Patch\n*** Add File: ../x\n+hello\n*** End Patch\n",
                "path escapes workspace: ../x",
            ),
            (
                "*** Begin Patch\n*** Add File: /tmp/x\n+hello\n*** End Patch\n",
                "path escapes workspace: /tmp/x",
            ),
        ];
        for (text, expected) in cases {
            assert_eq!(patch(&mut host, text), error(expected), "{text}");
        }
        assert!(!host.gate_taken());
        assert_eq!(host.file("exists.txt").as_deref(), Some("original\n"));
    }

    #[test]
    fn a_path_through_a_file_could_not_be_read_as_a_directory() {
        for refusal in [
            FsError::WrongKind,
            FsError::Io("Not a directory (os error 20)".into()),
        ] {
            let mut host = Fake::default().with_file("d.txt", "x\n");
            host.0.borrow_mut().refuse_stat = Some(refusal);
            let outcome = patch(
                &mut host,
                "*** Begin Patch\n*** Update File: d.txt/x\n-a\n+b\n*** End Patch\n",
            );
            assert_eq!(
                outcome,
                error("d.txt/x could not be read: Not a directory (os error 20)")
            );
        }
    }

    #[test]
    fn a_non_utf8_file_is_refused() {
        let mut host = Fake::default();
        host.0
            .borrow_mut()
            .files
            .insert("bin".into(), vec![0xff, 0xfe, b'a']);
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: bin\n-a\n+b\n*** End Patch\n",
        );
        assert_eq!(outcome, error("bin is not valid UTF-8."));
    }

    #[test]
    fn an_absolute_or_climbing_new_path_displays_relative_to_the_root() {
        let mut host = Fake::default();
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Add File: /ws/a/./b.txt\n+x\n*** Add File: ../ws/c.txt\n+y\n*** End Patch\n",
        );
        assert_eq!(outcome.content, "A a/b.txt\nA c.txt");
        assert_eq!(host.file("a/b.txt").as_deref(), Some("x\n"));
        assert_eq!(host.file("c.txt").as_deref(), Some("y\n"));
    }

    #[test]
    fn two_spellings_of_one_file_share_its_staged_contents() {
        // `/ws/f.txt` and `f.txt` are one file: the second hunk sees the first's result.
        let mut host = Fake::default().with_file("f.txt", "a\n");
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: /ws/f.txt\n-a\n+b\n*** Update File: f.txt\n-b\n+c\n*** End Patch\n",
        );
        assert_eq!(outcome.content, "M f.txt\nM f.txt");
        assert_eq!(host.file("f.txt").as_deref(), Some("c\n"));
    }

    #[test]
    fn a_large_file_is_read_whole_window_by_window() {
        let big = format!("{}\nlast\n", "x".repeat(READ_WINDOW as usize));
        let mut host = Fake::default().with_file("big.txt", &big);
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Update File: big.txt\n-last\n+LAST\n*** End Patch\n",
        );
        assert_eq!(outcome.status, Status::Ok, "{outcome:?}");
        let reads: Vec<String> = host
            .log()
            .into_iter()
            .filter(|entry| entry.starts_with("read "))
            .collect();
        assert_eq!(
            reads,
            [
                "read big.txt 0 gate=false".to_string(),
                format!("read big.txt {READ_WINDOW} gate=false")
            ]
        );
    }

    #[test]
    fn cancellation_is_checked_before_work_between_hunks_and_between_changes() {
        let three = || {
            Fake::default()
                .with_file(
                    "path/to/file.rs",
                    "fn existing_function\nunchanged context line\nremoved line\n",
                )
                .with_file("path/to/old_file.rs", "old\n")
        };
        // Before any work: not even a stat.
        let mut host = three();
        host.0.borrow_mut().cancel_after = Some(0);
        assert_eq!(patch(&mut host, THREE_FILES).status, Status::Cancelled);
        assert_eq!(host.log(), ["cancelled"]);

        // Between two hunks of the plan: nothing is changed and the gate is not taken.
        let mut host = three();
        host.0.borrow_mut().cancel_after = Some(3);
        let outcome = patch(&mut host, THREE_FILES);
        assert_eq!(
            outcome,
            Outcome {
                status: Status::Cancelled,
                content: String::new()
            }
        );
        assert!(!host.gate_taken());

        // After the first change: it stays, as natively (per-file atomicity only).
        let mut host = three();
        host.0.borrow_mut().cancel_after = Some(6);
        let outcome = patch(&mut host, THREE_FILES);
        assert_eq!(outcome.status, Status::Cancelled);
        assert_eq!(
            host.file("path/to/file.rs").as_deref(),
            Some("fn existing_function\nunchanged context line\nadded line\n")
        );
        assert_eq!(host.file("path/to/new_file.rs"), None);
        assert_eq!(host.log().last().map(String::as_str), Some("release"));
    }

    #[test]
    fn a_hosts_refusal_under_the_gate_is_shown_and_releases_the_gate() {
        let stale = "f.txt changed on disk since you last read it; read it again.";
        let cases = [
            (FsError::Io(stale.into()), stale.to_string()),
            (
                FsError::Io("failed to write f.txt: Permission denied (os error 13)".into()),
                "failed to write f.txt: Permission denied (os error 13)".to_string(),
            ),
            (FsError::NotFound, "f.txt does not exist.".to_string()),
            (
                FsError::OutsideWorkspace,
                "path escapes workspace: f.txt".to_string(),
            ),
            (
                FsError::WrongKind,
                "failed to write f.txt: Not a directory (os error 20)".to_string(),
            ),
        ];
        for (refusal, expected) in cases {
            let mut host = Fake::default().with_file("f.txt", "a\n");
            host.0.borrow_mut().refuse_change = Some(refusal);
            let outcome = patch(
                &mut host,
                "*** Begin Patch\n*** Update File: f.txt\n-a\n+b\n*** End Patch\n",
            );
            assert_eq!(outcome, error(&expected));
            let log = host.log();
            // The refusal ends the call: the gate is released and nothing follows it.
            assert_eq!(log.last().map(String::as_str), Some("release"));
            assert_eq!(
                log.iter()
                    .filter(|entry| entry.starts_with("write "))
                    .count(),
                1
            );
            assert!(host.0.borrow().observed.is_empty());
        }

        // An addition whose path an ungated writer filled after the plan: the native
        // planning text, not an overwrite.
        let mut host = Fake::default();
        host.0.borrow_mut().refuse_change = Some(FsError::AlreadyExists);
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Add File: n.txt\n+x\n*** End Patch\n",
        );
        assert_eq!(outcome, error("n.txt already exists."));

        let mut host = Fake::default();
        host.0.borrow_mut().refuse_change = Some(FsError::Cancelled);
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Add File: n.txt\n+x\n*** End Patch\n",
        );
        assert_eq!(outcome.status, Status::Cancelled);
    }

    #[test]
    fn a_wrong_kind_refusal_gets_the_native_operating_system_text() {
        // A file where the new file's parent directory should be: the native
        // `create_dir_all` reports `EEXIST`.
        let mut host = Fake::default().with_file("d.txt", "x\n");
        host.0.borrow_mut().refuse_change = Some(FsError::WrongKind);
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Add File: d.txt/x\n+a\n*** End Patch\n",
        );
        assert_eq!(
            outcome,
            error("failed to write d.txt/x: File exists (os error 17)")
        );

        let mut host = Fake::default().with_file("gone.txt", "x\n");
        host.0.borrow_mut().refuse_change = Some(FsError::WrongKind);
        let outcome = patch(
            &mut host,
            "*** Begin Patch\n*** Delete File: gone.txt\n*** End Patch\n",
        );
        assert_eq!(
            outcome,
            error("failed to delete gone.txt: Not a directory (os error 20)")
        );
    }

    #[test]
    fn invalid_input_is_refused_before_any_capability_is_used() {
        let mut host = Fake::default();
        let json = execute(&mut host, "apply_patch", true, RawInput::Json("{}"));
        assert_eq!(
            json,
            error("Invalid input for apply_patch: expected freeform text input, got a JSON object")
        );
        let text = execute(&mut host, "Patch", false, RawInput::Text("x"));
        assert_eq!(
            text,
            error("Invalid input for Patch: expected a JSON object input, got freeform text")
        );
        for raw in ["", "null", "[]", "{}", "{\"patch\": 5}"] {
            let outcome = execute(&mut host, "apply_patch", false, RawInput::Json(raw));
            assert!(
                outcome
                    .content
                    .starts_with("Invalid input for apply_patch: "),
                "{raw:?} -> {outcome:?}"
            );
        }
        let garbage = patch(&mut host, "*** Begin Patch\n*** End Patch\n");
        assert_eq!(
            garbage,
            error("Invalid patch: the patch contains no hunks (line 1).")
        );
        assert!(host.log().iter().all(|entry| entry == "cancelled"));
    }

    #[test]
    fn the_function_form_applies_the_same_patch() {
        let mut host = Fake::default().with_file("f.txt", "a\nb\n");
        let input = serde_json::json!({
            "patch": "*** Begin Patch\n*** Update File: f.txt\n-b\n+B\n*** End Patch\n"
        })
        .to_string();
        let outcome = execute(&mut host, "apply_patch", false, RawInput::Json(&input));
        assert_eq!(outcome.content, "M f.txt");
        assert_eq!(host.file("f.txt").as_deref(), Some("a\nB\n"));
    }

    #[test]
    fn long_output_is_bounded() {
        let mut text = String::from("*** Begin Patch\n");
        for index in 0..2_100 {
            text.push_str(&format!("*** Add File: f{index}\n+x\n"));
        }
        text.push_str("*** End Patch\n");
        let mut host = Fake::default();
        let outcome = patch(&mut host, &text);
        assert_eq!(outcome.status, Status::Ok);
        assert!(
            outcome.content.ends_with(" bytes]") && outcome.content.lines().count() == 2_001,
            "{}",
            &outcome.content[outcome.content.len() - 60..]
        );
    }

    #[test]
    fn normalization_keeps_what_only_the_host_can_place() {
        assert_eq!(normalized_parts("/a/../../b", true), ["b"]);
        assert_eq!(normalized_parts("../ws/./x", false), ["..", "ws", "x"]);
    }
}
