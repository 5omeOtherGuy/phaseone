//! ADR-0053 item 5: the host-supplied output contract, the `result` of an accepted
//! `done`, and the check stored next to the accepted outcome.
//!
//! The contract is exactly the subset `scripts/workflow.py::schema_errors` validates, so
//! the wording here mirrors that function with JSON type names. Every test drives the
//! tool through a fake [`SessionActivity`]; no host, no files, no provider.

use std::sync::{Arc, Mutex};

use p1_contracts::{
    CancellationToken, DeclarationKind, Tool, ToolCall, ToolContext, ToolInput, ToolOutcome,
    ToolStatus,
};
use p1_tool_finish::{
    Accepted, CompletionPolicy, Evidence, FinishOutcome, FinishTool, OutputContract, SchemaCheck,
    SessionActivity, ShellRun, StructuredResult, ToolFace,
};

const MISSING_VERIFICATION: &str = "Name the commands you ran to verify the work in \"verification\". If nothing can be verified by a command, say why in \"summary\" and pass [\"none\"].";
const CALL_SHAPE: &str =
    "Call finish again with \"verification\": [\"<one of the commands below>\"].";
const TRAILER_NONE: &str =
    "No run counts right now: run your checks (without a pipe) after your last file change.";
const PARAGRAPH: &str = "\nThis task requires a structured result: pass it as \"result\" together with \"done\". It is checked against the schema of the \"result\" parameter; the check is reported to your parent with the outcome.";

#[derive(Default)]
struct FakeActivity {
    last_file_change: Mutex<Option<u64>>,
    runs: Mutex<Vec<ShellRun>>,
}

impl FakeActivity {
    fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    fn ran(&self, command: &str, exit_code: Option<i32>, order: u64) {
        self.runs.lock().unwrap().push(ShellRun {
            command: command.to_string(),
            exit_code,
            order,
        });
    }
}

impl SessionActivity for FakeActivity {
    fn last_file_change(&self) -> Option<u64> {
        *self.last_file_change.lock().unwrap()
    }

    fn shell_runs(&self) -> Vec<ShellRun> {
        self.runs.lock().unwrap().clone()
    }
}

fn contract(schema: serde_json::Value) -> OutputContract {
    OutputContract::new(schema).expect("a valid schema")
}

/// The tool under `contract`, plus the outcome cell it writes.
fn tool(activity: Arc<FakeActivity>, contract: OutputContract) -> (FinishTool, FinishOutcome) {
    let outcome = FinishOutcome::default();
    let tool = FinishTool::new(activity, outcome.clone()).with_output_contract(contract);
    (tool, outcome)
}

fn plain_tool(activity: Arc<FakeActivity>) -> (FinishTool, FinishOutcome) {
    let outcome = FinishOutcome::default();
    (FinishTool::new(activity, outcome.clone()), outcome)
}

fn call(json: String) -> ToolCall {
    ToolCall {
        call_id: "call-1".into(),
        name: "finish".into(),
        input: ToolInput::Json(json),
    }
}

async fn execute(tool: &FinishTool, json: String) -> ToolOutcome {
    let call = call(json);
    let context = ToolContext {
        cancel: CancellationToken::new(),
    };
    tool.execute(&call, context).await
}

/// A `done` call; `result` is added only when the test asks for it.
fn done_json(
    summary: &str,
    verification: serde_json::Value,
    result: Option<&serde_json::Value>,
) -> String {
    let mut input = serde_json::json!({
        "status": "done",
        "summary": summary,
        "verification": verification,
    });
    if let Some(result) = result {
        input["result"] = result.clone();
    }
    input.to_string()
}

fn input_schema(tool: &FinishTool) -> serde_json::Value {
    match &tool.declaration().kind {
        DeclarationKind::Function { input_schema } => input_schema.clone(),
        other => panic!("finish is a function tool, got {other:?}"),
    }
}

/// A schema using every accepted keyword, nested through object and array.
fn full_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "title": "Findings",
        "description": "What the reviewer found.",
        "required": ["id", "kind", "findings", "score", "ok"],
        "additionalProperties": false,
        "properties": {
            "id": { "type": "string" },
            "kind": { "type": "string", "enum": ["a", "b"] },
            "findings": {
                "type": "array",
                "minItems": 1,
                "items": {
                    "type": "object",
                    "required": ["file", "line"],
                    "additionalProperties": false,
                    "properties": {
                        "file": { "type": "string" },
                        "line": { "type": "integer", "minimum": 1 },
                        "note": { "type": "null" }
                    }
                }
            },
            "score": { "type": "number", "minimum": 0 },
            "ok": { "type": "boolean" }
        }
    })
}

