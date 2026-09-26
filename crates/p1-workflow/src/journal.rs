//! The run journal (ADR-0053 item 6): one `JournalRecord` per line, append-only, and the
//! prefix replay a resumed run reads from its predecessor's journal.

use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

use serde_json::Value;

use crate::api::{CallId, JournalRecord, RunId, StepEnvelope, StepStatus};
use crate::decision::{ReplayEntry, ReplayView};

/// Appends records to `journal.jsonl`. Each record is written with one `write_all` on an
/// unbuffered file, so a crash loses at most the line being written and never reorders.
pub(crate) struct JournalWriter {
    file: Mutex<File>,
}

impl JournalWriter {
    pub(crate) fn create(path: &Path) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(Self {
            file: Mutex::new(file),
        })
    }

    pub(crate) fn append(&self, record: &JournalRecord) -> std::io::Result<()> {
        let mut line = serde_json::to_string(record).map_err(std::io::Error::other)?;
        line.push('\n');
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        file.write_all(line.as_bytes())?;
        file.flush()
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
pub fn read_journal(path: &Path) -> Result<Vec<JournalRecord>, String> {
    let text =
        std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
    let complete = text.ends_with('\n');
    let lines: Vec<&str> = text.lines().collect();
    let mut records = Vec::with_capacity(lines.len());
    for (index, line) in lines.iter().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalRecord>(line) {
            Ok(record) => records.push(record),
            Err(_) if !complete && index + 1 == lines.len() => {}
            Err(error) => {
                return Err(format!("{} line {}: {error}", path.display(), index + 1));
            }
        }
    }
    Ok(records)
}

/// Attempts the old run spent per wire model: every `Dispatch`, matched on resume or not,
/// is charged to the resuming run — nothing is refunded (ADR-0053 item 4).
pub(crate) fn dispatch_charges(records: &[JournalRecord]) -> BTreeMap<String, u32> {
    let mut charges = BTreeMap::new();
    for record in records {
        if let JournalRecord::Dispatch { wire_model, .. } = record {
            *charges.entry(wire_model.clone()).or_insert(0) += 1;
        }
    }
    charges
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

/// JSON with object keys sorted at every level and no whitespace. Sorted explicitly rather
/// than relying on `serde_json::Map` order, which a feature elsewhere in the build can change.
pub(crate) fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (index, key) in keys.into_iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(&map[key], out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

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
}
