//! The ACP-neutral durable session catalog (ADR-0158). A private directory per
//! opaque id holds workspace metadata and the existing JSONL journal format.

use std::fs::{self, DirBuilder, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use p1_contracts::frontend::{SessionArchive, StoredSession};

pub struct JournalArchive {
    root: PathBuf,
    next: AtomicU64,
}

impl JournalArchive {
    pub fn new(root: PathBuf) -> Self {
        Self {
            root,
            next: AtomicU64::new(0),
        }
    }

    fn entry(&self, id: &str) -> Result<StoredSession, String> {
        if id.is_empty() || !id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-') {
            return Err("invalid session id".into());
        }
        let directory = self.root.join(id);
        if fs::symlink_metadata(&directory)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("session directory must not be a symlink".into());
        }
        let workspace: PathBuf = serde_json::from_slice(
            &fs::read(directory.join("workspace.json")).map_err(|e| e.to_string())?,
        )
        .map_err(|e| e.to_string())?;
        Ok(StoredSession {
            id: id.into(),
            workspace,
            journal: directory.join("journal.jsonl"),
        })
    }
}

fn private_file(path: &Path) -> Result<std::fs::File, String> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    options.open(path).map_err(|e| e.to_string())
}

impl SessionArchive for JournalArchive {
    fn create(&self, workspace: &Path) -> Result<StoredSession, String> {
        let workspace = workspace.canonicalize().map_err(|e| e.to_string())?;
        let mut builder = DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.root).map_err(|e| e.to_string())?;
        let stamp = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| e.to_string())?
            .as_nanos();
        let id = format!(
            "p1-{stamp:032x}-{}-{}",
            std::process::id(),
            self.next.fetch_add(1, Ordering::Relaxed)
        );
        let directory = self.root.join(&id);
        builder
            .recursive(false)
            .create(&directory)
            .map_err(|e| e.to_string())?;
        let mut metadata = private_file(&directory.join("workspace.json"))?;
        metadata
            .write_all(&serde_json::to_vec(&workspace).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        metadata.sync_all().map_err(|e| e.to_string())?;
        #[cfg(unix)]
        for ancestor in directory.ancestors().filter(|p| !p.as_os_str().is_empty()) {
            fs::File::open(ancestor)
                .and_then(|f| f.sync_all())
                .map_err(|e| e.to_string())?;
        }
        Ok(StoredSession {
            id,
            workspace,
            journal: directory.join("journal.jsonl"),
        })
    }

    fn get(&self, id: &str, workspace: &Path) -> Result<StoredSession, String> {
        let session = self.entry(id)?;
        if workspace.canonicalize().map_err(|e| e.to_string())? != session.workspace {
            return Err("session belongs to a different cwd".into());
        }
        if fs::symlink_metadata(&session.journal)
            .map_err(|e| e.to_string())?
            .file_type()
            .is_symlink()
        {
            return Err("session journal must not be a symlink".into());
        }
        Ok(session)
    }

    fn list(&self) -> Result<Vec<StoredSession>, String> {
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error.to_string()),
        };
        let mut sessions = Vec::new();
        for entry in entries {
            let entry = entry.map_err(|e| e.to_string())?;
            // An interrupted create/fork or failed launch is not published yet.
            // Check before parsing metadata, which may also be incomplete.
            if !entry.path().join("journal.jsonl").is_file() {
                continue;
            }
            let id = entry.file_name().to_string_lossy().into_owned();
            let session = self.entry(&id)?;
            sessions.push(session);
        }
        Ok(sessions)
    }

    fn fork(&self, id: &str, workspace: &Path) -> Result<StoredSession, String> {
        let source = self.get(id, workspace)?;
        // load reads a bounded snapshot and validates dense sequences; a live
        // writer only appends. A torn last line is excluded, never repaired here.
        let loaded = p1_journal::load(&source.journal).map_err(|e| e.to_string())?;
        let fork = self.create(workspace)?;
        // Publish only a complete snapshot: another router must not resume a
        // partially written copy and repair away the remaining records.
        let pending = fork.journal.with_extension("pending");
        let mut writer = private_file(&pending)?;
        writeln!(writer, "{{\"p1_journal\":{}}}", loaded.version).map_err(|e| e.to_string())?;
        let mut assemblies = loaded.assemblies.iter().peekable();
        for seq in 0..=loaded.records.len() {
            while assemblies.peek().is_some_and(|a| a.from_seq == seq as u64) {
                let assembly = assemblies.next().expect("checked above");
                writeln!(
                    writer,
                    "{}",
                    serde_json::json!({"assembly":assembly.identity})
                )
                .map_err(|e| e.to_string())?;
            }
            if let Some(record) = loaded.records.get(seq) {
                let mut value = serde_json::to_value(record).map_err(|e| e.to_string())?;
                if let Some(at_ms) = loaded.at_ms[seq] {
                    value["at_ms"] = serde_json::json!(at_ms);
                }
                writeln!(writer, "{value}").map_err(|e| e.to_string())?;
            }
        }
        writer.sync_all().map_err(|e| e.to_string())?;
        fs::rename(pending, &fork.journal).map_err(|e| e.to_string())?;
        #[cfg(unix)]
        fs::File::open(fork.journal.parent().expect("archive directory"))
            .and_then(|f| f.sync_all())
            .map_err(|e| e.to_string())?;
        Ok(fork)
    }
}