/// A conforming value for [`full_schema`].
fn full_value() -> serde_json::Value {
    serde_json::json!({
        "id": "r-1",
        "kind": "a",
        "findings": [{ "file": "src/lib.rs", "line": 3, "note": null }],
        "score": 1.5,
        "ok": true
    })
}

// --------------------------------------------------------------- contract construction

#[test]
fn the_whole_subset_is_accepted_and_kept() {
    let schema = full_schema();
    let contract = OutputContract::new(schema.clone()).expect("the full subset is valid");

    assert_eq!(contract.schema(), &schema);
}

#[test]
fn an_unknown_keyword_is_rejected_with_its_path() {
    let error = OutputContract::new(serde_json::json!({
        "type": "object",
        "properties": { "x": { "type": "integer", "maximum": 3 } }
    }))
    .expect_err("maximum is not in the subset");

    assert!(error.contains("$.properties.x.maximum"), "{error}");
    assert!(error.contains("unknown keyword"), "{error}");
}

#[test]
fn additional_properties_true_is_rejected() {
    let error = OutputContract::new(serde_json::json!({
        "type": "object",
        "additionalProperties": true
    }))
    .expect_err("only false is supported");

    assert!(error.contains("$.additionalProperties"), "{error}");
}

#[test]
fn an_unknown_type_is_rejected() {
    let error = OutputContract::new(serde_json::json!({ "type": "float" }))
        .expect_err("float is not a JSON type name");

    assert!(error.contains("$.type"), "{error}");
    assert!(error.contains("float"), "{error}");
}

#[test]
fn a_wrong_typed_keyword_value_is_rejected() {
    let error = OutputContract::new(serde_json::json!({ "type": "object", "required": "id" }))
        .expect_err("required is an array of strings");

    assert!(error.contains("$.required"), "{error}");
    assert!(error.contains("array of strings"), "{error}");
}

#[test]
fn a_non_object_schema_is_rejected() {
    let error = OutputContract::new(serde_json::json!(["type", "object"]))
        .expect_err("a schema is an object");

    assert_eq!(error, "$: expected object, got array");
}

#[test]
fn nesting_deeper_than_32_is_rejected() {
    fn nested(depth: usize) -> serde_json::Value {
        let mut schema = serde_json::json!({ "type": "string" });
        for _ in 1..depth {
            schema = serde_json::json!({ "type": "object", "properties": { "a": schema } });
        }
        schema
    }

    assert!(OutputContract::new(nested(32)).is_ok());

    let error = OutputContract::new(nested(33)).expect_err("33 levels is too deep");
    assert!(error.contains("deeper than 32"), "{error}");
}

// ------------------------------------------------------------------------- the errors

#[test]
fn a_wrong_type_names_the_expected_and_the_got() {
    let contract = contract(serde_json::json!({ "type": "object" }));

    assert_eq!(
        contract.errors(&serde_json::json!("x")),
        vec!["$: expected object, got string".to_string()]
    );
}

