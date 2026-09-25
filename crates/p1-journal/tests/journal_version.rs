//! Journal format version 2: the header new files get, version 1 still read and
//! appended to as version 1, and the assembly identity line that only version 2
//! carries. All files live under `tempfile` dirs.

use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{CommitSink, JournalRecord, RecordBody};
use p1_journal::{
    AssemblyEntry, AssemblyIdentity, HostIdentity, JournalError, JsonlJournal, MemoryJournal,
    ModuleIdentity, ModuleKind, SyncPolicy, load, repair_truncated_tail,
};
use tempfile::TempDir;

const V1_HEADER: &str = "{\"p1_journal\":1}\n";
const V2_HEADER: &str = "{\"p1_journal\":2}\n";

fn user(seq: u64, text: &str) -> JournalRecord {
    JournalRecord {
        seq,
        body: RecordBody::UserInput { text: text.into() },
    }
}

fn record_line(record: &JournalRecord) -> String {
    let mut line = serde_json::to_string(record).unwrap();
    line.push('\n');
    line
}

fn identity(environment: &str, digest: &str) -> AssemblyIdentity {
    AssemblyIdentity {
        environment: environment.into(),
        host: HostIdentity {
            version: "0.1.0".into(),
            commit: "07881e9".into(),
        },
        modules: vec![
            ModuleIdentity {
                name: "read".into(),
                kind: ModuleKind::Tool,
                package: "p1-tool-read".into(),
                version: "1.2.0".into(),
                digest: Some(digest.into()),
                abi: Some("p1-module/1".into()),
            },
            ModuleIdentity {
                name: "anthropic".into(),
                kind: ModuleKind::Provider,
                package: "p1-provider-anthropic".into(),
                version: "0.1.0".into(),
                digest: None,
                abi: None,
            },
            ModuleIdentity {
                name: "compact".into(),
                kind: ModuleKind::ContextPolicy,
                package: "p1-context".into(),
                version: "0.1.0".into(),
                digest: None,
                abi: None,
            },
            ModuleIdentity {
                name: "ask".into(),
                kind: ModuleKind::AuthorizationPolicy,
                package: "p1-policy".into(),
                version: "0.1.0".into(),
                digest: None,
                abi: None,
            },
        ],
    }
}

fn assembly_line(identity: &AssemblyIdentity) -> String {
    let mut line = serde_json::to_string(&json!({ "assembly": identity })).unwrap();
    line.push('\n');
    line
}

async fn commit(sink: &impl CommitSink, record: &JournalRecord) {
    sink.commit(record).await.expect("commit succeeds");
}

fn first_line(path: &std::path::Path) -> String {
    let text = std::fs::read_to_string(path).unwrap();
    format!("{}\n", text.lines().next().unwrap())
}

/// The released binaries' header check, verbatim in effect: `p1_journal` must be
/// exactly 1. Slice S1.9 runs the real old binary against a version-2 file.
fn released_binary_accepts(header: &str) -> bool {
    let value: Value = serde_json::from_str(header).unwrap();
    value.get("p1_journal").and_then(Value::as_u64) == Some(1)
}

#[tokio::test]
async fn create_writes_version_2() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("new.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::EveryRecord).unwrap();
    commit(&journal, &user(0, "a")).await;
    drop(journal);

    assert_eq!(first_line(&path), V2_HEADER);
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.records, vec![user(0, "a")]);
    assert!(loaded.assemblies.is_empty());
    assert_eq!(loaded.truncated_tail, None);
}

#[tokio::test]
async fn version_1_file_loads_resumes_and_appends_as_version_1() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("old.jsonl");
    let written = format!(
        "{V1_HEADER}{}{}",
        record_line(&user(0, "a")),
        record_line(&user(1, "b"))
    );
    std::fs::write(&path, &written).unwrap();

    let loaded = load(&path).unwrap();
    assert_eq!(loaded.version, 1);
    assert_eq!(loaded.records, vec![user(0, "a"), user(1, "b")]);
    assert!(loaded.assemblies.is_empty());

    let (writer, resumed) = JsonlJournal::resume(&path, SyncPolicy::EveryRecord).unwrap();
    assert_eq!(resumed.version, 1);
    assert_eq!(resumed.records, loaded.records);
    assert!(resumed.assemblies.is_empty());
    assert_eq!(resumed.repaired_tail, None);
    commit(&writer, &user(2, "c")).await;
    // Writing an assembly line would make the file unreadable as version 1.
    let before = std::fs::read(&path).unwrap();
    assert_eq!(
        writer.record_assembly(&identity("default", "aa")),
        Err(JournalError::AssemblyNeedsVersion2)
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
    drop(writer);

    let writer = JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 3).unwrap();
    commit(&writer, &user(3, "d")).await;
    assert_eq!(
        writer.record_assembly(&identity("default", "aa")),
        Err(JournalError::AssemblyNeedsVersion2)
    );
    drop(writer);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(text.starts_with(&written), "appends only, never rewrites");
    assert_eq!(first_line(&path), V1_HEADER);
    assert_eq!(text.matches("p1_journal").count(), 1);
    let reloaded = load(&path).unwrap();
    assert_eq!(reloaded.version, 1);
    assert_eq!(
        reloaded.records,
        vec![user(0, "a"), user(1, "b"), user(2, "c"), user(3, "d")]
    );
    assert!(released_binary_accepts(&first_line(&path)));
}

