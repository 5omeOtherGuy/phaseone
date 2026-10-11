//! The ACP-neutral durable session archive (ADR-0158): the [`SessionArchive`] port over
//! one front-end directory of the session catalog (`catalog.rs`, ADR-0171). A private
//! directory per id holds workspace metadata and the existing JSONL journal format.

use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use p1_contracts::frontend::{SessionArchive, StoredSession};

use super::catalog::{self, private_file};

pub struct JournalArchive {
    root: PathBuf,
}

impl JournalArchive {
    /// The archive over `root`, the directory that holds one session directory per id
    /// (`<state>/sessions/acp` for `p1 acp`).
    pub fn new(root: PathBuf) -> Self {
        Self { root }
    }

    fn entry(&self, id: &str) -> Result<StoredSession, String> {
        catalog::entry_in(&self.root, catalog::ACP, id).map(|entry| entry.stored())
    }
}

impl SessionArchive for JournalArchive {
    fn create(&self, workspace: &Path) -> Result<StoredSession, String> {
        catalog::create_in(&self.root, catalog::ACP, workspace).map(|entry| entry.stored())
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
        Ok(catalog::list_in(&self.root, catalog::ACP)?
            .iter()
            .map(catalog::Entry::stored)
            .collect())
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
                // Absent when nothing was masked, as the parent's line has it (ADR-0184).
                if loaded.masked[seq] > 0 {
                    value["masked"] = serde_json::json!(loaded.masked[seq]);
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