#[test]
fn a_value_outside_the_enum_is_named_with_json_spelling() {
    let contract = contract(serde_json::json!({ "type": "string", "enum": ["a", "b"] }));

    assert_eq!(
        contract.errors(&serde_json::json!("x")),
        vec![r#"$: "x" is not one of ["a","b"]"#.to_string()]
    );
    assert!(contract.errors(&serde_json::json!("a")).is_empty());
}

#[test]
fn a_missing_required_key_is_named() {
    let contract = contract(serde_json::json!({
        "type": "object",
        "required": ["id"],
        "properties": { "id": { "type": "string" } }
    }));

    assert_eq!(
        contract.errors(&serde_json::json!({})),
        vec![r#"$: missing required key "id""#.to_string()]
    );
}

#[test]
fn an_unexpected_key_is_named() {
    let contract = contract(serde_json::json!({
        "type": "object",
        "required": ["id"],
        "additionalProperties": false,
        "properties": { "id": { "type": "string" } }
    }));

    assert_eq!(
        contract.errors(&serde_json::json!({ "id": "a", "extra": 1 })),
        vec![r#"$: unexpected key "extra""#.to_string()]
    );
}

#[test]
fn too_few_items_are_reported_at_the_arrays_own_path() {
    let contract = contract(serde_json::json!({
        "type": "object",
        "required": ["items"],
        "properties": { "items": { "type": "array", "minItems": 1 } }
    }));

    assert_eq!(
        contract.errors(&serde_json::json!({ "items": [] })),
        vec!["$.items: needs at least 1 items".to_string()]
    );
}

#[test]
fn a_number_below_the_minimum_is_reported() {
    let contract = contract(serde_json::json!({
        "type": "object",
        "required": ["score"],
        "properties": { "score": { "type": "number", "minimum": 0 } }
    }));

    assert_eq!(
        contract.errors(&serde_json::json!({ "score": -1 })),
        vec!["$.score: -1 is below 0".to_string()]
    );
    assert!(
        contract
            .errors(&serde_json::json!({ "score": 0 }))
            .is_empty()
    );
}

#[test]
fn an_array_item_error_carries_the_index_and_the_key() {
    let contract = contract(serde_json::json!({
        "type": "object",
        "required": ["findings"],
        "properties": {
            "findings": {
                "type": "array",
                "items": {
                    "type": "object",
                    "required": ["file"],
                    "properties": { "file": { "type": "string" } }
                }
            }
        }
    }));
    let value = serde_json::json!({
        "findings": [{ "file": "a" }, { "file": "b" }, { "file": 7 }]
    });

    assert_eq!(
        contract.errors(&value),
        vec!["$.findings[2].file: expected string, got integer".to_string()]
    );
}

#[test]
fn a_conforming_value_has_no_errors() {
    let contract = contract(full_schema());

    assert!(contract.errors(&full_value()).is_empty());
}

#[test]
fn at_most_32_errors_are_reported() {
    let contract = contract(serde_json::json!({
        "type": "array",
        "items": { "type": "string" }
    }));
    let value = serde_json::Value::Array((0..40).map(serde_json::Value::from).collect());

    let errors = contract.errors(&value);

    assert_eq!(errors.len(), 32);
    assert_eq!(errors[0], "$[0]: expected string, got integer");
    assert_eq!(errors[31], "$[31]: expected string, got integer");
}

#[test]
fn one_error_is_cut_at_300_characters() {
    let long = "x".repeat(400);
    let contract = contract(serde_json::json!({ "type": "string", "enum": ["a"] }));
    let value = serde_json::json!(long);

    let errors = contract.errors(&value);

    assert_eq!(errors.len(), 1);
    assert_eq!(errors[0].chars().count(), 300);
    assert!(errors[0].ends_with('…'), "{}", errors[0]);
}

// ------------------------------------------------------------------- the tool behaviour

#[tokio::test]
async fn a_valid_result_is_stored_next_to_todays_accepted_outcome() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = tool(activity, contract(full_schema()));
    let value = full_value();

    let result = execute(
        &finish,
        done_json("s", serde_json::json!(["cargo test -p x"]), Some(&value)),
    )
    .await;

    assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
    assert_eq!(result.content, "Finished.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string(),
            evidence: Evidence::CommandsPassed(vec!["cargo test -p x".to_string()]),
        })
    );
    assert_eq!(
        outcome.structured(),
        Some(StructuredResult {
            value: Some(value),
            schema: SchemaCheck::Passed,
        })
    );
}

#[tokio::test]
async fn an_invalid_result_is_accepted_with_its_errors() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = tool(activity, contract(full_schema()));
    let value = serde_json::json!({
        "id": "r-1",
        "kind": "x",
        "findings": [],
        "score": -1,
        "ok": true
    });
    let errors = vec![
        "$.findings: needs at least 1 items".to_string(),
        r#"$.kind: "x" is not one of ["a","b"]"#.to_string(),
        "$.score: -1 is below 0".to_string(),
    ];

    let result = execute(
        &finish,
        done_json("s", serde_json::json!(["cargo test -p x"]), Some(&value)),
    )
    .await;

    assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
    assert_eq!(
        result.content,
        format!(
            "Finished. The result does not match the schema:\n- {}\n- {}\n- {}\nYou may call finish again with a corrected result before you stop.",
            errors[0], errors[1], errors[2]
        )
    );
    // The call is accepted: the turn may end, and the errors travel with the outcome.
    assert!(matches!(outcome.get(), Some(Accepted::Done { .. })));
    assert_eq!(
        outcome.structured(),
        Some(StructuredResult {
            value: Some(value),
            schema: SchemaCheck::Failed(errors),
        })
    );
}

#[tokio::test]
async fn a_done_without_a_result_is_rejected_with_the_schema() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let schema = full_schema();
    let (finish, outcome) = tool(activity, contract(schema.clone()));

    let result = execute(
        &finish,
        done_json("s", serde_json::json!(["cargo test -p x"]), None),
    )
    .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!(
            "This task requires a structured \"result\". Call finish again with \"result\" filled in to match this schema:\n{}",
            serde_json::to_string_pretty(&schema).unwrap()
        )
    );
    assert_eq!(outcome.get(), None);
    assert_eq!(outcome.structured(), None);
}

