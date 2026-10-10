//! The run journal (ADR-0053 item 6): one `JournalRecord` per line, append-only, and the
//! prefix replay a resumed run reads from its predecessor's journal.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Write};
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

use crate::api::{
    CallId, JournalRecord, RunId, StepEnvelope, StepStatus, WORKFLOW_JOURNAL_VERSION,
};
use crate::decision::{ReplayEntry, ReplayView};

/// Appends records to `journal.jsonl`. Each record is written with one `write_all` on an
/// unbuffered file, so a crash loses at most the line being written and never reorders.
trait JournalOutput: Write + Send {
    fn durable(&mut self) -> std::io::Result<()>;
}

impl JournalOutput for File {
    fn durable(&mut self) -> std::io::Result<()> {
        self.sync_data()
    }
}

pub(crate) struct JournalWriter {
    file: Mutex<Option<Box<dyn JournalOutput>>>,
}

impl JournalWriter {
    pub(crate) fn create(path: &Path) -> std::io::Result<Self> {
        let mut options = OpenOptions::new();
        options.create_new(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
            #[cfg(target_os = "linux")]
            options.custom_flags(0o400000); // O_NOFOLLOW
        }
        let file = options.open(path)?;
        file.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => std::io::Error::other("journal is already owned"),
            std::fs::TryLockError::Error(error) => error,
        })?;
        Ok(Self {
            file: Mutex::new(Some(Box::new(file))),
        })
    }

    pub(crate) fn append(&self, record: &JournalRecord) -> std::io::Result<()> {
        let mut safe = record.clone();
        match &mut safe {
            JournalRecord::Started { args, .. } => *args = crate::redact::value(args),
            JournalRecord::Dispatch { prompt, opts, .. } => {
                *prompt = crate::redact::text(prompt);
                *opts = crate::redact::value(opts);
            }
            JournalRecord::Fallback { error, .. } => *error = crate::redact::text(error),
            JournalRecord::Result { envelope, .. } => {
                envelope.value = crate::redact::value(&envelope.value);
                envelope.error = envelope.error.as_deref().map(crate::redact::text);
                envelope.evidence = envelope.evidence.as_deref().map(crate::redact::text);
            }
            JournalRecord::Ended { error, .. } => {
                *error = error.as_deref().map(crate::redact::text)
            }
            JournalRecord::Phase { name } => *name = crate::redact::text(name),
            JournalRecord::Capped { .. } | JournalRecord::Replayed { .. } => {}
        }
        let mut line =
            crate::redact::text(&serde_json::to_string(&safe).map_err(std::io::Error::other)?);
        line.push('\n');
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let output = file
            .as_mut()
            .ok_or_else(|| std::io::Error::other("journal writer failed"))?;
        let result = output
            .write_all(line.as_bytes())
            .and_then(|()| output.durable());
        if let Err(error) = result {
            // The line may be torn or its sync uncertain: never append another record.
            *file = None;
            return Err(error);
        }
        Ok(())
    }

    /// Release journal ownership only after terminal record was synced.
    pub(crate) fn close(&self) {
        *self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner()) = None;
    }
}

/// Reads a journal back. A final line without its newline is a write a crash interrupted
/// and is ignored; any other unreadable line is an error, because silently skipping a
/// `Dispatch` would under-charge the caps of the resuming run — and, for the host's
/// resume-time worker-id reservation (issue #98), a skipped line could hide the only
/// record of a worker id whose journal file is gone.
///
/// Public because the host reads run journals to reserve worker ids on resume; the
/// crash-tolerance rule lives here once, not in every reader.
///
/// The journal's format version is read from its leading `started` record before any
/// record is parsed: a version this build does not know is an error, never a guess — a
/// newer format could mean something else by the very records this build would accept.
/// A leading record without a version is version 1 (every journal written before the
/// version existed).
pub fn read_journal(path: &Path) -> Result<Vec<JournalRecord>, String> {
    let file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    read_journal_file(&file, &path.display().to_string())
}

/// The same read over a journal [`File`] the caller already holds. A caller that took
/// the journal's advisory lock reads through this, so it parses exactly the bytes it
/// locked and never a pathname a concurrent rename or replacement could redirect.
pub(crate) fn read_journal_file(file: &File, label: &str) -> Result<Vec<JournalRecord>, String> {
    const MAX_JOURNAL: u64 = 256 * 1024 * 1024;
    if file.metadata().map_err(|error| error.to_string())?.len() > MAX_JOURNAL {
        return Err("workflow journal exceeds size limit".into());
    }
    let mut reader = BufReader::new(file);
    read_records(&mut reader, label)
}

