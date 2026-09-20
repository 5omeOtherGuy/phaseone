//! Lead checks for review finding R6: a session file is read, validated, repaired
//! and appended to only by whoever holds its lock, and never from a stale
//! observation.

use std::io::Write;
use std::path::Path;

use p1_contracts::{CommitSink, JournalRecord, RecordBody};
use p1_journal::{JournalError, JsonlJournal, SyncPolicy, load, repair_truncated_tail};

fn user(seq: u64, text: &str) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::UserInput { text: text.into() },
    }
}

/// A closed session file holding `count` records.
async fn session_with(path: &Path, count: u64) {
    let journal = JsonlJournal::create(path, SyncPolicy::OsBuffered).unwrap();
    for seq in 0..count {
        journal.commit(&user(seq, "text")).await.unwrap();
    }
}

fn append_bytes(path: &Path, bytes: &[u8]) {
    let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
    file.write_all(bytes).unwrap();
}

#[tokio::test]
async fn resume_refuses_a_file_another_writer_owns_and_leaves_it_alone() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let writer = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    writer.commit(&user(0, "a")).await.unwrap();
    // A partial line, as a second process would see it mid-write.
    append_bytes(&path, b"{\"seq\":1,\"bo");
    let before = std::fs::read(&path).unwrap();

    let error = JsonlJournal::resume(&path, SyncPolicy::OsBuffered)
        .map(|_| ())
        .unwrap_err();

    assert_eq!(error, JournalError::Locked);
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn resume_cuts_the_tail_and_continues_at_the_derived_sequence() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    session_with(&path, 3).await;
    append_bytes(&path, b"{\"seq\":3,\"bo");

    let (journal, resumed) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();

    assert_eq!(resumed.records.len(), 3);
    assert_eq!(resumed.repaired_tail.map(|tail| tail.bytes), Some(12));
    // The writer continues exactly where the complete records end…
    assert!(matches!(
        journal.commit(&user(4, "gap")).await,
        Err(error) if error.to_string().contains("expected seq 3")
    ));
    journal.commit(&user(3, "next")).await.unwrap();
    drop(journal);
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.records.len(), 4);
    assert_eq!(loaded.truncated_tail, None);
}

#[tokio::test]
async fn resume_holds_the_lock_for_the_life_of_the_writer() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    session_with(&path, 1).await;

    let (first, _) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    let second = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).map(|_| ());
    assert_eq!(second, Err(JournalError::Locked));
    drop(first);
    assert!(JsonlJournal::resume(&path, SyncPolicy::OsBuffered).is_ok());
}

#[tokio::test]
async fn repair_rejects_a_tail_that_is_no_longer_there() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    session_with(&path, 2).await;
    append_bytes(&path, b"{\"seq\":2,\"bo");
    let stale = load(&path).unwrap().truncated_tail.unwrap();

    // Between the observation and the repair, the owner finishes that line and
    // commits more. Cutting at the old offset would now delete real records.
    repair_truncated_tail(&path, &stale).unwrap();
    let (owner, _) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    owner.commit(&user(2, "c")).await.unwrap();
    drop(owner);
    let before = std::fs::read(&path).unwrap();

    assert_eq!(
        repair_truncated_tail(&path, &stale),
        Err(JournalError::StaleTail)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn open_for_append_rejects_a_sequence_number_from_a_stale_load() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    session_with(&path, 2).await;
    let stale_next = load(&path).unwrap().records.len() as u64;
    let (owner, _) = JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap();
    owner.commit(&user(2, "c")).await.unwrap();
    drop(owner);

    let result = JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, stale_next);

    assert_eq!(
        result.map(|_| ()),
        Err(JournalError::OutOfOrder {
            expected: 3,
            got: 2
        })
    );
}
