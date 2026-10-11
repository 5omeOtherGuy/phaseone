use p1_contracts::frontend::SessionArchive;
use p1_contracts::{CommitSink, JournalRecord, RecordBody};
use p1_host::session::archive::JournalArchive;

#[tokio::test]
async fn acp_archive_forks_a_live_snapshot_and_resumes_independently() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let other = tempfile::tempdir().unwrap();
    let archive = JournalArchive::new(state.path().join("sessions"));
    let original = archive.create(workspace.path()).unwrap();
    let writer = p1_host::session::create(&original.journal).unwrap();
    let record = JournalRecord {
        seq: 0,
        body: RecordBody::UserInput {
            text: "before fork".into(),
        },
    };
    writer.commit(&record).await.unwrap();
    assert!(p1_host::session::resume(&original.journal).is_err());
    let fork = archive.fork(&original.id, workspace.path()).unwrap();
    assert_ne!(fork.id, original.id);
    writer
        .commit(&JournalRecord {
            seq: 1,
            body: RecordBody::UserInput {
                text: "parent only".into(),
            },
        })
        .await
        .unwrap();
    let (fork_writer, restored) = p1_host::session::resume(&fork.journal).unwrap();
    assert_eq!(restored.records, vec![record]);
    fork_writer
        .commit(&JournalRecord {
            seq: 1,
            body: RecordBody::UserInput {
                text: "fork only".into(),
            },
        })
        .await
        .unwrap();
    drop(fork_writer);
    drop(writer);
    let (_, restored) = p1_host::session::resume(&original.journal).unwrap();
    assert!(
        matches!(&restored.records[1].body, RecordBody::UserInput { text } if text == "parent only")
    );
    let reopened = JournalArchive::new(state.path().join("sessions"));
    std::fs::create_dir(state.path().join("sessions/interrupted-create")).unwrap();
    std::fs::create_dir(state.path().join("sessions/interrupted-metadata")).unwrap();
    std::fs::write(
        state
            .path()
            .join("sessions/interrupted-metadata/workspace.json"),
        "{",
    )
    .unwrap();
    assert_eq!(reopened.list().unwrap().len(), 2);
    assert_eq!(
        reopened.get(&fork.id, workspace.path()).unwrap().journal,
        fork.journal
    );
    assert!(reopened.get(&fork.id, other.path()).is_err());
    assert!(reopened.get("../outside", workspace.path()).is_err());
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&fork.journal)
                .unwrap()
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}

#[test]
fn acp_archive_fork_preserves_format_assemblies_and_time_but_not_a_torn_tail() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let archive = JournalArchive::new(state.path().join("sessions"));
    for version in 1..=3 {
        let source = archive.create(workspace.path()).unwrap();
        let assembly = if version >= 2 {
            "{\"assembly\":{\"environment\":\"fixture\",\"host\":{\"version\":\"test\",\"commit\":\"test\"},\"modules\":[]}}\n"
        } else {
            ""
        };
        let time = if version == 3 { ",\"at_ms\":42" } else { "" };
        let text = format!(
            "{{\"p1_journal\":{version}}}\n{assembly}{{\"seq\":0,\"record\":\"user_input\",\"text\":\"snapshot\"{time}}}\n{assembly}{{\"seq\":1"
        );
        std::fs::write(&source.journal, &text).unwrap();
        let fork = archive.fork(&source.id, workspace.path()).unwrap();
        let loaded = p1_journal::load(&fork.journal).unwrap();
        assert_eq!(loaded.version, version);
        assert_eq!(
            loaded.records,
            vec![JournalRecord {
                seq: 0,
                body: RecordBody::UserInput {
                    text: "snapshot".into()
                }
            }]
        );
        assert_eq!(
            loaded.at_ms,
            vec![if version == 3 { Some(42) } else { None }]
        );
        assert!(loaded.truncated_tail.is_none());
        assert_eq!(
            loaded
                .assemblies
                .iter()
                .map(|a| a.from_seq)
                .collect::<Vec<_>>(),
            if version >= 2 { vec![0, 1] } else { vec![] }
        );
        assert_eq!(std::fs::read_to_string(&source.journal).unwrap(), text);
    }
}

/// #806: a fork keeps each record's masked count (ADR-0184), and a line that had none
/// gets none.
#[test]
fn acp_archive_fork_keeps_masked_counts() {
    let state = tempfile::tempdir().unwrap();
    let workspace = tempfile::tempdir().unwrap();
    let archive = JournalArchive::new(state.path().join("sessions"));
    let source = archive.create(workspace.path()).unwrap();
    std::fs::write(
        &source.journal,
        "{\"p1_journal\":3}\n\
         {\"seq\":0,\"record\":\"user_input\",\"text\":\"first\",\"at_ms\":1,\"masked\":3}\n\
         {\"seq\":1,\"record\":\"user_input\",\"text\":\"second\",\"at_ms\":2}\n",
    )
    .unwrap();
    let fork = archive.fork(&source.id, workspace.path()).unwrap();
    let loaded = p1_journal::load(&fork.journal).unwrap();
    assert_eq!(loaded.masked, vec![3, 0]);
    assert_eq!(loaded.at_ms, vec![Some(1), Some(2)]);
    let text = std::fs::read_to_string(&fork.journal).unwrap();
    assert_eq!(text.matches("\"masked\"").count(), 1, "{text}");
}
