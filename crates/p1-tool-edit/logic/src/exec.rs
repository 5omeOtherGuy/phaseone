//! The `edit` call as a component runs it, over the capabilities its assembly links.
//!
//! [`execute`] is generic over [`Capabilities`] so the guest's order of operations is tested
//! natively (with an in-memory host here, and over the real `p1-workspace` service in
//! `p1-tool-edit`'s tests) and the component only adapts the WIT imports to this trait.
//!
//! The order is the point: the input is validated, the target is read through `workspace`
//! and checked through `snapshot`, and the change is computed, all before the write gate is
//! taken; only then does the call `begin` a mutation, write, and let it go. The host rechecks
//! the target under the gate (docs/design/modules/workspace-mutation.md), so a concurrent
//! writer is refused as stale instead of being overwritten. Nothing here spawns a thread or
//! blocks one: every wait is a host import the guest is suspended in.

use crate::{
    EditInput, changed_since_observed, could_not_read, does_not_exist, edit_text, edited_output,
    escapes_workspace, failed_to_write, never_observed, parse_json_input, text_input_error,
};

/// How many bytes one `workspace.read` asks for. The file is read window by window so a
/// large file never needs one huge host allocation, and cancellation is checked in between.
pub const READ_WINDOW: u64 = 1 << 20;

/// Why a workspace operation failed: the WIT `fs-error`, mirrored so this crate stays
/// target-independent.
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

/// The WIT `entry-kind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EntryKind {
    File,
    Directory,
    Other,
}

/// The WIT `entry` of `workspace.stat`: `path` is relative to the root, `/`-separated.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub path: String,
    pub kind: EntryKind,
}

/// The WIT `snapshot.observation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    NeverObserved,
    Unchanged,
    ChangedSinceObserved,
}

/// A held write gate: the WIT `workspace-mutation.mutation` resource. Dropping it releases
/// the gate.
pub trait Mutation {
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError>;
}

/// The imports an `edit` call uses: `control`, `workspace`, `snapshot` and
/// `workspace-mutation`.
pub trait Capabilities {
    type Mutation: Mutation;
    fn cancelled(&self) -> bool;
    fn stat(&self, path: &str) -> Result<Entry, FsError>;
    fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError>;
    fn check(&self, path: &str, current: &[u8]) -> Result<Observation, FsError>;
    fn observe(&self, path: &str, contents: &[u8]) -> Result<(), FsError>;
    fn begin(&self) -> Self::Mutation;
}

/// The input of a tool call, as the wire carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallInput<'a> {
    Json(&'a str),
    Text(&'a str),
}

/// How an `edit` call ended; `content` is exactly what the model is shown.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Ok(String),
    Error(String),
    /// Empty content, as the native tools return it.
    Cancelled,
}

/// Run one `edit` call; `tool` is the name the model called it by, for input errors.
pub fn execute<C: Capabilities>(caps: &C, tool: &str, input: CallInput<'_>) -> Outcome {
    // Cancellation before any work: touch nothing, not even a stat.
    if caps.cancelled() {
        return Outcome::Cancelled;
    }
    let input = match input {
        CallInput::Json(raw) => match parse_json_input(tool, raw) {
            Ok(input) => input,
            Err(message) => return Outcome::Error(message),
        },
        CallInput::Text(_) => return Outcome::Error(text_input_error(tool)),
    };
    // A no-op edit succeeds without touching the file or its read state.
    if crate::is_no_change(&input) {
        return Outcome::Ok(crate::no_change(&input));
    }
    match run(caps, &input) {
        Ok(content) => Outcome::Ok(content),
        Err(Stop::Cancelled) => Outcome::Cancelled,
        Err(Stop::Error(message)) => Outcome::Error(message),
    }
}

enum Stop {
    Cancelled,
    Error(String),
}