#[tokio::test]
async fn version_3_is_unknown() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("future.jsonl");
    std::fs::write(
        &path,
        format!("{{\"p1_journal\":3}}\n{}", record_line(&user(0, "a"))),
    )
    .unwrap();
    let before = std::fs::read(&path).unwrap();
    assert_eq!(load(&path).unwrap_err(), JournalError::UnknownVersion);
    assert_eq!(
        JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap_err(),
        JournalError::UnknownVersion
    );
    assert_eq!(
        JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 1).unwrap_err(),
        JournalError::UnknownVersion
    );
    assert_eq!(std::fs::read(&path).unwrap(), before);
}

#[tokio::test]
async fn assembly_lines_round_trip_with_their_from_seq() {
    let first = identity("default", "11");
    let second = identity("switched", "22");
    let last = identity("reconfigured", "33");
    let expected = vec![
        AssemblyEntry {
            from_seq: 0,
            identity: first.clone(),
        },
        AssemblyEntry {
            from_seq: 2,
            identity: second.clone(),
        },
        AssemblyEntry {
            from_seq: 3,
            identity: last.clone(),
        },
    ];
    let records = vec![user(0, "a"), user(1, "b"), user(2, "c")];

    // File store: before the first record, between records, after the last.
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("assembly.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::EveryRecord).unwrap();
    journal.record_assembly(&first).unwrap();
    commit(&journal, &records[0]).await;
    commit(&journal, &records[1]).await;
    journal.record_assembly(&second).unwrap();
    commit(&journal, &records[2]).await;
    journal.record_assembly(&last).unwrap();
    drop(journal);

    let text = std::fs::read_to_string(&path).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 1 + 3 + 3);
    let raw: Value = serde_json::from_str(lines[1]).unwrap();
    assert_eq!(
        raw,
        serde_json::from_str::<Value>(&assembly_line(&first)).unwrap()
    );
    assert_eq!(raw["assembly"]["modules"][0]["kind"], "tool");
    assert_eq!(raw["assembly"]["modules"][2]["kind"], "context_policy");
    assert_eq!(
        raw["assembly"]["modules"][3]["kind"],
        "authorization_policy"
    );
    assert!(raw.get("seq").is_none(), "an assembly line carries no seq");

    let loaded = load(&path).unwrap();
    assert_eq!(loaded.version, 2);
    assert_eq!(loaded.truncated_tail, None);
    assert_eq!(loaded.records, records);
    assert_eq!(loaded.assemblies, expected);

    // Resume sees the same, and the dense seq continues past the trailing line.
    let (writer, resumed) = JsonlJournal::resume(&path, SyncPolicy::EveryRecord).unwrap();
    assert_eq!(resumed.version, 2);
    assert_eq!(resumed.records, records);
    assert_eq!(resumed.assemblies, expected);
    commit(&writer, &user(3, "d")).await;
    drop(writer);
    let reloaded = load(&path).unwrap();
    assert_eq!(reloaded.records.len(), 4);
    assert_eq!(reloaded.assemblies, expected);

    // Memory store: the same calls give the same entries.
    let memory = MemoryJournal::new();
    memory.record_assembly(&first).unwrap();
    commit(&memory, &records[0]).await;
    commit(&memory, &records[1]).await;
    memory.record_assembly(&second).unwrap();
    commit(&memory, &records[2]).await;
    memory.record_assembly(&last).unwrap();
    assert_eq!(memory.records(), records);
    assert_eq!(memory.assemblies(), expected);
}

