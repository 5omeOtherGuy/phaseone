//! Reading back which workers a journalled session started.
//!
//! Workers live in the process that started them, so after a resume their ids name
//! nothing; the host uses [`workers_started_in`] to say so and to keep new ids from
//! colliding with them. The scanner is pure: it reads records and the `worker_start`
//! result text, and the caller names the tool identities that text was journalled under
//! (the native delegate tool's, a member package's), so this crate names neither.

use std::collections::HashSet;

use p1_contracts::{JournalRecord, RecordBody, ToolStatus};

/// How a successful `worker_start` result begins; [`workers_started_in`] reads it back.
pub const STARTED_PREFIX: &str = "Started worker ";

/// The ids of every worker a journalled session started, in order: the successful results
/// of the calls whose `ToolStarted` identity is one of `implementations`.
pub fn workers_started_in(records: &[JournalRecord], implementations: &[&str]) -> Vec<String> {
    let mut delegate_calls = HashSet::new();
    let mut ids = Vec::new();
    for record in records {
        match &record.body {
            RecordBody::ToolStarted { call_id, identity }
                if implementations.contains(&identity.implementation.as_str()) =>
            {
                delegate_calls.insert(call_id.as_str());
            }
            RecordBody::ToolFinished { result }
                if result.status == ToolStatus::Ok
                    && delegate_calls.contains(result.call_id.as_str()) =>
            {
                let id = result
                    .content
                    .strip_prefix(STARTED_PREFIX)
                    .and_then(|rest| rest.split(' ').next());
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
}
