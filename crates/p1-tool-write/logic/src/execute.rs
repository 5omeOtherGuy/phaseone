//! The `execute` flow of the `write` tool over an abstract [`Host`]: the component's
//! imported capabilities, or a fake in tests.
//!
//! The order is the point: the input is validated and the target read and checked
//! against this agent's observation **before** the write gate is taken, and the gate is
//! held only for the write and its recorded observation. The host rechecks the
//! observation under the gate (`workspace-mutation`, `MutationPolicy::Observed`), so a file
//! that changes between this flow's check and its write is still refused, with the same
//! text.

use crate::{
    RawInput, WriteInput, bounded, changed_since_observed, could_not_be_read, display_of,
    never_observed, parse_input, relative_display, wrote,
};

/// How many bytes one `workspace.read` asks for. A window, not a limit: an existing
/// target is read whole, window by window, checking cancellation between windows.
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

/// This agent's observation of a file; the WIT `snapshot.observation`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Observation {
    NeverObserved,
    Unchanged,
    ChangedSinceObserved,
}

/// How a call ended; the `write` tool reaches only these three `ToolStatus`es.
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

/// The capabilities the flow uses: `control`, `workspace`, `snapshot` and
/// `workspace-mutation`, one method per import.
pub trait Host {
    /// The held write gate `begin` returns.
    type Mutation: Mutation;

    /// `control.cancelled`.
    fn cancelled(&mut self) -> bool;
    /// `workspace.stat`.
    fn stat(&mut self, path: &str) -> Result<Entry, FsError>;
    /// `workspace.read`: up to `length` bytes from `offset`, fewer only at the end.
    fn read(&mut self, path: &str, offset: u64, length: u64) -> Result<Vec<u8>, FsError>;
    /// `snapshot.check`.
    fn check(&mut self, path: &str, current: &[u8]) -> Result<Observation, FsError>;
    /// `snapshot.observe`.
    fn observe(&mut self, path: &str, contents: &[u8]) -> Result<(), FsError>;
    /// `workspace-mutation.begin`: waits for the write gate. Dropping the value releases it.
    fn begin(&mut self) -> Self::Mutation;
}

/// The WIT `mutation` resource: the write gate, held.
pub trait Mutation {
    /// `mutation.write`: atomic replacement, missing parents created by the host.
    fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError>;
}

/// Why the flow stopped early.
enum Stop {
    Cancelled,
    Failed(String),
}

/// Run one `write` call named `tool` (the name the model called it by, which invalid
/// input names) over `host`.
pub fn execute<H: Host>(host: &mut H, tool: &str, input: RawInput<'_>) -> Outcome {
    // Cancellation before any work: touch nothing, not even a stat.
    if host.cancelled() {
        return Outcome::cancelled();
    }
    let input = match parse_input(tool, input) {
        Ok(input) => input,
        Err(message) => return Outcome::error(message),
    };
    match run(host, &input) {
        Ok(content) => Outcome::ok(bounded(&content)),
        Err(Stop::Cancelled) => Outcome::cancelled(),
        Err(Stop::Failed(message)) => Outcome::error(message),
    }
}

/// The target as found outside the gate: its display form and, when it exists, its
/// current contents.
struct Target {
    display: String,
    current: Option<Vec<u8>>,
}

fn run<H: Host>(host: &mut H, input: &WriteInput) -> Result<String, Stop> {
    let request = input.file_path.as_str();
    let target = locate(host, request)?;

    // Read-before-mutate applies only when the target already exists: creating a new
    // file is a blind create, which is allowed. Checked here, outside the gate, so a
    // refusal never takes the gate; the host checks again under it.
    if let Some(current) = &target.current {
        stop_if_cancelled(host)?;
        match host
            .check(request, current)
            .map_err(|error| failure(error, request))?
        {
            Observation::NeverObserved => {
                return Err(Stop::Failed(never_observed(&target.display)));
            }
            Observation::ChangedSinceObserved => {
                return Err(Stop::Failed(changed_since_observed(&target.display)));
            }
            Observation::Unchanged => {}
        }
    }

    // The last point where stopping leaves the workspace untouched.
    stop_if_cancelled(host)?;
    let mutation = host.begin();
    mutation
        .write(request, input.content.as_bytes())
        .map_err(|error| failure(error, request))?;
    // Recorded while the gate is still held, as the native tool records after writing, so
    // a follow-up edit or write needs no re-read. The host has recorded the same bytes
    // already; a failure here cannot undo the write, so it does not turn into an error.
    let _ = host.observe(request, input.content.as_bytes());
    drop(mutation);

    Ok(wrote(&target.display, input.content.len()))
}

