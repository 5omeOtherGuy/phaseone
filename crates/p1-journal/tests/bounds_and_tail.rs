use p1_journal::{JournalError, JsonlJournal, SyncPolicy, load, repair_truncated_tail};

#[test]
fn equal_length_replaced_tail_is_stale() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    std::fs::write(&path, b"{\"p1_journal\":2}\npartial-A").unwrap();
    let tail = load(&path).unwrap().truncated_tail.unwrap();
    std::fs::write(&path, b"{\"p1_journal\":2}\npartial-B").unwrap();
    assert_eq!(
        repair_truncated_tail(&path, &tail),
        Err(JournalError::StaleTail)
    );
    assert!(std::fs::read(&path).unwrap().ends_with(b"partial-B"));
}

#[test]
fn sparse_oversize_journal_is_rejected_by_every_reader() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("session.jsonl");
    let file = std::fs::File::create(&path).unwrap();
    file.set_len(256 * 1024 * 1024 + 1).unwrap();
    drop(file);
    assert!(load(&path).unwrap_err().to_string().contains("size limit"));
    assert!(JsonlJournal::resume(&path, SyncPolicy::OsBuffered).is_err());
    assert!(JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 0).is_err());
    assert!(
        repair_truncated_tail(
            &path,
            &p1_journal::TruncatedTail {
                byte_offset: 0,
                bytes: 0,
                fingerprint: 0,
            }
        )
        .is_err()
    );
}