fn read_records<R: BufRead>(reader: &mut R, label: &str) -> Result<Vec<JournalRecord>, String> {
    const MAX_LINE: u64 = 8 * 1024 * 1024;
    let mut records = Vec::new();
    let mut pending: std::collections::HashMap<CallId, u32> = std::collections::HashMap::new();
    // The strict lifecycle checks (a `started` first, no duplicate `started`, `ended`
    // last, every `result` matched by a dispatch or replay) apply to a journal that
    // opens with its `started` record. A fragment that does not is read leniently: the
    // host's worker-id scan reads such a fragment, and the resume path re-checks the
    // first and last records itself before it replays anything.
    let mut lifecycle = false;
    let mut index = 0;
    loop {
        let mut bytes = Vec::new();
        let count = (&mut *reader)
            .take(MAX_LINE + 1)
            .read_until(b'\n', &mut bytes)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        index += 1;
        if count as u64 > MAX_LINE {
            return Err(format!("{label} line {index}: too long"));
        }
        if bytes.last() != Some(&b'\n') {
            break;
        }
        bytes.pop();
        let line = std::str::from_utf8(&bytes)
            .map_err(|error| format!("{label} line {index}: {error}"))?;
        if index == 1 {
            check_version(line).map_err(|error| format!("{label} line {index}: {error}"))?;
        }
        let record = serde_json::from_str::<JournalRecord>(line)
            .map_err(|error| format!("{label} line {index}: {error}"))?;
        if index == 1 && matches!(&record, JournalRecord::Started { .. }) {
            lifecycle = true;
        }
        match record {
            JournalRecord::Started {
                journal_version, ..
            } if index == 1 => {
                if journal_version != WORKFLOW_JOURNAL_VERSION {
                    return Err(unknown_version(&journal_version.to_string()));
                }
            }
            JournalRecord::Started {
                journal_version, ..
            } => {
                if journal_version != WORKFLOW_JOURNAL_VERSION {
                    return Err(format!(
                        "{label} line {index}: {}",
                        unknown_version(&journal_version.to_string())
                    ));
                }
                return Err(format!("{label} line {index}: duplicate started"));
            }
            JournalRecord::Ended { .. }
                if reader
                    .fill_buf()
                    .map_err(|error| error.to_string())?
                    .is_empty() => {}
            JournalRecord::Ended { .. } => {
                return Err(format!("{label} line {index}: ended before last line"));
            }
            JournalRecord::Result {
                ref call,
                ref envelope,
            } if lifecycle => {
                if envelope.step != *call {
                    return Err(format!(
                        "{label} line {index}: result belongs to another call"
                    ));
                }
                // Parallel calls can share one call id, so a dispatch or replay is
                // consumed, not merely seen: each result needs its own.
                let open = pending.get(call).copied().unwrap_or(0);
                if open == 0 && (envelope.status == StepStatus::Done || envelope.attempts > 0) {
                    return Err(format!(
                        "{label} line {index}: result has no dispatch or replay"
                    ));
                }
                if open > 0 {
                    *pending.get_mut(call).expect("counted above") -= 1;
                }
            }
            _ => {}
        }
        if let JournalRecord::Dispatch { call, .. } | JournalRecord::Replayed { call, .. } = &record
        {
            *pending.entry(call.clone()).or_insert(0) += 1;
        }
        records.push(record);
    }
    if records.is_empty() {
        return Err(format!("{label}: missing started"));
    }
    Ok(records)
}

/// The key the leading `started` record carries the version under (see `JournalRecord`).
const JOURNAL_VERSION_KEY: &str = "p1_workflow_journal";

/// The version check of a journal's leading line, on the raw JSON so a newer format is
/// refused before its records are read as this build's. A line that is not a JSON object
/// is left to the record parse (a crash-torn tail or a corrupt line, reported there).
fn check_version(line: &str) -> Result<(), String> {
    let Ok(Value::Object(fields)) = serde_json::from_str::<Value>(line) else {
        return Ok(());
    };
    match fields.get(JOURNAL_VERSION_KEY) {
        // Written before the version existed: version 1.
        None => Ok(()),
        Some(version) if version.as_u64() == Some(u64::from(WORKFLOW_JOURNAL_VERSION)) => Ok(()),
        Some(version) => Err(unknown_version(&version.to_string())),
    }
}

