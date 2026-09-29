//! `InProcessWorkflows`: the `WorkflowService` that runs scripts in this process
//! (ADR-0053 item 1). Each run gets a directory, a journal, a script thread and a status
//! watch; runs are retained for the service's lifetime.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, AtomicUsize};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::JoinHandle;

use p1_contracts::{BoxFuture, CancellationToken};
use serde_json::Value;
use tokio::runtime::Handle;
use tokio::sync::watch;

use crate::api::{
    JournalRecord, ModelResolver, RunId, RunReport, RunStatus, StartRequest, StepRunner,
    WorkflowError, WorkflowObserver, WorkflowService, WorkflowSettings,
};
use crate::caps::CapCounter;
use crate::decision::{Decisions, NativeDecisions};
use crate::engine::{self, Role, RunState, forbidden_tool};
use crate::journal::{JournalWriter, Replay, dispatch_charges, read_journal_file, script_hash};

pub struct InProcessWorkflows {
    runner: Arc<dyn StepRunner>,
    resolver: Arc<dyn ModelResolver>,
    observer: Arc<dyn WorkflowObserver>,
    decisions: Arc<dyn Decisions>,
    settings: WorkflowSettings,
    run_root: PathBuf,
    inner: Mutex<Inner>,
}

struct Inner {
    /// The highest `wf<N>` seen or allocated; the next run is `N + 1`.
    last_number: u64,
    runs: Vec<RunEntry>,
    shut_down: bool,
}

struct RunEntry {
    state: Arc<RunState>,
    thread: Option<JoinHandle<()>>,
}

impl InProcessWorkflows {
    /// `run_root` is where every run gets its directory `run_root/<run id>/`. Numbering
    /// continues after the highest `wf<N>` already there, so ids stay unique across
    /// processes and `resume_from: wf3` always names `run_root/wf3`. The steps' decisions
    /// are the native ones ([`NativeDecisions`]).
    pub fn new(
        runner: Arc<dyn StepRunner>,
        resolver: Arc<dyn ModelResolver>,
        observer: Arc<dyn WorkflowObserver>,
        settings: WorkflowSettings,
        run_root: PathBuf,
    ) -> Arc<Self> {
        Self::with_decisions(
            runner,
            resolver,
            observer,
            settings,
            run_root,
            Arc::new(NativeDecisions),
        )
    }

    /// [`Self::new`] with the steps' decisions chosen by the caller: every run of this
    /// service asks `decisions` (S6.3; the loaded decision component is S6.9's).
    pub fn with_decisions(
        runner: Arc<dyn StepRunner>,
        resolver: Arc<dyn ModelResolver>,
        observer: Arc<dyn WorkflowObserver>,
        settings: WorkflowSettings,
        run_root: PathBuf,
        decisions: Arc<dyn Decisions>,
    ) -> Arc<Self> {
        let last_number = highest_run_number(&run_root);
        Arc::new(Self {
            runner,
            resolver,
            observer,
            decisions,
            settings,
            run_root,
            inner: Mutex::new(Inner {
                last_number,
                runs: Vec::new(),
                shut_down: false,
            }),
        })
    }

    /// Cancel every running run (each journals `Ended { outcome: Cancelled }`), join the
    /// script threads. Afterwards every fallible call returns `ShutDown`.
    pub async fn shutdown(&self) {
        let threads: Vec<JoinHandle<()>> = {
            let mut inner = self.lock();
            inner.shut_down = true;
            inner
                .runs
                .iter_mut()
                .filter_map(|entry| {
                    entry.state.token.cancel();
                    entry.thread.take()
                })
                .collect()
        };
        for thread in threads {
            // Joining blocks; keep it off the async worker.
            let _ = tokio::task::spawn_blocking(move || thread.join()).await;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poison| poison.into_inner())
    }

    fn find(&self, id: &RunId) -> Result<Arc<RunState>, WorkflowError> {
        let inner = self.lock();
        if inner.shut_down {
            return Err(WorkflowError::ShutDown);
        }
        inner
            .runs
            .iter()
            .find(|entry| entry.state.id == *id)
            .map(|entry| entry.state.clone())
            .ok_or(WorkflowError::UnknownRun)
    }

