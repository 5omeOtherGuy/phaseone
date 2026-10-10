//! The session catalog (ADR-0171): every durable session the host keeps for the
//! operator, whichever front end started it, under the host's state directory
//! ([`crate::state::root`]).
//!
//! ```text
//! <state>/sessions/<front end>/<id>/workspace.json   the canonical workspace, a JSON string
//! <state>/sessions/<front end>/<id>/journal.jsonl    the journal; its existence publishes the session
//! ```
//!
//! `<front end>` is [`CLI`] (the default journal of an interactive run) or [`ACP`]
//! (`p1 acp`, ADR-0158, which wrote this layout first). A session's worker journals,
//! output store, scratch directory and workflow runs are `journal.jsonl.*` siblings
//! (`session.rs`), so one session is one directory: listing reads `journal.jsonl` only,
//! and pruning removes the directory. Ids are `<yyyymmdd-hhmmss>-<6 hex>` (UTC);
//! ids an earlier build gave (`p1-<hex>-<pid>-<n>`) stay valid.

use std::fs::{self, DirBuilder, File, OpenOptions, TryLockError};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use p1_contracts::RecordBody;
use p1_contracts::frontend::StoredSession;

/// The front end of an interactive `p1` run (line mode or TUI).
pub const CLI: &str = "cli";
/// `p1 acp` (ADR-0158).
pub const ACP: &str = "acp";
/// Every front-end directory the catalog reads, in this order.
const FRONT_ENDS: [&str; 2] = [CLI, ACP];

/// The journal file inside a session directory.
const JOURNAL: &str = "journal.jsonl";
/// The workspace metadata file inside a session directory.
const WORKSPACE: &str = "workspace.json";
/// How many characters of the first prompt a summary keeps.
const PROMPT_CHARS: usize = 80;

/// One published session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Entry {
    pub id: String,
    pub front_end: String,
    pub workspace: PathBuf,
    /// The session's own directory; everything the session owns is inside it.
    pub directory: PathBuf,
    pub journal: PathBuf,
}

impl Entry {
    /// The front-end-neutral view an ACP router keeps (ADR-0158).
    pub fn stored(&self) -> StoredSession {
        StoredSession {
            id: self.id.clone(),
            workspace: self.workspace.clone(),
            journal: self.journal.clone(),
        }
    }
}

/// What `p1 sessions` reports of one session. Read without taking the journal over:
/// a live session is listed as it is on disk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Summary {
    pub id: String,
    pub front_end: String,
    /// `None` when the metadata cannot be read (then `warning` says why).
    pub workspace: Option<PathBuf>,
    pub directory: PathBuf,
    pub journal: PathBuf,
    /// Unix milliseconds: the first record's `at_ms`, else when the session was created.
    pub started_ms: Option<u64>,
    /// Unix milliseconds of the journal's last write.
    pub modified_ms: Option<u64>,
    /// The last `Environment` record's route and model.
    pub route: Option<String>,
    pub model: Option<String>,
    /// The first `user_input`, whitespace collapsed, at most 80 characters.
    pub first_prompt: Option<String>,
    /// Complete records in the journal.
    pub records: usize,
    /// Whether a live process holds the journal's writer lock (ADR-0031).
    pub live: bool,
    /// Why the summary may be incomplete: a truncated tail, an unreadable journal.
    pub warning: Option<String>,
}

/// The catalog under one `sessions/` directory.
#[derive(Debug, Clone)]
pub struct SessionCatalog {
    root: PathBuf,
}

impl SessionCatalog {
    /// The catalog of the state directory `state` (`<state>/sessions`).
    pub fn at_state(state: &Path) -> Self {
        Self {
            root: state.join("sessions"),
        }
    }

    /// The directory holding one front end's sessions.
    pub fn front_end_dir(&self, front_end: &str) -> PathBuf {
        self.root.join(front_end)
    }

    /// A NEW, empty session of `front_end` for `workspace`. The journal does not exist
    /// yet: the caller creates it, which publishes the session.
    pub fn create(&self, front_end: &str, workspace: &Path) -> Result<Entry, String> {
        create_in(&self.front_end_dir(front_end), front_end, workspace)
    }