fn unknown_version(version: &str) -> String {
    format!(
        "workflow journal version {version} is not one this build reads (it reads version {WORKFLOW_JOURNAL_VERSION}); refusing to guess"
    )
}

/// Attempts the old run spent per wire model: every `Dispatch`, matched on resume or not,
/// is charged to the resuming run — nothing is refunded (ADR-0053 item 4).
pub(crate) fn dispatch_charges(records: &[JournalRecord]) -> Result<BTreeMap<String, u32>, String> {
    let mut charges = match records.first() {
        Some(JournalRecord::Started {
            inherited_charges, ..
        }) => inherited_charges.clone(),
        _ => return Err("workflow journal has no started record".into()),
    };
    for record in records {
        if let JournalRecord::Dispatch { wire_model, .. } = record {
            let charge = charges.entry(wire_model.clone()).or_insert(0u32);
            *charge = charge
                .checked_add(1)
                .ok_or("workflow dispatch charge overflow")?;
        }
    }
    Ok(charges)
}

/// The prefix replay of one resumed run. Matching is by call id among the entries not yet
/// taken, not by position: `parallel` reaches `agent()` in a different order every run. The
/// first call without a match latches replay off for the rest of the run — the prefix rule:
/// everything after a changed call re-runs even if it looks unchanged, because its inputs
/// may have come from the changed call.
pub(crate) struct Replay {
    from: Option<RunId>,
    entries: Vec<(CallId, StepEnvelope)>,
    taken: Vec<bool>,
    latched_off: bool,
}

impl Replay {
    pub(crate) fn none() -> Self {
        Self {
            from: None,
            entries: Vec::new(),
            taken: Vec::new(),
            latched_off: true,
        }
    }

    /// Only `done` results are replayable: a failed, blocked or cancelled step must run again.
    pub(crate) fn from_records(from: RunId, records: &[JournalRecord]) -> Self {
        let entries: Vec<(CallId, StepEnvelope)> = records
            .iter()
            .filter_map(|record| match record {
                JournalRecord::Result { call, envelope } if envelope.status == StepStatus::Done => {
                    Some((call.clone(), envelope.clone()))
                }
                _ => None,
            })
            .collect();
        Self {
            from: Some(from),
            taken: vec![false; entries.len()],
            entries,
            latched_off: false,
        }
    }

    /// The prefix as a decision sees it: the entries not yet taken, and the latch.
    pub(crate) fn view(&self) -> ReplayView {
        ReplayView {
            latched_off: self.latched_off,
            open: self
                .entries
                .iter()
                .enumerate()
                .filter(|(index, _)| !self.taken[*index])
                .filter_map(|(index, (call, _))| {
                    Some(ReplayEntry {
                        index: u32::try_from(index).ok()?,
                        call: call.clone(),
                    })
                })
                .collect(),
        }
    }

    /// The recorded envelope at `index` for `call` and the run it came from; `None` when
    /// another call took it or missed the prefix since the decision saw it. Checked and
    /// taken under the one lock the caller holds, so two calls never take one entry.
    pub(crate) fn take_entry(
        &mut self,
        index: u32,
        call: &CallId,
    ) -> Option<(StepEnvelope, RunId)> {
        let index = usize::try_from(index).ok()?;
        let from = self.from.as_ref()?;
        let (id, envelope) = self.entries.get(index)?;
        if self.latched_off || self.taken[index] || id != call {
            return None;
        }
        self.taken[index] = true;
        Some((envelope.clone(), from.clone()))
    }

    /// The first call the prefix did not answer: nothing after it is replayed.
    pub(crate) fn latch_off(&mut self) {
        self.latched_off = true;
    }

    /// The recorded envelope for `call` and the run it came from, or `None` (latching off):
    /// the matching rule the decisions apply, in one call.
    #[cfg(test)]
    pub(crate) fn take(&mut self, call: &CallId) -> Option<(StepEnvelope, RunId)> {
        if self.latched_off {
            return None;
        }
        let found = self
            .entries
            .iter()
            .enumerate()
            .find(|(index, (id, _))| !self.taken[*index] && id == call)
            .map(|(index, _)| index);
        match (found, &self.from) {
            (Some(index), Some(from)) => {
                self.taken[index] = true;
                Some((self.entries[index].1.clone(), from.clone()))
            }
            _ => {
                self.latched_off = true;
                None
            }
        }
    }
}