/// Find the target: an existing file is read whole; a missing one gets the display form
/// the native tool would print.
fn locate<H: Host>(host: &mut H, request: &str) -> Result<Target, Stop> {
    match host.stat(request) {
        Ok(entry) => match read_all(host, request) {
            Ok(bytes) => Ok(Target {
                display: entry.path,
                current: Some(bytes),
            }),
            // A symlink whose target is absent is an entry but reads as missing; the native
            // tool's existence check follows links, so it is a new file there too.
            Err(FsError::NotFound) => Ok(Target {
                display: new_file_display(host, request)?,
                current: None,
            }),
            Err(FsError::Cancelled) => Err(Stop::Cancelled),
            Err(error) => Err(Stop::Failed(could_not_be_read(
                &entry.path,
                &read_failure(&error, request, entry.kind),
            ))),
        },
        Err(FsError::NotFound) => Ok(Target {
            display: new_file_display(host, request)?,
            current: None,
        }),
        Err(error) => Err(failure(error, request)),
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
fn new_file_display<H: Host>(host: &mut H, request: &str) -> Result<String, Stop> {
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
            Err(FsError::Cancelled) => return Err(Stop::Cancelled),
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

fn stop_if_cancelled<H: Host>(host: &mut H) -> Result<(), Stop> {
    if host.cancelled() {
        Err(Stop::Cancelled)
    } else {
        Ok(())
    }
}

fn failure(error: FsError, request: &str) -> Stop {
    match error {
        FsError::Cancelled => Stop::Cancelled,
        other => Stop::Failed(fs_error_text(&other, request)),
    }
}

/// The model-facing text of a host refusal: the native `WorkspaceError` and
/// `MutationError` texts for the cases without a message, the host's own message
/// otherwise.
fn fs_error_text(error: &FsError, request: &str) -> String {
    match error {
        FsError::OutsideWorkspace => format!("path escapes workspace: {request}"),
        FsError::NotFound => format!("no such path in the workspace: {request}"),
        FsError::WrongKind => format!("not a file: {request}"),
        FsError::AlreadyExists => format!("already exists: {request}"),
        FsError::InvalidPattern(message) | FsError::Io(message) => message.clone(),
        FsError::Cancelled => "cancelled".to_string(),
    }
}

/// Why an existing target could not be read, as the native tool's `std::fs::read` error
/// reads.
///
/// The host reports a wrong kind without a message, so this fills the text in: a
/// `directory` is the operating system's `Is a directory (os error 21)`, which is what the
/// native `std::fs::read` prints for it. A `wrong-kind` entry that is not a directory (a
/// fifo, a socket, a device) makes `std::fs::read` fail with a different operating system
/// error this side cannot know, so it keeps the typed `not a file` text rather than
/// claiming to be a directory.
fn read_failure(error: &FsError, request: &str, kind: EntryKind) -> String {
    match (error, kind) {
        (FsError::WrongKind, EntryKind::Directory) => "Is a directory (os error 21)".to_string(),
        (other, _) => fs_error_text(other, request),
    }
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
        /// Entries that exist but are neither a file nor a directory (a fifo, a socket,
        /// a device): `stat` reports `Other` and `read` refuses with `WrongKind`.
        others: Vec<String>,
        observed: BTreeMap<String, Vec<u8>>,
        gate_held: bool,
        log: Vec<String>,
        cancel_after: Option<usize>,
        write_error: Option<FsError>,
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
        let absolute = request.starts_with('/');
        let joined = if absolute {
            request.to_string()
        } else {
            format!("{ROOT}/{request}")
        };
        let parts = normalized_parts(&joined, true);
        let full = format!("/{}", parts.join("/"));
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

        fn observed(self, path: &str, contents: &str) -> Self {
            self.0
                .borrow_mut()
                .observed
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
            let state = self.0.borrow();
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            if resolved.is_empty() || state.dirs.contains(&resolved) {
                return Ok(Entry {
                    path: resolved,
                    kind: EntryKind::Directory,
                    size: 0,
                });
            }
            if state.others.contains(&resolved) {
                return Ok(Entry {
                    path: resolved,
                    kind: EntryKind::Other,
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
            self.push(format!("read {path} {offset}"));
            let state = self.0.borrow();
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            if resolved.is_empty() || state.dirs.contains(&resolved) {
                return Err(FsError::WrongKind);
            }
            if state.others.contains(&resolved) {
                return Err(FsError::WrongKind);
            }
            let bytes = state.files.get(&resolved).ok_or(FsError::NotFound)?;
            let start = (offset as usize).min(bytes.len());
            let end = start.saturating_add(length as usize).min(bytes.len());
            Ok(bytes[start..end].to_vec())
        }

        fn check(&mut self, path: &str, current: &[u8]) -> Result<Observation, FsError> {
            self.push(format!("check {path}"));
            let state = self.0.borrow();
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            Ok(match state.observed.get(&resolved) {
                None => Observation::NeverObserved,
                Some(seen) if seen == current => Observation::Unchanged,
                Some(_) => Observation::ChangedSinceObserved,
            })
        }

        fn observe(&mut self, path: &str, contents: &[u8]) -> Result<(), FsError> {
            let held = self.0.borrow().gate_held;
            self.push(format!("observe {path} gate={held}"));
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            self.0
                .borrow_mut()
                .observed
                .insert(resolved, contents.to_vec());
            Ok(())
        }

        fn begin(&mut self) -> FakeMutation {
            self.push("begin".into());
            self.0.borrow_mut().gate_held = true;
            FakeMutation(self.0.clone())
        }
    }

    impl Mutation for FakeMutation {
        fn write(&self, path: &str, contents: &[u8]) -> Result<(), FsError> {
            let mut state = self.0.borrow_mut();
            state.log.push(format!("write {path}"));
            if let Some(error) = state.write_error.clone() {
                return Err(error);
            }
            let resolved = resolve(path).ok_or(FsError::OutsideWorkspace)?;
            state.files.insert(resolved, contents.to_vec());
            Ok(())
        }
    }

    impl Drop for FakeMutation {
        fn drop(&mut self) {
            let mut state = self.0.borrow_mut();
            state.gate_held = false;
            state.log.push("release".into());
        }
    }

    fn run_write(host: &mut Fake, arguments: &str) -> Outcome {
        execute(host, "write", RawInput::Json(arguments))
    }

    fn error(content: &str) -> Outcome {
        Outcome {
            status: Status::Error,
            content: content.into(),
        }
    }

    #[test]
    fn a_new_file_is_written_without_a_prior_read_and_recorded() {
        let mut host = Fake::default();
        let outcome = run_write(
            &mut host,
            r#"{"file_path": "nested/dir/file.txt", "content": "hello"}"#,
        );
        assert_eq!(
            outcome,
            Outcome {
                status: Status::Ok,
                content: "Wrote nested/dir/file.txt (5 bytes).".into()
            }
        );
        assert_eq!(host.file("nested/dir/file.txt").as_deref(), Some("hello"));
        // Validation and the stat come before the gate; the observation is recorded
        // while the gate is held; nothing is checked for a blind create.
        assert_eq!(
            host.log(),
            [
                "cancelled",
                "stat nested/dir/file.txt",
                "cancelled",
                "begin",
                "write nested/dir/file.txt",
                "observe nested/dir/file.txt gate=true",
                "release",
            ]
        );
    }

    #[test]
    fn an_existing_file_is_read_and_checked_before_the_gate() {
        let mut host = Fake::default()
            .with_file("out.txt", "old")
            .observed("out.txt", "old");
        let outcome = run_write(&mut host, r#"{"file_path": "out.txt", "content": "new"}"#);
        assert_eq!(outcome.status, Status::Ok);
        assert_eq!(outcome.content, "Wrote out.txt (3 bytes).");
        assert_eq!(host.file("out.txt").as_deref(), Some("new"));
        assert_eq!(
            host.log(),
            [
                "cancelled",
                "stat out.txt",
                "read out.txt 0",
                "cancelled",
                "check out.txt",
                "cancelled",
                "begin",
                "write out.txt",
                "observe out.txt gate=true",
                "release",
            ]
        );
        // The new contents are this agent's observation: a second write needs no re-read.
        let second = run_write(&mut host, r#"{"file_path": "out.txt", "content": "newer"}"#);
        assert_eq!(second.status, Status::Ok);
        assert_eq!(host.file("out.txt").as_deref(), Some("newer"));
    }

    #[test]
    fn an_unread_existing_file_is_refused_without_taking_the_gate() {
        let mut host = Fake::default().with_file("out.txt", "old");
        let outcome = run_write(&mut host, r#"{"file_path": "out.txt", "content": "new"}"#);
        assert_eq!(outcome, error("You must read out.txt before changing it."));
        assert_eq!(host.file("out.txt").as_deref(), Some("old"));
        assert!(!host.log().contains(&"begin".to_string()));
    }

    #[test]
    fn a_file_changed_since_it_was_read_is_refused() {
        let mut host = Fake::default()
            .with_file("out.txt", "externally changed")
            .observed("out.txt", "old");
        let outcome = run_write(&mut host, r#"{"file_path": "out.txt", "content": "new"}"#);
        assert_eq!(
            outcome,
            error("out.txt changed on disk since you last read it; read it again.")
        );
        assert_eq!(host.file("out.txt").as_deref(), Some("externally changed"));
        assert!(!host.log().contains(&"begin".to_string()));
    }

    #[test]
    fn a_large_file_is_read_whole_window_by_window() {
        let big = "x".repeat(READ_WINDOW as usize + 3);
        let mut host = Fake::default()
            .with_file("big.txt", &big)
            .observed("big.txt", &big);
        let outcome = run_write(&mut host, r#"{"file_path": "big.txt", "content": "y"}"#);
        assert_eq!(outcome.status, Status::Ok);
        let reads: Vec<String> = host
            .log()
            .into_iter()
            .filter(|entry| entry.starts_with("read "))
            .collect();
        assert_eq!(
            reads,
            ["read big.txt 0", &format!("read big.txt {READ_WINDOW}")]
        );
    }

    #[test]
    fn cancellation_is_checked_before_any_work_and_between_steps() {
        // Before any work: not even a stat.
        let mut host = Fake::default();
        host.0.borrow_mut().cancel_after = Some(0);
        let outcome = run_write(&mut host, r#"{"file_path": "fresh.txt", "content": "hi"}"#);
        assert_eq!(outcome.status, Status::Cancelled);
        assert_eq!(outcome.content, "");
        assert_eq!(host.log(), ["cancelled"]);

        // Between the windows of a large read.
        let big = "x".repeat(READ_WINDOW as usize + 3);
        let mut host = Fake::default()
            .with_file("big.txt", &big)
            .observed("big.txt", &big);
        host.0.borrow_mut().cancel_after = Some(1);
        let outcome = run_write(&mut host, r#"{"file_path": "big.txt", "content": "y"}"#);
        assert_eq!(outcome.status, Status::Cancelled);
        assert_eq!(host.file("big.txt").as_deref(), Some(big.as_str()));

        // After the check, before the gate: the workspace stays untouched.
        let mut host = Fake::default()
            .with_file("out.txt", "old")
            .observed("out.txt", "old");
        host.0.borrow_mut().cancel_after = Some(2);
        let outcome = run_write(&mut host, r#"{"file_path": "out.txt", "content": "new"}"#);
        assert_eq!(outcome.status, Status::Cancelled);
        assert_eq!(host.file("out.txt").as_deref(), Some("old"));
        assert!(!host.log().contains(&"begin".to_string()));
    }

    #[test]
    fn invalid_input_is_refused_before_any_capability_is_used() {
        let mut host = Fake::default();
        for arguments in ["", "null", "[]", "{\"file_path\": 5}"] {
            let outcome = run_write(&mut host, arguments);
            assert_eq!(outcome.status, Status::Error, "{arguments:?}");
            assert!(outcome.content.starts_with("Invalid input for write: "));
        }
        let text = execute(&mut host, "WriteFile", RawInput::Text("x"));
        assert_eq!(
            text,
            error("Invalid input for WriteFile: expected a JSON object input, got freeform text")
        );
        assert!(host.log().iter().all(|entry| entry == "cancelled"));
    }

    #[test]
    fn an_escaping_path_is_the_native_confinement_error() {
        let mut host = Fake::default();
        let outcome = run_write(&mut host, r#"{"file_path": "../evil.txt", "content": "x"}"#);
        assert_eq!(outcome, error("path escapes workspace: ../evil.txt"));
        let outcome = run_write(
            &mut host,
            r#"{"file_path": "/tmp/evil.txt", "content": "x"}"#,
        );
        assert_eq!(outcome, error("path escapes workspace: /tmp/evil.txt"));
        assert!(!host.log().contains(&"begin".to_string()));
    }

    #[test]
    fn an_absolute_or_climbing_new_path_displays_relative_to_the_root() {
        let mut host = Fake::default();
        let outcome = run_write(
            &mut host,
            r#"{"file_path": "/ws/a/./b.txt", "content": "x"}"#,
        );
        assert_eq!(outcome.content, "Wrote a/b.txt (1 bytes).");
        let outcome = run_write(&mut host, r#"{"file_path": "../ws/c.txt", "content": "x"}"#);
        assert_eq!(outcome.content, "Wrote c.txt (1 bytes).");
        assert_eq!(host.file("a/b.txt").as_deref(), Some("x"));
        assert_eq!(host.file("c.txt").as_deref(), Some("x"));
    }

    #[test]
    fn an_existing_file_displays_as_the_host_resolves_it() {
        // An absolute request of an existing file shows the host's root-relative path.
        let mut host = Fake::default()
            .with_file("src/a.rs", "old")
            .observed("src/a.rs", "old");
        let outcome = run_write(
            &mut host,
            r#"{"file_path": "/ws/src/a.rs", "content": "n"}"#,
        );
        assert_eq!(outcome.content, "Wrote src/a.rs (1 bytes).");
    }

    #[test]
    fn a_directory_target_reports_the_native_read_failure() {
        let mut host = Fake::default();
        host.0.borrow_mut().dirs.push("dir".into());
        let outcome = run_write(&mut host, r#"{"file_path": "dir", "content": "x"}"#);
        assert_eq!(
            outcome,
            error("dir could not be read: Is a directory (os error 21)")
        );
    }

    #[test]
    fn a_wrong_kind_that_is_not_a_directory_keeps_the_host_text() {
        // A fifo, a socket or a device is `wrong-kind` too, but `std::fs::read` fails
        // with a different operating system error there; only a directory is "Is a
        // directory".
        let mut host = Fake::default();
        host.0.borrow_mut().others.push("fifo".into());
        let outcome = run_write(&mut host, r#"{"file_path": "fifo", "content": "x"}"#);
        assert_eq!(outcome, error("fifo could not be read: not a file: fifo"));
    }

    #[test]
    fn a_host_refusal_of_the_write_is_reported_and_releases_the_gate() {
        let mut host = Fake::default()
            .with_file("out.txt", "old")
            .observed("out.txt", "old");
        host.0.borrow_mut().write_error = Some(FsError::Io(
            "out.txt changed on disk since you last read it; read it again.".into(),
        ));
        let outcome = run_write(&mut host, r#"{"file_path": "out.txt", "content": "new"}"#);
        assert_eq!(
            outcome,
            error("out.txt changed on disk since you last read it; read it again.")
        );
        let log = host.log();
        assert_eq!(log.last().map(String::as_str), Some("release"));
        assert!(!log.iter().any(|entry| entry.starts_with("observe")));

        let mut host = Fake::default();
        host.0.borrow_mut().write_error = Some(FsError::Cancelled);
        let outcome = run_write(&mut host, r#"{"file_path": "n.txt", "content": "x"}"#);
        assert_eq!(outcome.status, Status::Cancelled);
    }

    #[test]
    fn long_output_is_bounded() {
        let long = "d/".repeat(30_000);
        let request = format!("{long}f.txt");
        let mut host = Fake::default();
        let arguments = serde_json::json!({"file_path": request, "content": "x"}).to_string();
        let outcome = run_write(&mut host, &arguments);
        assert_eq!(outcome.status, Status::Ok);
        let total = format!("Wrote {request} (1 bytes).").len();
        assert!(
            outcome.content.ends_with(&format!(
                "[output truncated: showing 50000 of {total} bytes]"
            )),
            "{}",
            &outcome.content[outcome.content.len() - 60..]
        );
    }

    #[test]
    fn normalization_keeps_what_only_the_host_can_place() {
        assert_eq!(normalized_parts("/a/../../b", true), ["b"]);
        assert_eq!(normalized_parts("../ws/./x", false), ["..", "ws", "x"]);
        assert_eq!(normalized_parts("a/../../x", false), ["..", "x"]);
    }
}