#[tokio::test]
async fn assembly_line_in_a_version_1_file_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("v1-assembly.jsonl");
    let line = assembly_line(&identity("default", "aa"));

    // Between records.
    std::fs::write(
        &path,
        format!(
            "{V1_HEADER}{}{line}{}",
            record_line(&user(0, "a")),
            record_line(&user(1, "b"))
        ),
    )
    .unwrap();
    let error = load(&path).unwrap_err();
    assert_eq!(error, JournalError::AssemblyInVersion1 { line: 3 });
    assert!(error.to_string().contains("version-1"), "{error}");
    assert_eq!(
        JsonlJournal::resume(&path, SyncPolicy::OsBuffered).unwrap_err(),
        JournalError::AssemblyInVersion1 { line: 3 }
    );

    // As the last line it is still a valid line of the wrong version, not a tail.
    std::fs::write(
        &path,
        format!("{V1_HEADER}{}{line}", record_line(&user(0, "a"))),
    )
    .unwrap();
    assert_eq!(
        load(&path).unwrap_err(),
        JournalError::AssemblyInVersion1 { line: 3 }
    );
    assert_eq!(
        JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 1).unwrap_err(),
        JournalError::AssemblyInVersion1 { line: 3 }
    );
}

#[tokio::test]
async fn assembly_line_with_an_unknown_field_is_refused() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("unknown-field.jsonl");
    let valid: Value = serde_json::to_value(identity("default", "aa")).unwrap();

    let mut in_identity = valid.clone();
    in_identity["extra"] = json!(1);
    let mut in_host = valid.clone();
    in_host["host"]["build"] = json!("x");
    let mut in_module = valid.clone();
    in_module["modules"][0]["signature"] = json!("x");
    let mut unknown_kind = valid.clone();
    unknown_kind["modules"][0]["kind"] = json!("hook");
    let variants = [
        json!({ "assembly": in_identity }),
        json!({ "assembly": in_host }),
        json!({ "assembly": in_module }),
        json!({ "assembly": unknown_kind }),
        json!({ "assembly": valid, "seq": 1 }),
    ];
    for variant in variants {
        let bad = serde_json::to_string(&variant).unwrap();
        std::fs::write(
            &path,
            format!(
                "{V2_HEADER}{}{bad}\n{}",
                record_line(&user(0, "a")),
                record_line(&user(1, "b"))
            ),
        )
        .unwrap();
        assert_eq!(
            load(&path).unwrap_err(),
            JournalError::Corrupt { line: 3 },
            "{bad}"
        );
    }
}

#[tokio::test]
async fn torn_assembly_line_is_a_truncated_tail_and_repairs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("torn.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    journal.record_assembly(&identity("default", "11")).unwrap();
    commit(&journal, &user(0, "a")).await;
    commit(&journal, &user(1, "b")).await;
    let clean = std::fs::read(&path).unwrap();
    journal
        .record_assembly(&identity("switched", "22"))
        .unwrap();
    drop(journal);
    let full = std::fs::read(&path).unwrap();

    // Cut the last assembly line at every byte short of its newline.
    for len in clean.len() + 1..full.len() {
        std::fs::write(&path, &full[..len]).unwrap();
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.records, vec![user(0, "a"), user(1, "b")], "at {len}");
        assert_eq!(loaded.assemblies.len(), 1, "at {len}");
        let tail = loaded
            .truncated_tail
            .expect("a torn assembly line is the tail");
        assert_eq!(tail.byte_offset as usize, clean.len(), "at {len}");
        assert_eq!(tail.bytes as usize, len - clean.len(), "at {len}");

        repair_truncated_tail(&path, &tail).unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), clean, "at {len}");
    }

    // The repaired file continues normally, assembly lines included.
    let writer = JsonlJournal::open_for_append(&path, SyncPolicy::OsBuffered, 2).unwrap();
    writer.record_assembly(&identity("switched", "22")).unwrap();
    commit(&writer, &user(2, "c")).await;
    drop(writer);
    let loaded = load(&path).unwrap();
    assert_eq!(loaded.truncated_tail, None);
    assert_eq!(loaded.records.len(), 3);
    assert_eq!(
        loaded.assemblies[1],
        AssemblyEntry {
            from_seq: 2,
            identity: identity("switched", "22"),
        }
    );
}

#[tokio::test]
async fn released_binaries_refuse_a_version_2_header() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("refused.jsonl");
    drop(JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap());
    let header = first_line(&path);
    assert_eq!(header, V2_HEADER);
    assert!(!released_binary_accepts(&header));
    assert!(released_binary_accepts(V1_HEADER));
}