    /// The published session `id`, whichever front end created it.
    pub fn find(&self, id: &str) -> Result<Entry, String> {
        validate_id(id)?;
        let mut found = Vec::new();
        for front_end in FRONT_ENDS {
            let directory = self.front_end_dir(front_end).join(id);
            if directory.join(JOURNAL).is_file() {
                found.push(entry_in(&self.front_end_dir(front_end), front_end, id)?);
            }
        }
        match found.len() {
            0 => Err(format!(
                "no session `{id}` in {} (`p1 sessions` lists them)",
                self.root.display()
            )),
            1 => Ok(found.remove(0)),
            _ => Err(format!(
                "session id `{id}` names more than one session in {}",
                self.root.display()
            )),
        }
    }

    /// Every published session, newest first, each read as [`summarize`] reads it. A
    /// session whose files cannot be read is listed with a warning, never skipped.
    pub fn summaries(&self) -> Result<Vec<Summary>, String> {
        let mut summaries = Vec::new();
        for front_end in FRONT_ENDS {
            let dir = self.front_end_dir(front_end);
            for id in published_ids(&dir)? {
                summaries.push(summarize(&dir, front_end, &id));
            }
        }
        summaries.sort_by(|a, b| {
            b.started_ms
                .cmp(&a.started_ms)
                .then_with(|| b.id.cmp(&a.id))
        });
        Ok(summaries)
    }

    /// The newest session of `workspace` (compared canonically), any front end.
    pub fn newest_of(&self, workspace: &Path) -> Result<Option<Summary>, String> {
        let workspace = workspace
            .canonicalize()
            .map_err(|error| format!("{}: {error}", workspace.display()))?;
        Ok(self
            .summaries()?
            .into_iter()
            .find(|summary| summary.workspace.as_deref() == Some(workspace.as_path())))
    }

    /// Remove one session directory while holding its journal's writer lock, so a
    /// process that resumes it meanwhile cannot be left writing a deleted file. A live
    /// session is refused.
    pub fn remove(&self, summary: &Summary) -> Result<(), String> {
        let parent = self.front_end_dir(&summary.front_end);
        if summary.directory.parent() != Some(parent.as_path())
            || fs::symlink_metadata(&summary.directory)
                .map_err(|error| error.to_string())?
                .file_type()
                .is_symlink()
        {
            return Err(format!(
                "{} is not a session directory of this catalog",
                summary.directory.display()
            ));
        }
        let journal = File::open(&summary.journal).map_err(|error| error.to_string())?;
        match journal.try_lock() {
            Ok(()) => {}
            Err(TryLockError::WouldBlock) => {
                return Err(format!("session `{}` is live", summary.id));
            }
            Err(TryLockError::Error(error)) => return Err(error.to_string()),
        }
        fs::remove_dir_all(&summary.directory).map_err(|error| error.to_string())
    }
}

/// Sessions of [`SessionCatalog::summaries`] not written to for `older_than` before
/// `now`, oldest activity first. Live sessions are never candidates.
pub fn prune_candidates(
    summaries: &[Summary],
    older_than: Duration,
    now: SystemTime,
) -> Vec<Summary> {
    let cutoff = now
        .checked_sub(older_than)
        .and_then(|cutoff| cutoff.duration_since(UNIX_EPOCH).ok())
        .map(|cutoff| cutoff.as_millis() as u64);
    let mut old: Vec<Summary> = summaries
        .iter()
        .filter(|summary| !summary.live)
        .filter(|summary| {
            let last = summary.modified_ms.or(summary.started_ms);
            matches!((last, cutoff), (Some(last), Some(cutoff)) if last < cutoff)
        })
        .cloned()
        .collect();
    old.sort_by_key(|summary| summary.modified_ms.or(summary.started_ms));
    old
}