    /// The settings roles with `role_models` applied, each resolved — the whole fallback
    /// chain of each role, head first (ADR-0054 item 2). Every role is resolved, used by
    /// the script or not: a broken table fails before anything runs.
    fn resolve_roles(
        &self,
        role_models: &BTreeMap<String, String>,
    ) -> Result<BTreeMap<String, Role>, WorkflowError> {
        let preflight = WorkflowError::Preflight;
        let mut specs = self.settings.roles.clone();
        for (role, model) in role_models {
            let spec = specs
                .get_mut(role)
                .ok_or_else(|| preflight(format!("role_models names unknown role \"{role}\"")))?;
            // A `--role r=E/P` override changes the head only: the role keeps its tool
            // grant and its fallback chain.
            spec.model = model.clone();
        }
        let mut roles = BTreeMap::new();
        for (name, spec) in specs {
            if spec.tools.is_empty() {
                return Err(preflight(format!(
                    "role \"{name}\" has an empty tool grant"
                )));
            }
            if let Some(tool) = spec.tools.iter().find(|tool| forbidden_tool(tool)) {
                return Err(preflight(format!(
                    "role \"{name}\" grants \"{tool}\", which a step may not have"
                )));
            }
            let chain = std::iter::once(&spec.model)
                .chain(spec.fallback.iter())
                .map(|reference| {
                    self.resolver.resolve(reference).map_err(|reason| {
                        preflight(format!("role \"{name}\" ({reference}): {reason}"))
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;
            roles.insert(
                name,
                Role {
                    chain,
                    tools: spec.tools,
                },
            );
        }
        Ok(roles)
    }

    fn start_now(&self, request: StartRequest) -> Result<RunId, WorkflowError> {
        let preflight = WorkflowError::Preflight;
        if self.lock().shut_down {
            return Err(WorkflowError::ShutDown);
        }
        let handle = Handle::try_current()
            .map_err(|_| preflight("start must be called inside a tokio runtime".into()))?;

        let engine = engine::sandboxed_engine(self.settings.max_steps);
        let ast = engine::compile(&engine, &request.script)?;
        let roles = self.resolve_roles(&request.role_models)?;
        let args = match request.args {
            Value::Null => Value::Object(serde_json::Map::new()),
            Value::Object(map) => Value::Object(map),
            other => return Err(preflight(format!("args must be an object, not {other}"))),
        };
        let args_dynamic =
            rhai::serde::to_dynamic(&args).map_err(|error| preflight(format!("args: {error}")))?;
        let (replay, charged, recorded_base) = match &request.resume_from {
            None => (Replay::none(), BTreeMap::new(), None),
            Some(from) => {
                let path = self.run_root.join(&from.0).join("journal.jsonl");
                if from.0.contains(['/', '\\']) || from.0.starts_with('.') || !path.is_file() {
                    return Err(preflight(format!(
                        "resume_from {}: no such run journal",
                        from.0
                    )));
                }
                // An active predecessor owns its journal until Ended is synced.
                let file = std::fs::File::open(&path)
                    .map_err(|error| preflight(format!("resume_from: {error}")))?;
                file.try_lock()
                    .map_err(|error| preflight(format!("predecessor is still owned: {error}")))?;
                let records =
                    read_journal_file(&file, &path.display().to_string()).map_err(preflight)?;
                if !matches!(records.first(), Some(JournalRecord::Started { run, .. }) if run == from)
                    || !matches!(records.last(), Some(JournalRecord::Ended { .. }))
                {
                    return Err(preflight(
                        "resume requires a matching, ended predecessor".into(),
                    ));
                }
                let recorded_base = records.iter().find_map(|record| match record {
                    JournalRecord::Started { base, .. } => base.clone(),
                    _ => None,
                });
                (
                    Replay::from_records(from.clone(), &records),
                    dispatch_charges(&records).map_err(preflight)?,
                    recorded_base,
                )
            }
        };
        // A resumed run keeps the base its steps' worktrees were made from (ADR-0073).
        let base = recorded_base.or(request.base);

        // From here on a run exists. The lock is held through the spawn so a concurrent
        // `shutdown` either refuses this start or sees and cancels the run.
        let mut inner = self.lock();
        if inner.shut_down {
            return Err(WorkflowError::ShutDown);
        }
        // A script may wait indefinitely for a worker; admit only bounded active runs.
        const MAX_ACTIVE_RUNS: usize = 2;
        if inner
            .runs
            .iter()
            .filter(|entry| entry.state.ended.borrow().is_none())
            .count()
            >= MAX_ACTIVE_RUNS
        {
            return Err(preflight("too many active workflow runs".into()));
        }
        for entry in &mut inner.runs {
            if entry.thread.as_ref().is_some_and(JoinHandle::is_finished)
                && let Some(thread) = entry.thread.take()
            {
                let _ = thread.join();
            }
        }
        #[cfg(windows)]
        let windows_root_pins = {
            std::fs::create_dir_all(&self.run_root)
                .map_err(|error| WorkflowError::Io(error.to_string()))?;
            verify_windows_run_root(&self.run_root)?;
            pin_windows_directories(&self.run_root)
                .map_err(|error| WorkflowError::Io(error.to_string()))?
        };
        #[cfg(windows)]
        let (id, run_dir) = allocate_run_dir(&self.run_root, &mut inner.last_number)?;
        #[cfg(not(windows))]
        let (id, run_dir, run_dir_handle) =
            allocate_run_dir(&self.run_root, &mut inner.last_number)?;
        let io =
            |error: std::io::Error| WorkflowError::Io(format!("{}: {error}", run_dir.display()));
        #[cfg(windows)]
        let (run_dir_handle, windows_parent_pins) = {
            let mut pins = pin_windows_directories(&run_dir).map_err(io)?;
            let handle = pins
                .pop()
                .ok_or_else(|| WorkflowError::Io("run directory has no handle".into()))?;
            pins.extend(windows_root_pins);
            (handle, pins)
        };
        let artifact_dir = handle_path(&run_dir_handle, &run_dir);
        create_private_file(
            &artifact_dir.join("script.rhai"),
            crate::redact::text(&request.script).as_bytes(),
        )
        .map_err(io)?;
        let args_text = serde_json::to_vec_pretty(&crate::redact::value(&args))
            .map_err(|error| WorkflowError::Io(error.to_string()))?;
        create_private_file(&artifact_dir.join("args.json"), &args_text).map_err(io)?;
        let journal = JournalWriter::create(&artifact_dir.join("journal.jsonl")).map_err(io)?;
        journal
            .append(&JournalRecord::Started {
                journal_version: crate::api::WORKFLOW_JOURNAL_VERSION,
                run: id.clone(),
                script_hash: script_hash(&request.script),
                args,
                resumed_from: request.resume_from.clone(),
                base: base.clone(),
                inherited_charges: charged.clone(),
            })
            .map_err(io)?;

        let (ended, _) = watch::channel(None);
        let state = Arc::new(RunState {
            id: id.clone(),
            run_dir: run_dir.clone(),
            artifact_dir,
            _run_dir_handle: run_dir_handle,
            #[cfg(windows)]
            _windows_parent_pins: windows_parent_pins,
            resumed_from: request.resume_from.clone(),
            runner: self.runner.clone(),
            decisions: self.decisions.clone(),
            observer: self.observer.clone(),
            roles,
            caps: CapCounter::new(self.settings.caps.clone(), charged),
            max_steps: self.settings.max_steps,
            workspace: request.workspace,
            base,
            token: CancellationToken::new(),
            handle,
            journal,
            journal_error: Mutex::new(None),
            replay: Mutex::new(replay),
            free_threads: AtomicUsize::new(run_threads(self.settings.max_threads)),
            calls: AtomicU32::new(0),
            record: Mutex::default(),
            ended,
        });
        let script = engine::prepare(engine, ast, state.clone());
        self.observer.run_started(&id, state.resumed_from.as_ref());
        let thread = std::thread::Builder::new()
            .name("p1-wf-script".to_string())
            .spawn(move || engine::execute(script, args_dynamic));
        match thread {
            Ok(thread) => {
                inner.runs.push(RunEntry {
                    state,
                    thread: Some(thread),
                });
                Ok(id)
            }
            Err(error) => {
                let message = format!("cannot start the script thread: {error}");
                state.end(
                    Value::Null,
                    Some((crate::api::RunOutcome::Failed, message.clone())),
                );
                Err(WorkflowError::Io(message))
            }
        }
    }
}

fn highest_run_number(run_root: &Path) -> u64 {
    let Ok(entries) = std::fs::read_dir(run_root) else {
        return 0;
    };
    entries
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| run_number(&entry.file_name().to_string_lossy()))
        .max()
        .unwrap_or(0)
}

fn run_number(name: &str) -> Option<u64> {
    let digits = name.strip_prefix("wf")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// `create_dir`, not `create_dir_all`, for the run itself: another process that took the
/// same number makes it fail, and the next number is tried.
///
/// The run directory is returned with the handle that was checked against its creation:
/// the caller writes every artifact through that handle, so a pathname swapped after
/// this point cannot redirect a write. The root is opened and chmodded through its own
/// handle for the same reason.
#[cfg(not(windows))]
fn allocate_run_dir(
    run_root: &Path,
    last: &mut u64,
) -> Result<(RunId, PathBuf, std::fs::File), WorkflowError> {
    std::fs::create_dir_all(run_root)
        .map_err(|error| WorkflowError::Io(format!("{}: {error}", run_root.display())))?;
    let root_handle = open_checked_dir(run_root).map_err(|error| {
        WorkflowError::Preflight(format!("run root must be a plain owned directory: {error}"))
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        root_handle
            .set_permissions(std::fs::Permissions::from_mode(0o700))
            .map_err(|error| WorkflowError::Io(error.to_string()))?;
    }
    // Children are created through the pinned root, never through the pathname.
    let root_dir = handle_path(&root_handle, run_root);
    loop {
        *last = last
            .checked_add(1)
            .ok_or_else(|| WorkflowError::Preflight("workflow run id space exhausted".into()))?;
        let id = format!("wf{last}");
        let dir = run_root.join(&id);
        match std::fs::create_dir(root_dir.join(&id)) {
            Ok(()) => {
                let handle = open_checked_dir(&dir)
                    .map_err(|error| WorkflowError::Io(format!("{}: {error}", dir.display())))?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    handle
                        .set_permissions(std::fs::Permissions::from_mode(0o700))
                        .map_err(|error| WorkflowError::Io(error.to_string()))?;
                }
                return Ok((RunId(id), dir, handle));
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(WorkflowError::Io(format!("{}: {error}", dir.display())));
            }
        }
    }
}

/// Windows cannot open a directory with `std::fs::File::open`; its run directory is
/// pinned by [`pin_windows_directories`] in the caller instead.
#[cfg(windows)]
fn allocate_run_dir(run_root: &Path, last: &mut u64) -> Result<(RunId, PathBuf), WorkflowError> {
    std::fs::create_dir_all(run_root)
        .map_err(|error| WorkflowError::Io(format!("{}: {error}", run_root.display())))?;
    loop {
        *last = last
            .checked_add(1)
            .ok_or_else(|| WorkflowError::Preflight("workflow run id space exhausted".into()))?;
        let id = format!("wf{last}");
        let dir = run_root.join(&id);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok((RunId(id), dir)),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(error) => {
                return Err(WorkflowError::Io(format!("{}: {error}", dir.display())));
            }
        }
    }
}

/// Opens `path` as a directory and proves the returned handle still names the directory
/// that `path` named when it was checked: `symlink_metadata` refuses a symlink, and the
/// dev/inode comparison refuses a directory swapped in between the check and the open.
#[cfg(not(windows))]
fn open_checked_dir(path: &Path) -> std::io::Result<std::fs::File> {
    let before = std::fs::symlink_metadata(path)?;
    if !before.is_dir() {
        return Err(std::io::Error::other("path is not a plain directory"));
    }
    let handle = std::fs::File::open(path)?;
    let after = handle.metadata()?;
    if !after.is_dir() {
        return Err(std::io::Error::other("path is not a plain directory"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Err(std::io::Error::other(
                "directory changed while it was being opened",
            ));
        }
    }
    Ok(handle)
}

#[cfg(windows)]
fn verify_windows_run_root(run_root: &Path) -> Result<(), WorkflowError> {
    // Windows std cannot set or inspect an ACL. Fail closed on workspace paths:
    // only the account-private local application data tree may hold run artifacts.
    let local = std::env::var_os("LOCALAPPDATA").ok_or_else(|| {
        WorkflowError::Preflight("LOCALAPPDATA is required for private workflow runs".into())
    })?;
    let base = std::fs::canonicalize(PathBuf::from(local))
        .map_err(|error| WorkflowError::Preflight(format!("LOCALAPPDATA: {error}")))?;
    let root = std::fs::canonicalize(run_root)
        .map_err(|error| WorkflowError::Preflight(format!("run root: {error}")))?;
    if !root.starts_with(base) {
        return Err(WorkflowError::Preflight(
            "run root must be under private LOCALAPPDATA on Windows".into(),
        ));
    }
    Ok(())
}

/// On Windows a directory handle opened without FILE_SHARE_DELETE prevents another
/// process from renaming it. Pin every ancestor before writing an artifact: otherwise
/// replacing a writable parent could redirect a perfectly pinned child pathname.
#[cfg(windows)]
fn pin_windows_directories(path: &Path) -> std::io::Result<Vec<std::fs::File>> {
    use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
    const FILE_FLAG_BACKUP_SEMANTICS: u32 = 0x0200_0000;
    const FILE_FLAG_OPEN_REPARSE_POINT: u32 = 0x0020_0000;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x0000_0400;
    const FILE_SHARE_READ_WRITE: u32 = 0x0000_0003;

    let absolute = std::path::absolute(path)?;
    let mut pins = Vec::new();
    for ancestor in absolute
        .ancestors()
        .filter(|part| !part.as_os_str().is_empty())
        .rev()
    {
        let metadata = std::fs::symlink_metadata(ancestor)?;
        if !metadata.is_dir() || metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::other(
                "run directory ancestor is not a plain directory",
            ));
        }
        let mut options = std::fs::OpenOptions::new();
        options
            .read(true)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT)
            .share_mode(FILE_SHARE_READ_WRITE);
        let handle = options.open(ancestor)?;
        if handle.metadata()?.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(std::io::Error::other(
                "run directory ancestor changed to a reparse point",
            ));
        }
        pins.push(handle);
    }
    Ok(pins)
}

