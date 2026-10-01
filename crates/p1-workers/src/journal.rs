//! Reading back which workers a journalled session started.
//!
//! Workers live in the process that started them, so after a resume their ids name
//! nothing; the host uses [`workers_started_in`] to say so and to keep new ids from
//! colliding with them. The scanner is pure: it reads records and the `worker_start`
//! result text, and the caller names the tool identities that text was journalled under
//! (the native delegate tool's, a member package's), so this crate names neither.

use std::collections::HashMap;

use p1_contracts::{JournalRecord, RecordBody, ToolStatus};

/// How a successful `worker_start` result begins; [`workers_started_in`] reads it back.
pub const STARTED_PREFIX: &str = "Started worker ";

/// The ids of every worker a journalled session started, in order: the successful results
/// of the calls whose `ToolStarted` identity is one of `implementations`.
///
/// A call id names only the occurrence started last under it, and its finish consumes it:
/// a provider may reuse an id later in the session (`call_0`), and a result of another tool
/// under a reused id, or a second finish, must not read as a worker start.
pub fn workers_started_in(records: &[JournalRecord], implementations: &[&str]) -> Vec<String> {
    // Outstanding calls by id: whether the occurrence started last is a worker start.
    let mut outstanding: HashMap<&str, bool> = HashMap::new();
    let mut ids = Vec::new();
    for record in records {
        match &record.body {
            RecordBody::ToolStarted { call_id, identity } => {
                let delegate = implementations.contains(&identity.implementation.as_str());
                outstanding.insert(call_id.as_str(), delegate);
            }
            RecordBody::ToolFinished { result, .. } => {
                let delegate = outstanding.remove(result.call_id.as_str()) == Some(true);
                if !delegate || result.status != ToolStatus::Ok {
                    continue;
                }
                let id = result
                    .content
                    .strip_prefix(STARTED_PREFIX)
                    .and_then(|rest| rest.split(' ').next())
                    .filter(|id| !id.is_empty());
                if let Some(id) = id {
                    ids.push(id.to_string());
                }
            }
            _ => {}
        }
    }
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::{ToolIdentity, ToolResultItem};

    fn started(seq: u64, call_id: &str, implementation: &str) -> JournalRecord {
        JournalRecord {
            seq,
            body: RecordBody::ToolStarted {
                call_id: call_id.to_owned(),
                identity: ToolIdentity {
                    implementation: implementation.to_owned(),
                    variant: "default".to_owned(),
                },
            },
        }
    }

    fn finished(seq: u64, call_id: &str, status: ToolStatus, content: &str) -> JournalRecord {
        JournalRecord {
            seq,
            body: RecordBody::ToolFinished {
                result: ToolResultItem {
                    call_id: call_id.to_owned(),
                    name: "worker_start".to_owned(),
                    status,
                    content: content.to_owned(),
                },
                exit_code: None,
            },
        }
    }

    #[test]
    fn reads_the_ids_every_listed_identity_started_in_order() {
        let records = [
            started(0, "c1", "p1-tool-delegate"),
            finished(
                1,
                "c1",
                ToolStatus::Ok,
                "Started worker w1 on r/m with tools: finish.",
            ),
            started(2, "c2", "p1/worker-start"),
            finished(
                3,
                "c2",
                ToolStatus::Ok,
                "Started worker w2 on r/m with tools: finish.",
            ),
        ];
        assert_eq!(
            workers_started_in(&records, &["p1-tool-delegate", "p1/worker-start"]),
            ["w1", "w2"]
        );
        assert_eq!(workers_started_in(&records, &["p1/worker-start"]), ["w2"]);
    }

    #[test]
    fn ignores_other_tools_failed_starts_and_other_texts() {
        let records = [
            started(0, "c1", "p1-tool-shell"),
            finished(1, "c1", ToolStatus::Ok, "Started worker w9 on r/m"),
            started(2, "c2", "p1/worker-start"),
            finished(3, "c2", ToolStatus::Error, "Started worker w8 on r/m"),
            started(4, "c3", "p1/worker-result"),
            finished(5, "c3", ToolStatus::Ok, "w1: finished"),
        ];
        assert!(workers_started_in(&records, &["p1/worker-start", "p1/worker-result"]).is_empty());
    }

    #[test]
    fn a_call_id_resolves_only_its_own_later_result() {
        let records = [
            started(0, "c1", "p1/worker-start"),
            finished(1, "c1", ToolStatus::Ok, "Started worker w1 on r/m"),
            // A second finish of the same occurrence is not another start.
            finished(2, "c1", ToolStatus::Ok, "Started worker w7 on r/m"),
            // The provider reuses c1 for a shell call whose output looks like a start.
            started(3, "c1", "p1-tool-shell"),
            finished(4, "c1", ToolStatus::Ok, "Started worker w999 on r/m"),
            // A worker start whose text names no id.
            started(5, "c2", "p1/worker-start"),
            finished(6, "c2", ToolStatus::Ok, "Started worker  on r/m"),
            // A reused id started as a shell call, then again as a worker start: the
            // latest occurrence decides.
            started(7, "c3", "p1-tool-shell"),
            started(8, "c3", "p1/worker-start"),
            finished(9, "c3", ToolStatus::Ok, "Started worker w2 on r/m"),
        ];
        assert_eq!(
            workers_started_in(&records, &["p1/worker-start"]),
            ["w1", "w2"]
        );
    }
}
