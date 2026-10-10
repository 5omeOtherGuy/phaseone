#[test]
fn serde_json_maps_are_sorted() {
    let mut object = serde_json::Map::new();
    object.insert("z".into(), serde_json::Value::Null);
    object.insert("a".into(), serde_json::Value::Null);
    assert_eq!(
        object.keys().map(String::as_str).collect::<Vec<_>>(),
        ["a", "z"],
        "a dependency enabled serde_json/preserve_order; see https://github.com/5omeOtherGuy/phaseone/issues/689"
    );
}
