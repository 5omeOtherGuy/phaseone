//! Lexicographic JSON object order, independent of serde_json feature unification.

use serde_json::{Map, Value};

/// Sort object keys recursively, preserving array order and scalar values.
pub fn canonicalize(mut value: Value) -> Value {
    value.sort_all_objects();
    value
}

/// Iterate object entries in ascending lexicographic key order.
pub fn sorted_entries(map: &Map<String, Value>) -> impl Iterator<Item = (&String, &Value)> {
    let mut entries: Vec<_> = map.iter().collect();
    entries.sort_unstable_by_key(|(left, _)| *left);
    entries.into_iter()
}

/// JSON with object keys sorted at every level and no whitespace.
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Array(items) => {
            out.push('[');
            for (index, item) in items.iter().enumerate() {
                if index > 0 {
                    out.push(',');
                }
                write_canonical(item, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            out.push('{');
            for (index, (key, value)) in sorted_entries(map).enumerate() {
                if index > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(key.clone()).to_string());
                out.push(':');
                write_canonical(value, out);
            }
            out.push('}');
        }
        scalar => out.push_str(&scalar.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sorts_nested_objects_but_not_arrays_and_preserves_json_spelling() {
        let value: Value =
            serde_json::from_str(r#"{"z":[{"β":true,"a\"":null},2,1],"a":{"z":1.5,"a":"line\n"}}"#)
                .unwrap();
        let expected = r#"{"a":{"a":"line\n","z":1.5},"z":[{"a\"":null,"β":true},2,1]}"#;
        assert_eq!(canonical_json(&value), expected);
        assert_eq!(canonicalize(value.clone()).to_string(), expected);
        assert_eq!(canonicalize(value.clone()), value);
        assert_eq!(
            sorted_entries(value.as_object().unwrap())
                .map(|(key, _)| key.as_str())
                .collect::<Vec<_>>(),
            ["a", "z"]
        );
    }
}