#[tokio::test]
async fn the_schema_shown_for_a_missing_result_is_cut_at_4_kib() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let schema = serde_json::json!({
        "type": "object",
        "description": "y".repeat(5000)
    });
    let (finish, _) = tool(activity, contract(schema));

    let result = execute(
        &finish,
        done_json("s", serde_json::json!(["cargo test -p x"]), None),
    )
    .await;

    let shown = result.content.split_once('\n').unwrap().1;
    assert_eq!(shown.chars().count(), 4096);
    assert!(shown.ends_with('…'), "{}", &shown[shown.len() - 20..]);
}

#[tokio::test]
async fn a_verification_failure_comes_first_and_says_nothing_about_the_result() {
    let activity = FakeActivity::new();
    let (finish, outcome) = tool(activity, contract(full_schema()));

    let result = execute(&finish, done_json("s", serde_json::json!([]), None)).await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        format!("{MISSING_VERIFICATION}\n\n{CALL_SHAPE}\n\n{TRAILER_NONE}")
    );
    assert!(!result.content.contains("structured"), "{}", result.content);
    assert_eq!(outcome.get(), None);
    assert_eq!(outcome.structured(), None);
}

#[tokio::test]
async fn a_result_without_a_contract_is_invalid_input() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = plain_tool(activity);

    let result = execute(
        &finish,
        done_json(
            "s",
            serde_json::json!(["cargo test -p x"]),
            Some(&serde_json::json!({ "id": "a" })),
        ),
    )
    .await;

    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.content,
        "Invalid input for finish: no structured result was requested for this task; remove \"result\"."
    );
    assert_eq!(outcome.get(), None);
    assert_eq!(outcome.structured(), None);
}

#[tokio::test]
async fn without_a_contract_an_accepted_done_says_nothing_was_requested() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = plain_tool(activity);

    let result = execute(
        &finish,
        done_json("s", serde_json::json!(["cargo test -p x"]), None),
    )
    .await;

    assert_eq!(result.content, "Finished.");
    assert!(matches!(outcome.get(), Some(Accepted::Done { .. })));
    assert_eq!(
        outcome.structured(),
        Some(StructuredResult {
            value: None,
            schema: SchemaCheck::NotRequested,
        })
    );
}

#[tokio::test]
async fn blocked_ignores_a_result_and_clears_the_structured_cell() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = tool(activity, contract(full_schema()));
    let value = full_value();

    let accepted = execute(
        &finish,
        done_json("s", serde_json::json!(["cargo test -p x"]), Some(&value)),
    )
    .await;
    assert_eq!(accepted.status, ToolStatus::Ok);
    assert!(outcome.structured().is_some());

    let result = execute(
        &finish,
        serde_json::json!({
            "status": "blocked",
            "summary": "s",
            "needs": "a token",
            "result": value,
        })
        .to_string(),
    )
    .await;

    assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
    assert_eq!(result.content, "Recorded as blocked.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Blocked {
            summary: "s".to_string(),
            needs: "a token".to_string(),
            tried: Vec::new(),
        })
    );
    assert_eq!(outcome.structured(), None);
}

#[tokio::test]
async fn the_last_accepted_done_replaces_both_fields() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = tool(activity, contract(full_schema()));

    let first = execute(
        &finish,
        done_json(
            "first",
            serde_json::json!(["cargo test -p x"]),
            Some(&full_value()),
        ),
    )
    .await;
    assert_eq!(first.status, ToolStatus::Ok, "{}", first.content);

    // The second call corrects the result; both fields now describe it.
    let mut corrected = full_value();
    corrected["score"] = serde_json::json!(2);
    let second = execute(
        &finish,
        done_json(
            "second",
            serde_json::json!(["cargo test -p x"]),
            Some(&corrected),
        ),
    )
    .await;
    assert_eq!(second.status, ToolStatus::Ok, "{}", second.content);

    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "second".to_string(),
            evidence: Evidence::CommandsPassed(vec!["cargo test -p x".to_string()]),
        })
    );
    assert_eq!(
        outcome.structured(),
        Some(StructuredResult {
            value: Some(corrected),
            schema: SchemaCheck::Passed,
        })
    );
}

#[tokio::test]
async fn clear_drops_both_fields() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let (finish, outcome) = tool(activity, contract(full_schema()));

    let result = execute(
        &finish,
        done_json(
            "s",
            serde_json::json!(["cargo test -p x"]),
            Some(&full_value()),
        ),
    )
    .await;
    assert_eq!(result.status, ToolStatus::Ok);

    outcome.clear();

    assert_eq!(outcome.get(), None);
    assert_eq!(outcome.structured(), None);
}