/// Create a session directory under `dir`, mode 0700, with its `workspace.json` (0600,
/// synced with every ancestor so a crash cannot publish a directory without it).
pub(crate) fn create_in(dir: &Path, front_end: &str, workspace: &Path) -> Result<Entry, String> {
    let workspace = workspace
        .canonicalize()
        .map_err(|error| format!("{}: {error}", workspace.display()))?;
    let mut builder = DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder
        .create(dir)
        .map_err(|error| format!("{}: {error}", dir.display()))?;
    builder.recursive(false);
    // A fresh random suffix per attempt: two sessions started in the same second
    // collide only when the suffixes do too, and then the second attempt differs.
    let mut attempts = 0;
    let (id, directory) = loop {
        let id = new_id(SystemTime::now());
        let directory = dir.join(&id);
        match builder.create(&directory) {
            Ok(()) => break (id, directory),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && attempts < 8 => {
                attempts += 1;
            }
            Err(error) => return Err(format!("{}: {error}", directory.display())),
        }
    };
    let mut metadata = private_file(&directory.join(WORKSPACE))?;
    metadata
        .write_all(&serde_json::to_vec(&workspace).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    metadata.sync_all().map_err(|e| e.to_string())?;
    #[cfg(unix)]
    for ancestor in directory.ancestors().filter(|p| !p.as_os_str().is_empty()) {
        File::open(ancestor)
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
    }
    Ok(Entry {
        id,
        front_end: front_end.to_string(),
        workspace,
        journal: directory.join(JOURNAL),
        directory,
    })
}

/// The session `id` under `dir`: its directory must not be a symlink and its
/// `workspace.json` must parse. Whether the journal exists is the caller's question.
pub(crate) fn entry_in(dir: &Path, front_end: &str, id: &str) -> Result<Entry, String> {
    validate_id(id)?;
    let directory = dir.join(id);
    if fs::symlink_metadata(&directory)
        .map_err(|e| e.to_string())?
        .file_type()
        .is_symlink()
    {
        return Err("session directory must not be a symlink".into());
    }
    let workspace: PathBuf =
        serde_json::from_slice(&fs::read(directory.join(WORKSPACE)).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
    Ok(Entry {
        id: id.into(),
        front_end: front_end.to_string(),
        workspace,
        journal: directory.join(JOURNAL),
        directory,
    })
}

/// Every published session under `dir`, in directory order.
pub(crate) fn list_in(dir: &Path, front_end: &str) -> Result<Vec<Entry>, String> {
    published_ids(dir)?
        .iter()
        .map(|id| entry_in(dir, front_end, id))
        .collect()
}

/// The ids under `dir` whose journal exists. An interrupted create or fork, or a failed
/// launch, is not published yet; anything that is not an id is not a session.
fn published_ids(dir: &Path) -> Result<Vec<String>, String> {
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(format!("{}: {error}", dir.display())),
    };
    let mut ids = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|e| e.to_string())?;
        if !entry.path().join(JOURNAL).is_file() {
            continue;
        }
        let id = entry.file_name().to_string_lossy().into_owned();
        if validate_id(&id).is_ok() {
            ids.push(id);
        }
    }
    Ok(ids)
}

/// The summary of session `id` under `dir`. Failures become the warning.
fn summarize(dir: &Path, front_end: &str, id: &str) -> Summary {
    let directory = dir.join(id);
    let journal = directory.join(JOURNAL);
    let mut warnings = Vec::new();
    let workspace = match entry_in(dir, front_end, id) {
        Ok(entry) => Some(entry.workspace),
        Err(error) => {
            warnings.push(format!("unreadable {WORKSPACE}: {error}"));
            None
        }
    };
    let mut summary = Summary {
        id: id.to_string(),
        front_end: front_end.to_string(),
        workspace,
        directory: directory.clone(),
        journal: journal.clone(),
        started_ms: None,
        modified_ms: modified_ms(&journal),
        route: None,
        model: None,
        first_prompt: None,
        records: 0,
        live: false,
        warning: None,
    };
    match is_locked(&journal) {
        Ok(live) => summary.live = live,
        Err(error) => warnings.push(format!("lock status unknown: {error}")),
    }
    match p1_journal::load(&journal) {
        Ok(loaded) => {
            if let Some(tail) = loaded.truncated_tail {
                warnings.push(format!(
                    "incomplete last record ({} bytes); a resume cuts it off",
                    tail.bytes
                ));
            }
            summary.records = loaded.records.len();
            summary.started_ms = loaded.at_ms.first().copied().flatten();
            for record in &loaded.records {
                match &record.body {
                    RecordBody::Environment { route, .. } => {
                        summary.route = Some(route.origin.route.clone());
                        summary.model = Some(route.origin.model.clone());
                    }
                    RecordBody::UserInput { text } if summary.first_prompt.is_none() => {
                        summary.first_prompt = Some(prompt_excerpt(text));
                    }
                    _ => {}
                }
            }
        }
        Err(error) => warnings.push(format!("unreadable journal: {error}")),
    }
    // A session with no record yet started when it was created: `workspace.json` is
    // written once, at creation.
    if summary.started_ms.is_none() {
        summary.started_ms = modified_ms(&directory.join(WORKSPACE));
    }
    if !warnings.is_empty() {
        summary.warning = Some(warnings.join("; "));
    }
    summary
}