fn handle_path(handle: &std::fs::File, ordinary: &Path) -> PathBuf {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        use std::os::fd::AsRawFd;
        let _ = ordinary;
        #[cfg(target_os = "linux")]
        let prefix = "/proc/self/fd";
        #[cfg(target_os = "macos")]
        let prefix = "/dev/fd";
        PathBuf::from(format!("{prefix}/{}", handle.as_raw_fd()))
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = handle;
        ordinary.to_path_buf()
    }
}

pub(crate) fn create_private_file(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
        #[cfg(target_os = "linux")]
        options.custom_flags(0o400000);
    }
    options.open(path)?.write_all(bytes)
}

fn status_of(state: &RunState) -> RunStatus {
    match state.ended.borrow().as_ref() {
        Some(report) => RunStatus::Ended(report.clone()),
        None => RunStatus::Running(state.progress()),
    }
}

/// Resolves once the run has ended, or `None` when `cancel` fires first.
async fn ended(state: &RunState, cancel: &CancellationToken) -> Option<RunReport> {
    // Subscribe BEFORE reading: an end landing between the two marks the watch changed,
    // so no wake-up is lost.
    let mut rx = state.ended.subscribe();
    loop {
        if let Some(report) = rx.borrow_and_update().as_ref() {
            return Some(report.clone());
        }
        tokio::select! {
            biased;
            _ = cancel.cancelled() => return None,
            changed = rx.changed() => {
                // The state owns the sender, and we hold the state.
                if changed.is_err() {
                    return rx.borrow().clone();
                }
            }
        }
    }
}