fn run<C: Capabilities>(caps: &C, input: &EditInput) -> Result<String, Stop> {
    let requested = input.file_path.as_str();
    let display = match caps.stat(requested) {
        Ok(entry) => entry.path,
        Err(FsError::NotFound) => {
            return Err(Stop::Error(does_not_exist(&missing_display(
                caps, requested,
            ))));
        }
        Err(error) => return Err(resolve_error(caps, error, requested)),
    };

    let bytes = read_whole(caps, requested, &display)?;

    // Read-before-mutate, in the native order: an unread or stale file is refused before
    // its contents are matched. The host checks again under the gate.
    match caps.check(requested, &bytes) {
        Ok(Observation::Unchanged) => {}
        Ok(Observation::NeverObserved) => return Err(Stop::Error(never_observed(&display))),
        Ok(Observation::ChangedSinceObserved) => {
            return Err(Stop::Error(changed_since_observed(&display)));
        }
        Err(error) => return Err(read_error(error, requested, &display)),
    }

    let edited = edit_text(&display, &bytes, input).map_err(Stop::Error)?;
    if caps.cancelled() {
        return Err(Stop::Cancelled);
    }

    // The gate is held only from here: the host's recheck, the write and the observation.
    let mutation = caps.begin();
    mutation
        .write(requested, edited.contents.as_bytes())
        .map_err(|error| write_error(error, requested, &display))?;
    // The host records a successful write as this agent's observation already; recording it
    // here too keeps consecutive edits free of a re-read whatever the host does. The file is
    // written either way, so a failure to record must not turn the call into an error.
    let _ = caps.observe(requested, edited.contents.as_bytes());
    drop(mutation);

    Ok(edited_output(
        &display,
        edited.replacements,
        edited.applied_region.as_deref(),
    ))
}

/// The whole file, window by window, with a cancellation check between windows.
fn read_whole<C: Capabilities>(caps: &C, requested: &str, display: &str) -> Result<Vec<u8>, Stop> {
    let mut bytes = Vec::new();
    loop {
        let window = caps
            .read(requested, bytes.len() as u64, READ_WINDOW)
            .map_err(|error| read_error(error, requested, display))?;
        let short = (window.len() as u64) < READ_WINDOW;
        bytes.extend_from_slice(&window);
        if short {
            return Ok(bytes);
        }
        if caps.cancelled() {
            return Err(Stop::Cancelled);
        }
    }
}

/// How the native tool shows a path that does not exist: its root-relative form.
///
/// A relative request inside the root displays as its lexical normalization. An absolute one
/// cannot be stripped of a root the guest never sees, so the nearest existing ancestor is
/// asked for its root-relative path and the rest is appended; when no ancestor answers, the
/// request is shown as given.
fn missing_display<C: Capabilities>(caps: &C, requested: &str) -> String {
    if !requested.starts_with('/') {
        return crate::lexical_normalize(requested);
    }
    let normalized = crate::lexical_normalize(requested);
    let components: Vec<&str> = normalized.split('/').collect();
    for keep in (0..components.len()).rev() {
        let ancestor = format!("/{}", components[..keep].join("/"));
        if let Ok(entry) = caps.stat(&ancestor) {
            let rest = components[keep..].join("/");
            return match entry.path.as_str() {
                "" | "." => rest,
                base => format!("{base}/{rest}"),
            };
        }
    }
    requested.to_string()
}

/// A failure while resolving the target, before its display form is known.
///
/// The native tool stats nothing: it resolves the path and reads it, so what it shows is its
/// own read's message. The path is therefore shown in its root-relative form
/// ([`missing_display`]) and the host's `io` message — the io error's own text, per
/// `docs/design/modules/workspace-mutation.md` — is wrapped exactly as the native read's is.
fn resolve_error<C: Capabilities>(caps: &C, error: FsError, requested: &str) -> Stop {
    match error {
        FsError::Cancelled => Stop::Cancelled,
        FsError::OutsideWorkspace => Stop::Error(escapes_workspace(requested)),
        FsError::InvalidPattern(message) => Stop::Error(message),
        other => Stop::Error(could_not_read(
            &missing_display(caps, requested),
            unresolved_error_text(&other),
        )),
    }
}

/// A failure while reading or checking the target shown as `display`.
fn read_error(error: FsError, requested: &str, display: &str) -> Stop {
    match error {
        FsError::Cancelled => Stop::Cancelled,
        FsError::OutsideWorkspace => Stop::Error(escapes_workspace(requested)),
        FsError::NotFound => Stop::Error(does_not_exist(display)),
        FsError::Io(message) => Stop::Error(could_not_read(display, &message)),
        other => Stop::Error(could_not_read(display, fs_error_text(&other))),
    }
}