/// The stable identity of an `agent()` call: FNV-1a 64 over label, prompt and canonical
/// opts. Written here because `DefaultHasher` is not stable across builds, and a call id
/// must match across processes and compiler versions for replay to work.
pub(crate) fn call_id(label: &str, prompt: &str, opts: &Value) -> CallId {
    let opts = canonical_json(opts);
    let hash = fnv1a64(&[
        label.as_bytes(),
        b"\0",
        prompt.as_bytes(),
        b"\0",
        opts.as_bytes(),
    ]);
    CallId(format!("{hash:016x}"))
}

/// The `Started` line's script digest: the same FNV, so no hashing crate is needed for a
/// "did the script change" hint that is never a security boundary.
pub(crate) fn script_hash(script: &str) -> String {
    format!("{:016x}", fnv1a64(&[script.as_bytes()]))
}

fn fnv1a64(parts: &[&[u8]]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in parts.iter().flat_map(|part| part.iter()) {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

pub(crate) use p1_json_order::canonical_json;

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct PartialOutput(File);
    impl Write for PartialOutput {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.write_all(&bytes[..bytes.len().min(3)])?;
            Err(std::io::Error::other("injected partial write"))
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    impl JournalOutput for PartialOutput {
        fn durable(&mut self) -> std::io::Result<()> {
            self.0.sync_data()
        }
    }

    #[test]
    fn partial_result_or_ended_write_poison_prevents_any_later_append() {
        for result in [true, false] {
            let journal = TempJournal::new(if result {
                "partial-result"
            } else {
                "partial-ended"
            });
            std::fs::write(
                &journal.0,
                format!("{}\n", serde_json::to_string(&started(1)).unwrap()),
            )
            .unwrap();
            let output = OpenOptions::new().append(true).open(&journal.0).unwrap();
            let writer = JournalWriter {
                file: Mutex::new(Some(Box::new(PartialOutput(output)))),
            };
            let record = if result {
                JournalRecord::Result {
                    call: CallId("c".into()),
                    envelope: StepEnvelope {
                        step: CallId("c".into()),
                        label: None,
                        status: StepStatus::Failed,
                        value: Value::Null,
                        schema: crate::api::SchemaCheck::NotRequested,
                        evidence: None,
                        attempts: 0,
                        worker: None,
                        needs: None,
                        error: None,
                        models: Vec::new(),
                        worktree: None,
                    },
                }
            } else {
                JournalRecord::Ended {
                    outcome: crate::api::RunOutcome::Completed,
                    counts: crate::api::Counts::default(),
                    error: None,
                }
            };
            assert!(writer.append(&record).is_err());
            let size = std::fs::metadata(&journal.0).unwrap().len();
            assert!(
                writer
                    .append(&JournalRecord::Phase {
                        name: "after".into()
                    })
                    .is_err()
            );
            assert_eq!(std::fs::metadata(&journal.0).unwrap().len(), size);
            assert_eq!(read_journal(&journal.0).unwrap(), vec![started(1)]);
        }
    }

    #[test]
    fn canonical_json_sorts_keys_recursively_without_spaces() {
        let value = json!({"b": [1, {"z": 1, "a": "x y"}], "a": null});
        assert_eq!(
            canonical_json(&value),
            r#"{"a":null,"b":[1,{"a":"x y","z":1}]}"#
        );
    }

    #[test]
    fn the_call_id_is_the_fnv_of_label_prompt_and_opts() {
        // Published FNV-1a 64 test vectors.
        assert_eq!(fnv1a64(&[]), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv1a64(&[b"a"]), 0xaf63_dc4c_8601_ec8c);
        assert_eq!(fnv1a64(&[b"foo", b"bar"]), 0x85944171f73967e8);
        let id = call_id("", "", &json!({}));
        assert_eq!(id.0.len(), 16);
        assert_eq!(id, call_id("", "", &json!({})));
        assert_ne!(id, call_id("x", "", &json!({})));
        assert_ne!(
            call_id("a", "b", &json!({})),
            call_id("", "a\0b", &json!({}))
        );
        assert_eq!(
            call_id("l", "p", &json!({"b": 1, "a": 2})),
            call_id("l", "p", &json!({"a": 2, "b": 1}))
        );
    }

    #[test]
    fn replay_matches_by_content_and_latches_off_at_the_first_miss() {
        let envelope = |id: &str| StepEnvelope {
            step: CallId(id.into()),
            label: None,
            status: StepStatus::Done,
            value: json!(id),
            schema: crate::api::SchemaCheck::NotRequested,
            evidence: None,
            attempts: 1,
            worker: None,
            needs: None,
            error: None,
            models: Vec::new(),
            worktree: None,
        };
        let records: Vec<JournalRecord> = ["a", "b", "c"]
            .iter()
            .map(|id| JournalRecord::Result {
                call: CallId(id.to_string()),
                envelope: envelope(id),
            })
            .collect();
        let mut replay = Replay::from_records(RunId("wf1".into()), &records);
        assert!(replay.take(&CallId("b".into())).is_some());
        assert!(replay.take(&CallId("a".into())).is_some());
        assert!(
            replay.take(&CallId("a".into())).is_none(),
            "taken once only"
        );
        assert!(replay.take(&CallId("c".into())).is_none(), "latched off");
    }

    /// A journal file of its own under the temp dir, removed on drop.
    struct TempJournal(std::path::PathBuf);

    impl TempJournal {
        fn new(case: &str) -> Self {
            let path = std::env::temp_dir().join(format!(
                "p1-workflow-journal-{}-{case}.jsonl",
                std::process::id()
            ));
            let _ = std::fs::remove_file(&path);
            Self(path)
        }

        fn write(case: &str, text: &str) -> Self {
            let journal = Self::new(case);
            std::fs::write(&journal.0, text).unwrap();
            journal
        }
    }

    impl Drop for TempJournal {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    fn started(journal_version: u32) -> JournalRecord {
        JournalRecord::Started {
            journal_version,
            run: RunId("wf1".into()),
            script_hash: "00".into(),
            args: json!({}),
            resumed_from: None,
            base: None,
            inherited_charges: BTreeMap::new(),
        }
    }

    const PHASE: &str = r#"{"kind":"phase","name":"p"}"#;

    #[test]
    fn the_version_record_is_written_first_and_read_back() {
        let journal = TempJournal::new("written");
        let writer = JournalWriter::create(&journal.0).unwrap();
        writer.append(&started(WORKFLOW_JOURNAL_VERSION)).unwrap();
        let text = std::fs::read_to_string(&journal.0).unwrap();
        assert!(
            text.starts_with(r#"{"kind":"started","p1_workflow_journal":1,"run":"wf1","#),
            "{text}"
        );
        assert_eq!(
            read_journal(&journal.0).unwrap(),
            [started(WORKFLOW_JOURNAL_VERSION)]
        );
    }

    #[test]
    fn a_journal_without_the_version_record_reads_as_version_1() {
        let journal = TempJournal::write(
            "unversioned",
            &format!(
                "{}\n{PHASE}\n",
                r#"{"kind":"started","run":"wf1","script_hash":"00","args":{},"resumed_from":null}"#
            ),
        );
        let records = read_journal(&journal.0).unwrap();
        assert_eq!(records.len(), 2);
        assert_eq!(records[0], started(1));
    }

    #[test]
    fn a_locked_journal_is_read_through_its_handle_not_the_pathname() {
        let locked = TempJournal::new("locked-handle");
        let moved = TempJournal::new("locked-handle-moved");
        std::fs::write(
            &locked.0,
            format!("{}\n", serde_json::to_string(&started(1)).unwrap()),
        )
        .unwrap();
        let file = File::open(&locked.0).unwrap();
        file.try_lock().unwrap();
        // A concurrent actor replaces the pathname with another valid journal.
        std::fs::rename(&locked.0, &moved.0).unwrap();
        std::fs::write(&locked.0, format!("{PHASE}\n")).unwrap();
        assert_eq!(
            read_journal_file(&file, &locked.0.display().to_string()).unwrap(),
            vec![started(1)],
            "the locked handle still names the journal that was locked"
        );
        assert!(
            matches!(
                read_journal(&locked.0).unwrap().as_slice(),
                [JournalRecord::Phase { name }] if name == "p"
            ),
            "a fresh path open sees the replacement"
        );
    }

    #[test]
    fn torn_tail_is_never_replayed_even_when_valid_json_or_invalid_utf8() {
        let prefix = format!("{}\n", serde_json::to_string(&started(1)).unwrap());
        for tail in [PHASE.as_bytes().to_vec(), vec![0xc3]] {
            let journal = TempJournal::new("torn");
            let mut bytes = prefix.as_bytes().to_vec();
            bytes.extend(tail);
            std::fs::write(&journal.0, bytes).unwrap();
            assert_eq!(read_journal(&journal.0).unwrap(), vec![started(1)]);
        }
    }

    #[test]
    fn blank_and_repeated_start_are_rejected() {
        for interior in [
            "\n".to_string(),
            format!("{}\n", serde_json::to_string(&started(1)).unwrap()),
        ] {
            let journal = TempJournal::write(
                "corrupt",
                &format!(
                    "{}\n{interior}",
                    serde_json::to_string(&started(1)).unwrap()
                ),
            );
            assert!(read_journal(&journal.0).is_err());
        }
    }

    #[test]
    fn duplicate_done_result_and_orphan_failed_attempt_are_rejected() {
        let mut envelope = StepEnvelope {
            step: CallId("c".into()),
            label: None,
            status: StepStatus::Done,
            value: json!("ok"),
            schema: crate::api::SchemaCheck::NotRequested,
            evidence: None,
            attempts: 1,
            worker: None,
            needs: None,
            error: None,
            models: Vec::new(),
            worktree: None,
        };
        let dispatch = JournalRecord::Dispatch {
            call: CallId("c".into()),
            label: None,
            role: "r".into(),
            model: "m".into(),
            wire_model: "m".into(),
            attempt: 1,
            prompt: "p".into(),
            opts: json!({}),
        };
        let done = JournalRecord::Result {
            call: CallId("c".into()),
            envelope: envelope.clone(),
        };
        let prefix = [started(1), dispatch, done.clone()];
        let journal = TempJournal::new("duplicate-result");
        let serialized = |records: &[JournalRecord]| {
            records
                .iter()
                .map(|record| format!("{}\n", serde_json::to_string(record).unwrap()))
                .collect::<String>()
        };
        std::fs::write(
            &journal.0,
            serialized(&[prefix.as_slice(), &[done]].concat()),
        )
        .unwrap();
        assert!(read_journal(&journal.0).is_err());
        envelope.status = StepStatus::Failed;
        std::fs::write(
            &journal.0,
            serialized(&[
                started(1),
                JournalRecord::Result {
                    call: CallId("c".into()),
                    envelope,
                },
            ]),
        )
        .unwrap();
        assert!(read_journal(&journal.0).is_err());
    }

    #[test]
    fn chained_resume_carries_inherited_charges_without_double_counting() {
        let mut first = started(1);
        if let JournalRecord::Started {
            inherited_charges, ..
        } = &mut first
        {
            inherited_charges.insert("model".into(), 3);
        }
        let records = [
            first,
            JournalRecord::Dispatch {
                call: CallId("c".into()),
                label: None,
                role: "r".into(),
                model: "model".into(),
                wire_model: "model".into(),
                attempt: 1,
                prompt: "p".into(),
                opts: json!({}),
            },
        ];
        assert_eq!(dispatch_charges(&records).unwrap()["model"], 4);
    }

    #[test]
    fn oversized_journal_is_refused_before_ingestion() {
        let journal = TempJournal::new("huge");
        let file = File::create(&journal.0).unwrap();
        file.set_len(256 * 1024 * 1024 + 1).unwrap();
        assert!(read_journal(&journal.0).unwrap_err().contains("size limit"));
    }

    #[test]
    fn an_unknown_or_newer_version_is_rejected_never_skipped() {
        for version in ["2", "0", "99", "\"1\"", "null", "1.5"] {
            let journal = TempJournal::write(
                "unknown",
                &format!(
                    "{{\"kind\":\"started\",\"p1_workflow_journal\":{version},\"run\":\"wf1\",\"script_hash\":\"00\",\"args\":{{}},\"resumed_from\":null}}\n{PHASE}\n"
                ),
            );
            let error = read_journal(&journal.0).unwrap_err();
            assert!(
                error.contains(&format!(
                    "line 1: workflow journal version {version} is not one this build reads (it reads version 1)"
                )),
                "{version}: {error}"
            );
        }

        // The leading line is checked before it is read as a record: a newer format whose
        // `started` this build could not even parse is still refused for its version.
        let newer = TempJournal::write(
            "newer",
            &format!("{{\"kind\":\"begun\",\"p1_workflow_journal\":2}}\n{PHASE}\n"),
        );
        let error = read_journal(&newer.0).unwrap_err();
        assert!(error.contains("workflow journal version 2"), "{error}");

        // A `started` record past the first line names a version too.
        let later = TempJournal::write(
            "later",
            &format!(
                "{}\n{}\n",
                serde_json::to_string(&started(1)).unwrap(),
                serde_json::to_string(&started(2)).unwrap()
            ),
        );
        let error = read_journal(&later.0).unwrap_err();
        assert!(
            error.contains("line 2: workflow journal version 2"),
            "{error}"
        );
    }
}