/// Whether another process holds `journal`'s writer lock. A shared probe: it never
/// conflicts with another reader, only with the writer.
fn is_locked(journal: &Path) -> std::io::Result<bool> {
    let file = File::open(journal)?;
    match file.try_lock_shared() {
        Ok(()) => Ok(false),
        Err(TryLockError::WouldBlock) => Ok(true),
        Err(TryLockError::Error(error)) => Err(error),
    }
}

fn modified_ms(path: &Path) -> Option<u64> {
    let modified = fs::metadata(path).ok()?.modified().ok()?;
    Some(modified.duration_since(UNIX_EPOCH).ok()?.as_millis() as u64)
}

/// `text` on one line, at most [`PROMPT_CHARS`] characters (a longer one ends in `…`).
fn prompt_excerpt(text: &str) -> String {
    let line = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if line.chars().count() <= PROMPT_CHARS {
        return line;
    }
    let mut excerpt: String = line.chars().take(PROMPT_CHARS - 1).collect();
    excerpt.push('…');
    excerpt
}

/// A new id for a session created at `now`: `<yyyymmdd-hhmmss>-<6 hex>`, UTC.
fn new_id(now: SystemTime) -> String {
    let seconds = now
        .duration_since(UNIX_EPOCH)
        .map(|since| since.as_secs() as i64)
        .unwrap_or(0);
    let (year, month, day, hour, minute, second) = crate::civil_utc(seconds);
    format!(
        "{year:04}{month:02}{day:02}-{hour:02}{minute:02}{second:02}-{}",
        &super::random_hex()[..6]
    )
}

/// An id is a directory name: ASCII letters, digits and `-`, nothing that walks.
fn validate_id(id: &str) -> Result<(), String> {
    if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
        return Err(format!("invalid session id `{id}`"));
    }
    Ok(())
}

pub(crate) fn private_file(path: &Path) -> Result<File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_id_is_the_utc_second_and_six_hex_digits() {
        // 2024-02-29T12:34:56Z
        let id = new_id(UNIX_EPOCH + Duration::from_secs(1_709_210_096));
        let (stamp, suffix) = id.split_at(16);
        assert_eq!(stamp, "20240229-123456-");
        assert_eq!(suffix.len(), 6);
        assert!(suffix.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(validate_id(&id).is_ok());
    }

    #[test]
    fn an_id_that_walks_out_of_the_catalog_is_refused() {
        for id in ["", "..", "a/b", "a b", "../x"] {
            assert!(validate_id(id).is_err(), "{id:?}");
        }
        // The ids `p1 acp` gave before ADR-0171 stay valid.
        assert!(validate_id("p1-0000000000000000000000000000abcd-42-0").is_ok());
    }

    #[test]
    fn a_long_prompt_is_cut_to_eighty_characters_on_one_line() {
        assert_eq!(prompt_excerpt("fix\n  the   bug"), "fix the bug");
        let long = "é".repeat(100);
        let excerpt = prompt_excerpt(&long);
        assert_eq!(excerpt.chars().count(), 80);
        assert!(excerpt.ends_with('…'));
    }

    #[test]
    fn prune_takes_old_idle_sessions_and_never_a_live_one() {
        let now = UNIX_EPOCH + Duration::from_secs(100 * 86_400);
        let day = 86_400_000;
        let summary = |id: &str, modified_days: u64, live: bool| Summary {
            id: id.into(),
            front_end: CLI.into(),
            workspace: None,
            directory: PathBuf::new(),
            journal: PathBuf::new(),
            started_ms: Some(0),
            modified_ms: Some(modified_days * day),
            route: None,
            model: None,
            first_prompt: None,
            records: 0,
            live,
            warning: None,
        };
        let all = [
            summary("recent", 95, false),
            summary("old", 10, false),
            summary("older", 5, false),
            summary("old-live", 1, true),
        ];
        let ids: Vec<String> = prune_candidates(&all, Duration::from_secs(30 * 86_400), now)
            .into_iter()
            .map(|summary| summary.id)
            .collect();
        assert_eq!(ids, ["older", "old"]);
    }
}