/// A failure of the write under the gate. The host's `io` message is already model-facing
/// and worded as the native tool words it (the stale refusals, "failed to write …").
fn write_error(error: FsError, requested: &str, display: &str) -> Stop {
    match error {
        FsError::Cancelled => Stop::Cancelled,
        FsError::OutsideWorkspace => Stop::Error(escapes_workspace(requested)),
        FsError::NotFound => Stop::Error(does_not_exist(display)),
        FsError::Io(message) => Stop::Error(message),
        other => Stop::Error(failed_to_write(display, fs_error_text(&other))),
    }
}

/// A short text for an `fs-error` case that carries no message of its own, for a target the
/// host stat'd.
///
/// A `wrong-kind` here is a directory where the call needed a file: the host's `read` reports
/// `wrong-kind` only for a directory (`Workspace::read`'s own `NotADirectory` check), and the
/// native tool prints `std::fs::read`'s `EISDIR` for it on the Unix hosts p1 runs on.
fn fs_error_text(error: &FsError) -> &str {
    match error {
        FsError::OutsideWorkspace => "the path escapes the workspace",
        FsError::NotFound => "no such file",
        FsError::WrongKind => "Is a directory (os error 21)",
        FsError::AlreadyExists => "already exists",
        FsError::Cancelled => "cancelled",
        FsError::InvalidPattern(message) | FsError::Io(message) => message,
    }
}