impl WorkflowService for InProcessWorkflows {
    fn start<'a>(&'a self, request: StartRequest) -> BoxFuture<'a, Result<RunId, WorkflowError>> {
        Box::pin(async move { self.start_now(request) })
    }

    fn status<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            let state = self.find(id)?;
            Ok(status_of(&state))
        })
    }

    fn wait<'a>(
        &'a self,
        id: &'a RunId,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<RunStatus, WorkflowError>> {
        Box::pin(async move {
            let state = self.find(id)?;
            Ok(match ended(&state, &cancel).await {
                Some(report) => RunStatus::Ended(report),
                None => status_of(&state),
            })
        })
    }

    fn cancel<'a>(&'a self, id: &'a RunId) -> BoxFuture<'a, Result<(), WorkflowError>> {
        Box::pin(async move {
            let state = self.find(id)?;
            state.token.cancel();
            // A spinning script dies within microseconds and a blocked step is dropped
            // at once, so returning only once `Ended` is journalled costs nothing.
            ended(&state, &CancellationToken::new()).await;
            Ok(())
        })
    }

    fn list<'a>(&'a self) -> BoxFuture<'a, Vec<(RunId, RunStatus)>> {
        Box::pin(async move {
            let states: Vec<Arc<RunState>> = self
                .lock()
                .runs
                .iter()
                .map(|entry| entry.state.clone())
                .collect();
            states
                .iter()
                .map(|state| (state.id.clone(), status_of(state)))
                .collect()
        })
    }
}

