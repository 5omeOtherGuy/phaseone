//! The schema bundle and the wire types agree, and both agree with `p1-contracts`.
//!
//! Every fixture under `tests/fixtures/<family>/` must survive
//! JSON → wire type → contract type → wire type → JSON unchanged and validate against
//! `schema/<family>.json`; every fixture under `tests/fixtures/<family>/rejected/` must be
//! refused by both serde and the schema. The validator below implements exactly the
//! keywords the bundle uses and fails on any other, so the bundle cannot quietly start
//! relying on a keyword nothing checks.

use std::collections::BTreeMap;
use std::fmt::Debug;
use std::fs;
use std::path::{Path, PathBuf};

use p1_contracts::tool::ResultDescription;
use p1_contracts::{
    CallDescription, Item, ModelOptions, ProviderError, ReplayData, RouteDescription, StreamEvent,
    ToolCall, ToolOutcome, Usage,
};
use p1_module_protocol::{
    PROTOCOL_VERSION, WireCallDescription, WireItem, WireModelOptions, WireProviderError,
    WireReplayData, WireResultDescription, WireRouteDescription, WireStreamEvent, WireToolCall,
    WireToolOutcome, WireUsage,
};
use serde::Serialize;
use serde::de::DeserializeOwned;
use serde_json::Value;

const FAMILIES: [&str; 11] = [
    "tool-call",
    "tool-outcome",
    "history-item",
    "stream-event",
    "provider-error",
    "call-description",
    "result-description",
    "model-options",
    "route-description",
    "replay-data",
    "usage",
];

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

fn read_json(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    serde_json::from_str(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()))
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|entry| entry.unwrap().path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

// ---------------------------------------------------------------------------------------
// Validator for the keyword subset the bundle uses.

struct Bundle {
    by_id: BTreeMap<String, Value>,
}

const ANNOTATIONS: [&str; 5] = ["$schema", "$id", "$defs", "title", "description"];
const KEYWORDS: [&str; 9] = [
    "type",
    "properties",
    "required",
    "additionalProperties",
    "items",
    "enum",
    "const",
    "oneOf",
    "$ref",
];

impl Bundle {
    fn load() -> Self {
        let mut by_id = BTreeMap::new();
        for path in json_files(&crate_dir().join("schema")) {
            let schema = read_json(&path);
            let name = path.file_stem().unwrap().to_str().unwrap();
            let id = schema["$id"].as_str().expect("every schema has an $id");
            assert_eq!(
                id,
                format!("p1:protocol/{name}/{}", PROTOCOL_VERSION.major),
                "{}",
                path.display()
            );
            assert_eq!(
                schema["$schema"],
                "https://json-schema.org/draft/2020-12/schema"
            );
            by_id.insert(id.to_owned(), schema);
        }
        Self { by_id }
    }

    fn family(&self, family: &str) -> (&str, &Value) {
        let id = format!("p1:protocol/{family}/{}", PROTOCOL_VERSION.major);
        let (id, schema) = self
            .by_id
            .get_key_value(&id)
            .unwrap_or_else(|| panic!("no schema {id}"));
        (id.as_str(), schema)
    }

    fn validate_family(&self, family: &str, value: &Value) -> Result<(), String> {
        let (id, schema) = self.family(family);
        self.validate(id, schema, value, "")
    }