/// As [`fs_error_text`], for a path the host could not stat at all.
///
/// `wrong-kind` covers the two native errors the frozen WIT folds together, and what decides
/// which one this is, is the stat'd entry's kind: a directory the call read is
/// [`fs_error_text`]'s `EISDIR`, while a file used as a directory component (target
/// `d.txt/x`, which `std::fs::read` reports as `ENOTDIR`) is this case — nothing was stat'd,
/// so the path's component is not a directory.
fn unresolved_error_text(error: &FsError) -> &str {
    match error {
        FsError::WrongKind => "Not a directory (os error 20)",
        other => fs_error_text(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// The in-memory host's state: files by root-relative path, this agent's observations,
    /// and a log of every import call in order, so a test sees where the gate was taken.
    #[derive(Default)]
    struct State {
        files: BTreeMap<String, Vec<u8>>,
        observed: BTreeMap<String, Vec<u8>>,
        log: Vec<String>,
        /// How many `cancelled()` calls answer false before every later one answers true.
        cancel_after: Option<usize>,
        /// What the next stat is refused with, as the host's failure to resolve would.
        refuse_stat: Option<FsError>,
        /// What the next write under the gate is refused with, as the host's recheck would.
        refuse_write: Option<FsError>,
        held: bool,
    }

    /// A workspace rooted at `/root` with one directory, `dir`, besides the root.
    #[derive(Clone, Default)]
    struct Host(Rc<RefCell<State>>);

    struct Held(Rc<RefCell<State>>);

    impl Host {
        fn with(files: &[(&str, &[u8])]) -> Self {
            let host = Self::default();
            for (path, contents) in files {
                host.0
                    .borrow_mut()
                    .files
                    .insert((*path).to_string(), contents.to_vec());
            }
            host
        }

        /// A prior read of `path` by this agent.
        fn observe_now(&self, path: &str) {
            let mut state = self.0.borrow_mut();
            let contents = state.files[path].clone();
            state.observed.insert(path.to_string(), contents);
        }

        fn cancel_after(&self, calls: usize) {
            self.0.borrow_mut().cancel_after = Some(calls);
        }

        fn refuse_next_write(&self, error: FsError) {
            self.0.borrow_mut().refuse_write = Some(error);
        }

        /// The next `stat` fails as the host's resolution of a file used as a directory
        /// component does.
        fn refuse_next_stat(&self, error: FsError) {
            self.0.borrow_mut().refuse_stat = Some(error);
        }

        fn log(&self, entry: String) {
            self.0.borrow_mut().log.push(entry);
        }

        fn calls(&self) -> Vec<String> {
            self.0.borrow().log.clone()
        }

        fn file(&self, path: &str) -> Vec<u8> {
            self.0.borrow().files[path].clone()
        }

        /// Confinement as the host applies it, lexically: this host has no symlinks.
        fn key(path: &str) -> Result<String, FsError> {
            let relative = if path.starts_with('/') {
                match path.strip_prefix("/root") {
                    Some(rest) if rest.is_empty() || rest.starts_with('/') => {
                        rest.trim_start_matches('/')
                    }
                    _ => return Err(FsError::OutsideWorkspace),
                }
            } else {
                path
            };
            if crate::escapes_lexically(relative) {
                return Err(FsError::OutsideWorkspace);
            }
            Ok(crate::lexical_normalize(relative))
        }
    }

    impl Drop for Held {
        fn drop(&mut self) {
            let mut state = self.0.borrow_mut();
            state.held = false;
            state.log.push("release".into());
        }
    }

    impl Mutation for Held {
        fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
            let mut state = self.0.borrow_mut();
            state.log.push(format!("write {path}"));
            assert!(state.held, "a write outside the gate");
            if let Some(error) = state.refuse_write.take() {
                return Err(error);
            }
            let key = Host::key(path)?;
            state.files.insert(key.clone(), contents.to_vec());
            state.observed.insert(key, contents.to_vec());
            Ok(())
        }
    }

    impl Capabilities for Host {
        type Mutation = Held;

        fn cancelled(&self) -> bool {
            let mut state = self.0.borrow_mut();
            match state.cancel_after.as_mut() {
                Some(0) => true,
                Some(remaining) => {
                    *remaining -= 1;
                    false
                }
                None => false,
            }
        }

        fn stat(&self, path: &str) -> Result<Entry, FsError> {
            self.log(format!("stat {path}"));
            if let Some(error) = self.0.borrow_mut().refuse_stat.take() {
                return Err(error);
            }
            let key = Self::key(path)?;
            if key.is_empty() || key == "dir" {
                return Ok(Entry {
                    path: key,
                    kind: EntryKind::Directory,
                });
            }
            if self.0.borrow().files.contains_key(&key) {
                Ok(Entry {
                    path: key,
                    kind: EntryKind::File,
                })
            } else {
                Err(FsError::NotFound)
            }
        }

        fn read(&self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError> {
            self.log(format!("read {path} {offset}"));
            let key = Self::key(path)?;
            if key.is_empty() || key == "dir" {
                return Err(FsError::WrongKind);
            }
            let state = self.0.borrow();
            let bytes = state.files.get(&key).ok_or(FsError::NotFound)?;
            let start = usize::try_from(offset).unwrap().min(bytes.len());
            let end = start
                .saturating_add(usize::try_from(length).unwrap())
                .min(bytes.len());
            Ok(bytes[start..end].to_vec())
        }

        fn check(&self, path: &str, current: &[u8]) -> Result<Observation, FsError> {
            self.log(format!("check {path}"));
            let key = Self::key(path)?;
            Ok(match self.0.borrow().observed.get(&key) {
                None => Observation::NeverObserved,
                Some(seen) if seen == current => Observation::Unchanged,
                Some(_) => Observation::ChangedSinceObserved,
            })
        }

        fn observe(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
            self.log(format!("observe {path}"));
            let key = Self::key(path)?;
            self.0.borrow_mut().observed.insert(key, contents.to_vec());
            Ok(())
        }

        fn begin(&self) -> Held {
            let mut state = self.0.borrow_mut();
            assert!(!state.held, "a second begin while the gate is held");
            state.held = true;
            state.log.push("begin".into());
            Held(self.0.clone())
        }
    }

    fn edit(host: &Host, json: &str) -> Outcome {
        execute(host, "edit", CallInput::Json(json))
    }

    fn strings(entries: &[&str]) -> Vec<String> {
        entries.iter().map(|entry| (*entry).to_string()).collect()
    }

    #[test]
    fn validates_and_computes_before_the_gate_and_writes_under_it() {
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");

        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#,
        );

        assert_eq!(outcome, Outcome::Ok("Edited d.txt (1 replacement).".into()));
        assert_eq!(host.file("d.txt"), b"one\nTWO\n");
        assert_eq!(
            host.calls(),
            strings(&[
                "stat d.txt",
                "read d.txt 0",
                "check d.txt",
                "begin",
                "write d.txt",
                "observe d.txt",
                "release",
            ])
        );
        // The write became the observation, so a second edit needs no re-read.
        let second = edit(
            &host,
            r#"{"file_path": "d.txt", "old_string": "one", "new_string": "ONE"}"#,
        );
        assert_eq!(second, Outcome::Ok("Edited d.txt (1 replacement).".into()));
        assert_eq!(host.file("d.txt"), b"ONE\nTWO\n");
    }

    /// #706: a list is one read, one check and one write under the gate; a failing entry
    /// writes nothing and never takes the gate.
    #[test]
    fn a_list_is_written_once_or_not_at_all() {
        let host = Host::with(&[("d.txt", b"one\ntwo\nthree\n")]);
        host.observe_now("d.txt");

        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "edits": [{"old_string": "three", "new_string": "3"}, {"old_string": "one", "new_string": "1"}]}"#,
        );

        assert_eq!(
            outcome,
            Outcome::Ok("Edited d.txt (2 replacements).".into())
        );
        assert_eq!(host.file("d.txt"), b"1\ntwo\n3\n");
        assert_eq!(
            host.calls(),
            strings(&[
                "stat d.txt",
                "read d.txt 0",
                "check d.txt",
                "begin",
                "write d.txt",
                "observe d.txt",
                "release",
            ])
        );

        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");
        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "edits": [{"old_string": "one", "new_string": "1"}, {"old_string": "absent", "new_string": "x"}]}"#,
        );
        assert_eq!(
            outcome,
            Outcome::Error(
                "edits[1] failed; no edit was applied. old_string was not found in d.txt.".into()
            )
        );
        assert_eq!(host.file("d.txt"), b"one\ntwo\n");
        assert!(!host.calls().contains(&"begin".to_string()));
    }

    #[test]
    fn a_never_read_target_is_refused_before_matching_and_without_the_gate() {
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);

        // Even an old_string that does not occur reports the missing read first, as native.
        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "old_string": "absent", "new_string": "x"}"#,
        );

        assert_eq!(
            outcome,
            Outcome::Error("You must read d.txt before changing it.".into())
        );
        assert_eq!(host.file("d.txt"), b"one\ntwo\n");
        assert!(!host.calls().contains(&"begin".to_string()));
    }

    #[test]
    fn a_target_changed_since_it_was_read_is_refused() {
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");
        host.0
            .borrow_mut()
            .files
            .insert("d.txt".into(), b"one\ntwo\nthree\n".to_vec());

        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#,
        );

        assert_eq!(
            outcome,
            Outcome::Error("d.txt changed on disk since you last read it; read it again.".into())
        );
        assert!(!host.calls().contains(&"begin".to_string()));
    }

    #[test]
    fn the_hosts_refusal_under_the_gate_is_shown_verbatim_and_the_gate_released() {
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");
        let stale = "d.txt changed on disk since you last read it; read it again.";
        host.refuse_next_write(FsError::Io(stale.into()));

        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#,
        );

        assert_eq!(outcome, Outcome::Error(stale.into()));
        assert_eq!(host.file("d.txt"), b"one\ntwo\n");
        assert_eq!(host.calls().last().map(String::as_str), Some("release"));
        assert!(!host.0.borrow().held);
    }

    /// #458 unit 3: identical `old_string` and `new_string` is an Ok no-op; the file is not
    /// touched and no read-state is required.
    #[test]
    fn identical_strings_are_a_no_op_without_touching_the_file() {
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        // No `observe_now`: the no-op must not require the file to have been read.
        let outcome = edit(
            &host,
            r#"{"file_path": "d.txt", "old_string": "two", "new_string": "two"}"#,
        );
        assert_eq!(
            outcome,
            Outcome::Ok(
                "No change: old_string and new_string are identical; d.txt was not modified."
                    .into()
            )
        );
        assert_eq!(host.file("d.txt"), b"one\ntwo\n");
        assert!(host.calls().is_empty());
    }

    #[test]
    fn match_errors_are_the_native_texts() {
        let host = Host::with(&[("e.txt", b"dup\ndup\n"), ("g.txt", b"alpha\n")]);
        host.observe_now("e.txt");
        host.observe_now("g.txt");
        assert_eq!(
            edit(
                &host,
                r#"{"file_path": "e.txt", "old_string": "dup", "new_string": "x"}"#
            ),
            Outcome::Error(
                "old_string occurs 2 times in e.txt; add context to make it unique or set replace_all."
                    .into()
            )
        );
        assert_eq!(
            edit(
                &host,
                r#"{"file_path": "g.txt", "old_string": "beta", "new_string": "x"}"#
            ),
            Outcome::Error("old_string was not found in g.txt.".into())
        );
        assert_eq!(
            edit(
                &host,
                r#"{"file_path": "e.txt", "old_string": "dup", "new_string": "x", "replace_all": true}"#
            ),
            Outcome::Ok("Edited e.txt (2 replacements).".into())
        );
        assert_eq!(host.file("e.txt"), b"x\nx\n");
    }

    #[test]
    fn line_endings_and_the_bom_are_kept() {
        let host = Host::with(&[
            ("crlf.txt", b"one\r\ntwo\r\n"),
            ("bom.txt", "\u{FEFF}alpha\n".as_bytes()),
            ("bare.txt", b"one\ntwo"),
        ]);
        for path in ["crlf.txt", "bom.txt", "bare.txt"] {
            host.observe_now(path);
        }
        edit(
            &host,
            r#"{"file_path": "crlf.txt", "old_string": "two", "new_string": "TWO"}"#,
        );
        edit(
            &host,
            r#"{"file_path": "bom.txt", "old_string": "alpha", "new_string": "beta"}"#,
        );
        edit(
            &host,
            r#"{"file_path": "bare.txt", "old_string": "two", "new_string": "TWO"}"#,
        );
        assert_eq!(host.file("crlf.txt"), b"one\r\nTWO\r\n");
        assert_eq!(host.file("bom.txt"), "\u{FEFF}beta\n".as_bytes());
        assert_eq!(host.file("bare.txt"), b"one\nTWO");
    }

    #[test]
    fn missing_escaping_and_directory_targets() {
        let host = Host::with(&[]);
        let json = |path: &str| {
            serde_json::json!({"file_path": path, "old_string": "a", "new_string": "b"}).to_string()
        };
        assert_eq!(
            edit(&host, &json("./sub/../d.txt")),
            Outcome::Error("d.txt does not exist.".into())
        );
        assert_eq!(
            edit(&host, &json("/root/dir/new.txt")),
            Outcome::Error("dir/new.txt does not exist.".into())
        );
        assert_eq!(
            edit(&host, &json("/root/a/b.txt")),
            Outcome::Error("a/b.txt does not exist.".into())
        );
        assert_eq!(
            edit(&host, &json("../victim.txt")),
            Outcome::Error("path escapes workspace: ../victim.txt".into())
        );
        assert_eq!(
            edit(&host, &json("/etc/passwd")),
            Outcome::Error("path escapes workspace: /etc/passwd".into())
        );
        assert_eq!(
            edit(&host, &json("dir")),
            Outcome::Error("dir could not be read: Is a directory (os error 21)".into())
        );
        assert!(!host.calls().contains(&"begin".to_string()));
    }

    /// A target whose component is a file. The native tool reads `d.txt/x` and reports
    /// `Not a directory`; the guest must not print the directory text (`EISDIR`) the shared
    /// `wrong-kind` variant is otherwise for, nor the host's raw io message.
    #[test]
    fn a_path_through_a_file_could_not_be_read_as_a_directory() {
        let json = r#"{"file_path": "d.txt/x", "old_string": "a", "new_string": "b"}"#;

        // What `Workspace::stat` of `d.txt/x` fails with: the io error's own text.
        let host = Host::with(&[("d.txt", b"one\n")]);
        host.refuse_next_stat(FsError::Io("Not a directory (os error 20)".into()));
        assert_eq!(
            edit(&host, json),
            Outcome::Error("d.txt/x could not be read: Not a directory (os error 20)".into())
        );
        assert!(!host.calls().contains(&"begin".to_string()));

        // A host that folds the same failure into `wrong-kind` renders the same text: nothing
        // was stat'd, so the entry's kind is not the directory kind.
        let host = Host::with(&[("d.txt", b"one\n")]);
        host.refuse_next_stat(FsError::WrongKind);
        assert_eq!(
            edit(&host, json),
            Outcome::Error("d.txt/x could not be read: Not a directory (os error 20)".into())
        );

        // An absolute path is shown root-relative, as the native display is.
        let host = Host::with(&[("d.txt", b"one\n")]);
        host.refuse_next_stat(FsError::Io("Not a directory (os error 20)".into()));
        assert_eq!(
            edit(
                &host,
                r#"{"file_path": "/root/d.txt/x", "old_string": "a", "new_string": "b"}"#
            ),
            Outcome::Error("d.txt/x could not be read: Not a directory (os error 20)".into())
        );
    }

    #[test]
    fn a_large_file_is_read_window_by_window() {
        let window = usize::try_from(READ_WINDOW).unwrap();
        let mut contents = vec![b'x'; window * 2];
        contents.extend_from_slice(b"\nneedle\n");
        let host = Host::with(&[("big.txt", &contents)]);
        host.observe_now("big.txt");

        let outcome = edit(
            &host,
            r#"{"file_path": "big.txt", "old_string": "needle", "new_string": "pin"}"#,
        );

        assert_eq!(
            outcome,
            Outcome::Ok("Edited big.txt (1 replacement).".into())
        );
        let reads: Vec<String> = host
            .calls()
            .into_iter()
            .filter(|call| call.starts_with("read"))
            .collect();
        assert_eq!(
            reads,
            vec![
                "read big.txt 0".to_string(),
                format!("read big.txt {window}"),
                format!("read big.txt {}", window * 2),
            ]
        );
        assert!(host.file("big.txt").ends_with(b"\npin\n"));
    }

    #[test]
    fn cancellation_is_checked_before_work_between_windows_and_before_the_gate() {
        // Before any work: not even a stat.
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");
        host.cancel_after(0);
        let json = r#"{"file_path": "d.txt", "old_string": "two", "new_string": "TWO"}"#;
        assert_eq!(edit(&host, json), Outcome::Cancelled);
        assert!(host.calls().is_empty());

        // After the change is computed, before the gate.
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");
        host.cancel_after(1);
        assert_eq!(edit(&host, json), Outcome::Cancelled);
        assert!(!host.calls().contains(&"begin".to_string()));
        assert_eq!(host.file("d.txt"), b"one\ntwo\n");

        // Between two windows of a large file.
        let contents = vec![b'x'; usize::try_from(READ_WINDOW).unwrap() + 1];
        let host = Host::with(&[("big.txt", &contents)]);
        host.observe_now("big.txt");
        host.cancel_after(1);
        let big = r#"{"file_path": "big.txt", "old_string": "x", "new_string": "y"}"#;
        assert_eq!(edit(&host, big), Outcome::Cancelled);
        assert_eq!(host.calls(), strings(&["stat big.txt", "read big.txt 0"]));

        // A host import that returned because of cancellation.
        let host = Host::with(&[("d.txt", b"one\ntwo\n")]);
        host.observe_now("d.txt");
        host.refuse_next_write(FsError::Cancelled);
        assert_eq!(edit(&host, json), Outcome::Cancelled);
    }

    #[test]
    fn invalid_input_touches_nothing() {
        let host = Host::with(&[]);
        assert_eq!(
            execute(&host, "edit", CallInput::Text("file_path=a")),
            Outcome::Error(
                "Invalid input for edit: expected a JSON object input, got freeform text".into()
            )
        );
        let Outcome::Error(message) = execute(&host, "EditFile", CallInput::Json("null")) else {
            panic!("null is not an edit input");
        };
        assert!(
            message.starts_with("Invalid input for EditFile: "),
            "{message}"
        );
        assert!(host.calls().is_empty());
    }
}