/// The thunk threads a run gets: the configured `max_threads`, never more than
/// [`engine::MAX_RUN_THREADS`] — the data budget is per value, so the run's memory ceiling
/// holds only while concurrency is bounded too.
fn run_threads(configured: usize) -> usize {
    configured.min(engine::MAX_RUN_THREADS)
}

#[cfg(test)]
mod run_threads_tests {
    use super::*;

    #[cfg(windows)]
    #[test]
    fn windows_run_directory_and_ancestors_cannot_be_swapped_while_pinned() {
        let Some(local) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let root = PathBuf::from(local).join(format!("p1-wf-pinned-{}", std::process::id()));
        let run = root.join("wf1");
        std::fs::create_dir_all(&run).unwrap();
        if verify_windows_run_root(&root).is_err() {
            std::fs::remove_dir_all(&root).unwrap();
            return;
        }
        let Ok(pins) = pin_windows_directories(&run) else {
            std::fs::remove_dir_all(&root).unwrap();
            return;
        };
        assert!(std::fs::rename(&run, root.join("moved")).is_err());
        assert!(std::fs::rename(&root, root.with_extension("moved")).is_err());
        drop(pins);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(windows)]
    #[test]
    fn windows_workspace_outside_private_profile_is_refused() {
        let Some(local) = std::env::var_os("LOCALAPPDATA") else {
            return;
        };
        let base = PathBuf::from(local);
        assert!(verify_windows_run_root(base.parent().unwrap()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn run_root_and_directory_are_private_even_with_open_umask() {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("p1-wf-modes-{}", std::process::id()));
        let mut last = 0;
        let (_, run, _handle) = allocate_run_dir(&root, &mut last).unwrap();
        for path in [&root, &run] {
            assert_eq!(
                std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn renamed_run_directory_cannot_redirect_artifact_write() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("p1-wf-swap-{}", std::process::id()));
        std::fs::create_dir(&root).unwrap();
        let run = root.join("wf1");
        let external = root.join("external");
        std::fs::create_dir(&run).unwrap();
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("result.json"), b"sentinel").unwrap();
        let handle = std::fs::File::open(&run).unwrap();
        std::fs::rename(&run, root.join("moved")).unwrap();
        symlink(&external, &run).unwrap();
        create_private_file(&handle_path(&handle, &run).join("result.json"), b"report").unwrap();
        assert_eq!(
            std::fs::read(external.join("result.json")).unwrap(),
            b"sentinel"
        );
        assert_eq!(
            std::fs::read(root.join("moved/result.json")).unwrap(),
            b"report"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn an_allocated_run_directory_handle_ignores_a_path_swap() {
        use std::os::unix::fs::symlink;
        let root = std::env::temp_dir().join(format!("p1-wf-alloc-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let mut last = 0;
        let (_, run, handle) = allocate_run_dir(&root, &mut last).unwrap();
        let external = root.join("external");
        std::fs::create_dir(&external).unwrap();
        std::fs::write(external.join("result.json"), b"sentinel").unwrap();
        std::fs::rename(&run, root.join("moved")).unwrap();
        symlink(&external, &run).unwrap();
        create_private_file(&handle_path(&handle, &run).join("result.json"), b"report").unwrap();
        assert_eq!(
            std::fs::read(external.join("result.json")).unwrap(),
            b"sentinel"
        );
        assert_eq!(
            std::fs::read(root.join("moved/result.json")).unwrap(),
            b"report"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn workflow_artifact_is_private() {
        use std::os::unix::fs::PermissionsExt;
        let path = std::env::temp_dir().join(format!("p1-wf-private-{}", std::process::id()));
        create_private_file(&path, b"private").unwrap();
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(create_private_file(&path, b"overwrite").is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"private");
        std::fs::remove_file(path).unwrap();
    }

    #[test]
    fn maximum_run_number_fails_without_wrapping() {
        let root = std::env::temp_dir().join(format!("p1-wf-overflow-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        let mut last = u64::MAX;
        assert!(allocate_run_dir(&root, &mut last).is_err());
        assert!(!root.join("wf0").exists());
        std::fs::remove_dir(&root).unwrap();
    }

    #[test]
    fn a_configured_thread_count_above_the_ceiling_is_clamped() {
        assert_eq!(run_threads(8), 8);
        assert_eq!(
            run_threads(engine::MAX_RUN_THREADS),
            engine::MAX_RUN_THREADS
        );
        assert_eq!(run_threads(10_000), engine::MAX_RUN_THREADS);
    }
}
