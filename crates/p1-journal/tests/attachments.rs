//! A message with attachments in a JSONL journal (ADR-0180): its record holds references
//! only, even when the item carried resolved bytes and paths, a text-only message keeps
//! its `user_input` record, and both project back into history.

use p1_contracts::{
    AttachmentId, CacheKeySupport, CommitSink, FileRef, ImageData, ImageRef, ImageType, Item,
    JournalRecord, ModelOptions, Origin, RecordBody, RouteDescription, UserBlock,
};
use p1_journal::{JsonlJournal, SyncPolicy, load};

const ID: &str = "abababababababababababababababababababababababababababababababab";

fn environment() -> RecordBody {
    RecordBody::Environment {
        route: RouteDescription {
            origin: Origin {
                route: "r".into(),
                model: "m".into(),
            },
            supports_freeform_tools: false,
            mandatory_prompt_prefix: None,
            reports_cost: false,
            cache_key: CacheKeySupport::Unsupported,
            input_modalities: Vec::new(),
        },
        system_prompt: "p".into(),
        tools: Vec::new(),
        options: ModelOptions::default(),
    }
}

fn blocks(resolved: bool) -> Vec<UserBlock> {
    let id = AttachmentId::new(ID).unwrap();
    vec![
        UserBlock::text("see attached"),
        UserBlock::Image {
            image: ImageRef {
                id: id.clone(),
                media_type: ImageType::Png,
                bytes: 3,
                width: 1,
                height: 1,
            },
            data: resolved.then(|| ImageData::encode(b"abc")),
        },
        UserBlock::File {
            file: FileRef {
                id,
                name: "notes.txt".into(),
                bytes: 5,
            },
            path: resolved.then(|| "/state/attachments/objects/x".to_owned()),
        },
    ]
}

#[tokio::test]
async fn the_record_holds_references_and_text_input_is_unchanged() {
    let scratch = tempfile::tempdir().unwrap();
    let path = scratch.path().join("session.jsonl");
    let journal = JsonlJournal::create(&path, SyncPolicy::OsBuffered).unwrap();
    let records = [
        environment(),
        RecordBody::UserContent {
            blocks: blocks(true),
        },
        RecordBody::UserInput {
            text: "plain".into(),
        },
    ];
    for (seq, body) in records.into_iter().enumerate() {
        let record = JournalRecord {
            seq: seq as u64,
            body,
        };
        journal.commit(&record).await.unwrap();
    }
    drop(journal);

    let text = std::fs::read_to_string(&path).unwrap();
    assert!(
        !text.contains("YWJj"),
        "image bytes reached the journal: {text}"
    );
    assert!(
        !text.contains("/state/attachments"),
        "a path reached the journal: {text}"
    );
    let lines: Vec<&str> = text.lines().collect();
    assert!(lines[2].contains(&format!(
        r#""record":"user_content","blocks":[{{"block":"text","text":"see attached"}},{{"block":"image","image":{{"id":"{ID}","media_type":"png","bytes":3,"width":1,"height":1}}}},{{"block":"file","file":{{"id":"{ID}","name":"notes.txt","bytes":5}}}}]"#
    )), "{}", lines[2]);
    assert!(
        lines[3].contains(r#""seq":2,"record":"user_input","text":"plain""#),
        "{}",
        lines[3]
    );

    let loaded = load(&path).unwrap();
    let projection = p1_core::project(&loaded.records).unwrap();
    assert_eq!(
        projection.history,
        vec![
            Item::UserContent {
                blocks: blocks(false)
            },
            Item::User {
                text: "plain".into()
            },
        ]
    );
}
