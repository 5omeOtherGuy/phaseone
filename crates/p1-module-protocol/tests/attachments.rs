//! Attachment blocks on the wire (ADR-0180): what a module may send back is checked, and
//! what the host resolved for one request crosses to the module but never into a journal.

use p1_contracts::{Item, JournalRecord, RecordBody};
use p1_module_protocol::WireItem;
use serde_json::json;

const ID: &str = "abababababababababababababababababababababababababababababababab";

#[test]
fn a_path_shaped_attachment_id_is_refused() {
    for id in [
        "../../etc/passwd",
        "ABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABABAB",
        "",
    ] {
        let value = json!({"item": "user_content", "blocks": [{"block": "file",
            "file": {"id": id, "name": "x", "bytes": 1}}]});
        assert!(serde_json::from_value::<WireItem>(value).is_err(), "{id}");
    }
}

#[test]
fn resolved_bytes_cross_to_a_module_but_not_into_a_record() {
    let wire = json!({"item": "user_content", "blocks": [
        {"block": "image", "image": {"id": ID, "media_type": "png", "bytes": 3, "width": 1,
            "height": 1}, "data": "YWJj"},
        {"block": "file", "file": {"id": ID, "name": "n", "bytes": 3}, "path": "/p/objects/x"}
    ]});
    let item = Item::from(serde_json::from_value::<WireItem>(wire.clone()).unwrap());
    // Back to the wire unchanged: a module sees the resolved data and path.
    assert_eq!(
        serde_json::to_value(WireItem::from(item.clone())).unwrap(),
        wire
    );
    // As a journal record the blocks hold references only.
    let Item::UserContent { blocks } = item else {
        panic!("not user content")
    };
    let record = JournalRecord {
        seq: 1,
        body: RecordBody::UserContent { blocks },
    };
    let text = serde_json::to_string(&record).unwrap();
    assert!(
        !text.contains("YWJj") && !text.contains("/p/objects"),
        "{text}"
    );
}