#[tokio::test]
async fn a_face_after_the_contract_keeps_the_contract_and_the_result_property() {
    let activity = FakeActivity::new();
    activity.ran("cargo test -p x", Some(0), 1);
    let outcome = FinishOutcome::default();
    let schema = full_schema();
    let finish = FinishTool::new(activity, outcome.clone())
        .with_output_contract(contract(schema))
        .with_face(ToolFace::new("done", "the environment's own words"), "gpt");

    assert_eq!(finish.declaration().name, "done");
    assert_eq!(
        finish.declaration().description,
        "the environment's own words"
    );
    assert_eq!(finish.identity().variant, "gpt");
    assert!(finish.output_contract().is_some());
    let declared = input_schema(&finish);
    assert_eq!(
        declared["required"],
        serde_json::json!(["status", "summary"])
    );
    assert_eq!(declared["properties"]["result"]["type"], "object");

    // ...and the contract still governs the call.
    let result = execute(
        &finish,
        done_json(
            "s",
            serde_json::json!(["cargo test -p x"]),
            Some(&full_value()),
        ),
    )
    .await;
    assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
    assert!(matches!(
        outcome.structured(),
        Some(StructuredResult {
            schema: SchemaCheck::Passed,
            ..
        })
    ));
}

#[tokio::test]
async fn report_to_parent_with_a_contract_checks_the_result_too() {
    let activity = FakeActivity::new();
    let outcome = FinishOutcome::default();
    let finish = FinishTool::new(activity, outcome.clone())
        .with_output_contract(contract(full_schema()))
        .with_policy(CompletionPolicy::ReportToParent);

    let result = execute(
        &finish,
        done_json("s", serde_json::json!(["none"]), Some(&full_value())),
    )
    .await;

    assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
    assert_eq!(result.content, "Finished.");
    assert_eq!(
        outcome.get(),
        Some(Accepted::Done {
            summary: "s".to_string(),
            evidence: Evidence::NotRun("no command tool granted".to_string()),
        })
    );
    assert_eq!(
        outcome.structured(),
        Some(StructuredResult {
            value: Some(full_value()),
            schema: SchemaCheck::Passed,
        })
    );
}

// ------------------------------------------------------------------------ declaration

#[test]
fn the_contract_adds_one_paragraph_and_the_result_property() {
    let activity = FakeActivity::new();
    let plain = FinishTool::new(activity.clone(), FinishOutcome::default());
    let with_contract = FinishTool::new(activity, FinishOutcome::default())
        .with_output_contract(contract(full_schema()));

    assert_eq!(
        with_contract.declaration().description,
        format!("{}{PARAGRAPH}", plain.declaration().description)
    );

    let declared = input_schema(&with_contract);
    assert_eq!(
        declared["required"],
        serde_json::json!(["status", "summary"])
    );
    assert_eq!(
        declared["properties"]["result"]["description"],
        "Required with \"done\": What the reviewer found."
    );
    assert_eq!(
        declared["properties"]["result"]["properties"],
        full_schema()["properties"]
    );

    // Without a contract the declaration is today's: the base schema untouched, and no
    // paragraph appended to the policy's own words.
    let plain_schema = input_schema(&plain);
    assert!(plain_schema["properties"].get("result").is_none());
    let mut stripped = declared.clone();
    stripped["properties"]
        .as_object_mut()
        .unwrap()
        .remove("result");
    assert_eq!(stripped, plain_schema);
    assert!(
        plain
            .declaration()
            .description
            .contains("verify first with a command"),
        "{}",
        plain.declaration().description
    );
}

#[test]
fn policy_and_contract_apply_in_either_order() {
    let schema = full_schema();
    let policy_first = FinishTool::new(FakeActivity::new(), FinishOutcome::default())
        .with_policy(CompletionPolicy::ReportToParent)
        .with_output_contract(contract(schema.clone()));
    let contract_first = FinishTool::new(FakeActivity::new(), FinishOutcome::default())
        .with_output_contract(contract(schema))
        .with_policy(CompletionPolicy::ReportToParent);
    let report_face = FinishTool::new(FakeActivity::new(), FinishOutcome::default())
        .with_policy(CompletionPolicy::ReportToParent);

    assert_eq!(
        policy_first.declaration().description,
        format!("{}{PARAGRAPH}", report_face.declaration().description)
    );
    assert_eq!(
        policy_first.declaration().description,
        contract_first.declaration().description
    );
    assert_eq!(
        policy_first.declaration().kind,
        contract_first.declaration().kind
    );
    assert!(policy_first.output_contract().is_some());
}
