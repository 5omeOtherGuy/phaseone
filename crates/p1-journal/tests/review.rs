use p1_journal::{JsonlJournal, SyncPolicy, TruncatedTail, repair_truncated_tail};
#[test]
fn review_repair_respects_active_writer_lock() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let _writer = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    let before = std::fs::read(&path).unwrap();
    let result = repair_truncated_tail(&path, &TruncatedTail { byte_offset: 0, bytes: before.len() as u64 });
    let after = std::fs::read(&path).unwrap();
    assert!(result.is_err() && before == after, "repair modified an actively locked journal: before={} after={} result={result:?}", before.len(), after.len());
}