    fn resolve<'a>(&'a self, base: &'a str, reference: &str) -> (&'a str, &'a Value) {
        let (document, pointer) = reference.split_once('#').unwrap_or((reference, ""));
        let (id, root) = if document.is_empty() {
            (base, &self.by_id[base])
        } else {
            let (id, root) = self
                .by_id
                .get_key_value(document)
                .unwrap_or_else(|| panic!("$ref to unknown schema {document}"));
            (id.as_str(), root)
        };
        assert!(
            pointer.is_empty() || pointer.starts_with("/$defs/"),
            "$ref {reference} must point at the root or into $defs"
        );
        let target = root
            .pointer(pointer)
            .unwrap_or_else(|| panic!("$ref {reference} does not resolve"));
        (id, target)
    }

    fn validate(&self, base: &str, schema: &Value, value: &Value, at: &str) -> Result<(), String> {
        let schema = schema.as_object().expect("a schema is an object");
        for key in schema.keys() {
            assert!(
                ANNOTATIONS.contains(&key.as_str()) || KEYWORDS.contains(&key.as_str()),
                "the bundle uses keyword {key}, which the validator does not implement"
            );
        }
        if let Some(reference) = schema.get("$ref") {
            let (id, target) = self.resolve(base, reference.as_str().unwrap());
            self.validate(id, target, value, at)?;
        }
        if let Some(expected) = schema.get("type") {
            let expected = expected.as_str().unwrap();
            let ok = match expected {
                "object" => value.is_object(),
                "array" => value.is_array(),
                "string" => value.is_string(),
                "boolean" => value.is_boolean(),
                "integer" => value.is_i64() || value.is_u64(),
                "number" => value.is_number(),
                "null" => value.is_null(),
                other => panic!("unknown type {other}"),
            };
            if !ok {
                return Err(format!("{at}: expected {expected}, got {value}"));
            }
        }
        if let Some(constant) = schema.get("const")
            && constant != value
        {
            return Err(format!("{at}: expected {constant}, got {value}"));
        }
        if let Some(allowed) = schema.get("enum")
            && !allowed.as_array().unwrap().contains(value)
        {
            return Err(format!("{at}: {value} is not one of {allowed}"));
        }
        if let Some(object) = value.as_object() {
            let properties = schema.get("properties").and_then(Value::as_object);
            if let Some(required) = schema.get("required") {
                for name in required.as_array().unwrap() {
                    let name = name.as_str().unwrap();
                    if !object.contains_key(name) {
                        return Err(format!("{at}: missing required {name}"));
                    }
                }
            }
            for (name, member) in object {
                let path = format!("{at}/{name}");
                match properties.and_then(|p| p.get(name)) {
                    Some(sub) => self.validate(base, sub, member, &path)?,
                    None => match schema.get("additionalProperties") {
                        Some(Value::Bool(false)) => {
                            return Err(format!("{at}: unknown field {name}"));
                        }
                        Some(Value::Bool(true)) | None => {}
                        Some(sub) => self.validate(base, sub, member, &path)?,
                    },
                }
            }
        }
        if let (Some(items), Some(array)) = (schema.get("items"), value.as_array()) {
            for (index, element) in array.iter().enumerate() {
                self.validate(base, items, element, &format!("{at}/{index}"))?;
            }
        }
        if let Some(branches) = schema.get("oneOf") {
            let matching = branches
                .as_array()
                .unwrap()
                .iter()
                .filter(|branch| self.validate(base, branch, value, at).is_ok())
                .count();
            if matching != 1 {
                return Err(format!(
                    "{at}: {value} matches {matching} oneOf branches, not exactly one"
                ));
            }
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------------------
// Round trips.

/// JSON → wire → contract → wire → JSON must reproduce the fixture exactly.
fn round_trip<W, C>(fixture: &Value) -> Result<Value, String>
where
    W: Serialize + DeserializeOwned + From<C>,
    C: TryFrom<W>,
    <C as TryFrom<W>>::Error: Debug,
{
    let wire: W = serde_json::from_value(fixture.clone()).map_err(|e| e.to_string())?;
    let contract = C::try_from(wire).map_err(|e| format!("{e:?}"))?;
    let back = W::from(contract);
    Ok(serde_json::to_value(back).unwrap())
}

fn round_trip_family(family: &str, fixture: &Value) -> Result<Value, String> {
    match family {
        "tool-call" => round_trip::<WireToolCall, ToolCall>(fixture),
        "tool-outcome" => round_trip::<WireToolOutcome, ToolOutcome>(fixture),
        "history-item" => round_trip::<WireItem, Item>(fixture),
        "stream-event" => round_trip::<WireStreamEvent, StreamEvent>(fixture),
        "provider-error" => round_trip::<WireProviderError, ProviderError>(fixture),
        "call-description" => round_trip::<WireCallDescription, CallDescription>(fixture),
        "result-description" => round_trip::<WireResultDescription, ResultDescription>(fixture),
        "model-options" => round_trip::<WireModelOptions, ModelOptions>(fixture),
        "route-description" => round_trip::<WireRouteDescription, RouteDescription>(fixture),
        "replay-data" => round_trip::<WireReplayData, ReplayData>(fixture),
        "usage" => round_trip::<WireUsage, Usage>(fixture),
        other => panic!("no wire type for family {other}"),
    }
}

#[test]
fn every_schema_file_is_a_family_and_every_family_has_fixtures() {
    let bundle = Bundle::load();
    let families: Vec<String> = FAMILIES
        .iter()
        .map(|f| format!("p1:protocol/{f}/{}", PROTOCOL_VERSION.major))
        .collect();
    let mut sorted = families.clone();
    sorted.sort();
    assert_eq!(bundle.by_id.keys().cloned().collect::<Vec<_>>(), sorted);
    for family in FAMILIES {
        let dir = crate_dir().join("tests/fixtures").join(family);
        assert!(!json_files(&dir).is_empty(), "{family} has no fixtures");
        assert!(
            !json_files(&dir.join("rejected")).is_empty(),
            "{family} has no rejected fixture"
        );
    }
}

#[test]
fn every_fixture_round_trips_through_the_contract_types_and_validates() {
    let bundle = Bundle::load();
    let mut failures = Vec::new();
    for family in FAMILIES {
        for path in json_files(&crate_dir().join("tests/fixtures").join(family)) {
            let fixture = read_json(&path);
            match round_trip_family(family, &fixture) {
                Ok(back) if back == fixture => {}
                Ok(back) => failures.push(format!("{}: round trip gave {back}", path.display())),
                Err(error) => failures.push(format!("{}: {error}", path.display())),
            }
            if let Err(error) = bundle.validate_family(family, &fixture) {
                failures.push(format!("{}: schema: {error}", path.display()));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn every_rejected_fixture_is_refused_by_serde_and_the_schema() {
    let bundle = Bundle::load();
    let mut failures = Vec::new();
    for family in FAMILIES {
        let dir = crate_dir()
            .join("tests/fixtures")
            .join(family)
            .join("rejected");
        for path in json_files(&dir) {
            let fixture = read_json(&path);
            if round_trip_family(family, &fixture).is_ok() {
                failures.push(format!("{}: serde accepted it", path.display()));
            }
            if bundle.validate_family(family, &fixture).is_ok() {
                failures.push(format!("{}: the schema accepted it", path.display()));
            }
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

/// The fixtures must cover every variant; checked against the values they actually hold,
/// so a deleted fixture shows up here rather than as silently shrunken coverage.
#[test]
fn fixtures_cover_every_variant() {
    fn tags(family: &str, pointer: &str) -> Vec<String> {
        let mut found: Vec<String> = json_files(&crate_dir().join("tests/fixtures").join(family))
            .iter()
            .filter_map(|path| read_json(path).pointer(pointer).cloned())
            .map(|value| value.as_str().unwrap().to_owned())
            .collect();
        found.sort();
        found.dedup();
        found
    }
    fn sorted(values: &[&str]) -> Vec<String> {
        let mut values: Vec<String> = values.iter().map(|v| v.to_string()).collect();
        values.sort();
        values
    }
    fn nested(family: &str, key: &str) -> Vec<String> {
        fn walk(value: &Value, key: &str, found: &mut Vec<String>) {
            match value {
                Value::Object(object) => {
                    if let Some(Value::String(tag)) = object.get(key) {
                        found.push(tag.clone());
                    }
                    object.values().for_each(|v| walk(v, key, found));
                }
                Value::Array(array) => array.iter().for_each(|v| walk(v, key, found)),
                _ => {}
            }
        }
        let mut found = Vec::new();
        for path in json_files(&crate_dir().join("tests/fixtures").join(family)) {
            walk(&read_json(&path), key, &mut found);
        }
        found.sort();
        found.dedup();
        found
    }
    assert_eq!(
        tags("history-item", "/item"),
        sorted(&["user", "inbox", "assistant", "tool_result"])
    );
    assert_eq!(
        tags("history-item", "/kind"),
        sorted(&["steering", "notification"])
    );
    assert_eq!(
        nested("history-item", "block"),
        sorted(&["text", "reasoning", "tool_call"])
    );
    assert_eq!(
        nested("stream-event", "block"),
        sorted(&["text", "reasoning", "tool_call"])
    );
    assert_eq!(
        tags("stream-event", "/event"),
        sorted(&[
            "text_delta",
            "reasoning_delta",
            "tool_input_delta",
            "notice",
            "activity",
            "finished"
        ])
    );
    assert_eq!(
        tags("stream-event", "/outcome/status"),
        sorted(&["completed", "failed", "cancelled"])
    );
    assert_eq!(
        tags("stream-event", "/outcome/stop"),
        sorted(&[
            "end_turn",
            "tool_use",
            "max_output_tokens",
            "context_window_exceeded",
            "refusal",
            "paused",
            "other"
        ])
    );
    assert_eq!(
        tags("provider-error", "/kind"),
        sorted(&[
            "invalid_request",
            "authentication",
            "insufficient_balance",
            "not_entitled",
            "usage_limit_exhausted",
            "rate_limited",
            "context_window_exceeded",
            "transport",
            "protocol"
        ])
    );
    assert_eq!(
        tags("tool-outcome", "/status"),
        sorted(&[
            "ok",
            "error",
            "unavailable",
            "denied",
            "cancelled",
            "unknown"
        ])
    );
    assert_eq!(
        tags("result-description", "/detail/kind"),
        sorted(&["diff", "command", "matches", "files", "text"])
    );
    assert_eq!(tags("tool-call", "/input/kind"), sorted(&["json", "text"]));
}
