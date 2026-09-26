//! The guest behaviour of the `finish` tool (ADR-0037, ADR-0051, ADR-0053 item 5), as pure
//! computation over the session record a caller hands in.
//!
//! One source for three callers (decision S0-R3, `docs/design/modules/package.md` "Shared
//! guest logic"): the `p1/finish` component runs it over what it reads through the
//! `completion` capability, the native `FinishTool` runs it over the host's
//! `SessionActivity`, so the frozen native tests prove the code the component ships, and the
//! host's completion hub reuses the command rules ([`command_failure`], [`normalise_command`])
//! and the contract check ([`OutputContract::errors`]) when it re-verifies a candidate
//! (ADR-0083 §2), so both sides mean the same thing by "a run that counts".
//!
//! Nothing here reads a clock, a file or the environment, so it builds unchanged for
//! `wasm32-unknown-unknown`.
#![forbid(unsafe_code)]

use std::collections::HashMap;

use serde::Deserialize;

/// One finished `Executes` call, oldest first in a [`Record`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    /// The command exactly as the model passed it to the `shell` tool.
    pub command: String,
    /// The parsed `[exit code: N]` footer; `None` when there was none (timeout,
    /// cancellation, a non-shell `Executes` tool). `None` never counts as success.
    pub exit_code: Option<i32>,
    /// Monotonically increasing order of the finished call within the session.
    pub order: u64,
}

/// What a `finish` call is checked against: the session record as the caller read it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Record {
    /// `order` of the last file change, if any.
    pub last_file_change: Option<u64>,
    /// Every finished `Executes` call so far, oldest first.
    pub runs: Vec<ShellRun>,
}

/// Which completion rule a `finish` tool applies (ADR-0051 item 1). The HOST chooses
/// it, from the assembled tools' identities; the tool never inspects grant names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// Today's rule (ADR-0037), unchanged: `done` needs a command whose successful
    /// run is recorded after the last file change, and `["none"]` is accepted only in
    /// a session that changed no file.
    RecordedCommands,
    /// For an agent with no tool that runs commands: `["none"]` is accepted after a
    /// file change too, and the accepted result is labelled unverified for the parent.
    ReportToParent,
}

impl CompletionPolicy {
    /// The model-facing description that carries this policy. The default face of a
    /// tool built under the policy, so a host that switches the policy late can
    /// present the tool the model would have got from the factory.
    pub fn description(self) -> &'static str {
        match self {
            Self::RecordedCommands => DESCRIPTION,
            Self::ReportToParent => REPORT_DESCRIPTION,
        }
    }
}

/// What an accepted `done` established (ADR-0051 item 2). Host-owned: it is built
/// from the session's own record, never from the model's input, and it is what the
/// parent is told.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// These commands all have a recorded successful run after the last file change,
    /// spelled as the trailer spells them (normalised).
    CommandsPassed(Vec<String>),
    /// Nothing was established, and why. Never printed as "verified".
    NotRun(String),
}

/// The `not-run` reason under [`CompletionPolicy::ReportToParent`].
pub const NOT_RUN_NO_COMMAND_TOOL: &str = "no command tool granted";
/// The `not-run` reason under [`CompletionPolicy::RecordedCommands`] in a session that
/// changed no file.
pub const NOT_RUN_NO_FILE_CHANGED: &str = "no file changed";

/// A `finish` call the tool accepted. Last accepted call wins.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Accepted {
    Done {
        summary: String,
        evidence: Evidence,
    },
    Blocked {
        summary: String,
        needs: String,
        tried: Vec<String>,
    },
}

/// A host-supplied JSON-Schema SUBSET the `result` of an accepted `done` is checked
/// against (ADR-0053 item 5). Exactly the subset `scripts/workflow.py::schema_errors`
/// validates, so a brief's schema means the same thing in both runners.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OutputContract {
    schema: serde_json::Value,
}

impl OutputContract {
    /// Validate the schema ONCE, here: a malformed contract fails before any worker is
    /// started, and every rejection names the JSON path and the reason.
    /// Accepts `type` (one of object, array, string, integer, number, boolean, null),
    /// `enum`, `required`, `properties`, `additionalProperties: false`, `items`,
    /// `minItems`, `minimum`, and the ignored documentation keywords `description` and
    /// `title`.
    pub fn new(schema: serde_json::Value) -> Result<Self, String> {
        validate_schema(&schema)?;
        Ok(Self { schema })
    }

    /// The validated schema, for a host that wants to show or journal it.
    pub fn schema(&self) -> &serde_json::Value {
        &self.schema
    }

    /// The errors of `value` against the schema, empty when it conforms. The wording
    /// and the order mirror `workflow.py::schema_errors` with JSON type names:
    /// `$: expected object, got string`, `$.kind: "x" is not one of ["a","b"]`,
    /// `$: missing required key "id"`, `$: unexpected key "extra"`,
    /// `$.items: needs at least 1 items`, `$.score: -1 is below 0`. At most
    /// [`MAX_ERRORS`] errors, each at most [`MAX_ERROR_CHARS`] characters.
    pub fn errors(&self, value: &serde_json::Value) -> Vec<String> {
        let mut errors = Vec::new();
        collect_errors(value, &self.schema, "$", &mut errors);
        errors
    }
}

/// Whether an accepted `done`'s `result` met the contract.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCheck {
    /// No contract was set, so nothing was asked for.
    NotRequested,
    Passed,
    Failed(Vec<String>),
}

/// The `result` of the LAST accepted `done`, with its check. `value` is `None` only
/// under [`SchemaCheck::NotRequested`] (no contract, so nothing was asked for).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredResult {
    pub value: Option<serde_json::Value>,
    pub schema: SchemaCheck,
}

impl StructuredResult {
    /// The result and its check for `value` under `contract`: what an accepted `done`
    /// stores next to its outcome. `None` for the value when no contract asked for one.
    pub fn checked(contract: Option<&OutputContract>, value: Option<serde_json::Value>) -> Self {
        match (contract, value) {
            (Some(contract), Some(value)) => {
                let errors = contract.errors(&value);
                let schema = if errors.is_empty() {
                    SchemaCheck::Passed
                } else {
                    SchemaCheck::Failed(errors)
                };
                Self {
                    value: Some(value),
                    schema,
                }
            }
            _ => Self {
                value: None,
                schema: SchemaCheck::NotRequested,
            },
        }
    }
}

/// Keywords the contract accepts, as a rejection lists them.
const CONTRACT_KEYWORDS: &str = "type, enum, required, properties, additionalProperties, items, minItems, minimum, description, title";
/// The nesting a contract may reach before it is refused: a schema deeper than this
/// is a mistake, not a brief.
const MAX_SCHEMA_DEPTH: usize = 32;
/// Bounds on what a mismatch sends back, so one huge value cannot flood the model.
pub const MAX_ERRORS: usize = 32;
pub const MAX_ERROR_CHARS: usize = 300;
const MAX_SCHEMA_PRINT_CHARS: usize = 4096;

/// Validate the contract's schema, depth-first, reporting the FIRST problem with the
/// JSON path that names it.
fn validate_schema(schema: &serde_json::Value) -> Result<(), String> {
    validate_subschema(schema, "$", 1)
}

fn validate_subschema(schema: &serde_json::Value, path: &str, depth: usize) -> Result<(), String> {
    if depth > MAX_SCHEMA_DEPTH {
        return Err(format!("{path}: nesting deeper than {MAX_SCHEMA_DEPTH}"));
    }
    let Some(object) = schema.as_object() else {
        return Err(format!(
            "{path}: expected object, got {}",
            json_type_name(schema)
        ));
    };
    for (keyword, value) in object {
        match keyword.as_str() {
            "description" | "title" => {
                if !value.is_string() {
                    return Err(format!(
                        "{path}.{keyword}: expected string, got {}",
                        json_type_name(value)
                    ));
                }
            }
            "type" => {
                let Some(name) = value.as_str() else {
                    return Err(format!(
                        "{path}.type: expected string, got {}",
                        json_type_name(value)
                    ));
                };
                if !matches!(
                    name,
                    "object" | "array" | "string" | "integer" | "number" | "boolean" | "null"
                ) {
                    return Err(format!(
                        "{path}.type: unknown type {name:?}; expected one of object, array, string, integer, number, boolean, null"
                    ));
                }
            }
            "enum" => {
                if !value.is_array() {
                    return Err(format!(
                        "{path}.enum: expected array, got {}",
                        json_type_name(value)
                    ));
                }
            }
            "required" => {
                let strings = value
                    .as_array()
                    .is_some_and(|entries| entries.iter().all(serde_json::Value::is_string));
                if !strings {
                    return Err(format!(
                        "{path}.required: expected an array of strings, got {}",
                        json_type_name(value)
                    ));
                }
            }
            "properties" => {
                let Some(properties) = value.as_object() else {
                    return Err(format!(
                        "{path}.properties: expected object, got {}",
                        json_type_name(value)
                    ));
                };
                for (key, sub_schema) in properties {
                    validate_subschema(sub_schema, &format!("{path}.properties.{key}"), depth + 1)?;
                }
            }
            "additionalProperties" => {
                if value != &serde_json::Value::Bool(false) {
                    return Err(format!(
                        "{path}.additionalProperties: only false is supported, got {}",
                        compact(value)
                    ));
                }
            }
            "items" => validate_subschema(value, &format!("{path}.items"), depth + 1)?,
            "minItems" => {
                if value.as_u64().is_none() {
                    return Err(format!(
                        "{path}.minItems: expected a non-negative integer, got {}",
                        compact(value)
                    ));
                }
            }
            "minimum" => {
                if !value.is_number() {
                    return Err(format!(
                        "{path}.minimum: expected a number, got {}",
                        json_type_name(value)
                    ));
                }
            }
            other => {
                return Err(format!(
                    "{path}.{other}: unknown keyword; supported: {CONTRACT_KEYWORDS}"
                ));
            }
        }
    }
    Ok(())
}

/// The errors of `value` under `schema` at `path`, in the order `workflow.py` reports
/// them; a wrong type is the whole answer for that node, as there.
fn collect_errors(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    path: &str,
    errors: &mut Vec<String>,
) {
    if errors.len() >= MAX_ERRORS {
        return;
    }
    if let Some(kind) = schema.get("type").and_then(serde_json::Value::as_str)
        && !matches_type(kind, value)
    {
        push_error(
            errors,
            format!("{path}: expected {kind}, got {}", json_type_name(value)),
        );
        return;
    }
    if let Some(allowed) = schema.get("enum").and_then(serde_json::Value::as_array)
        && !allowed.contains(value)
    {
        push_error(
            errors,
            format!(
                "{path}: {} is not one of {}",
                compact(value),
                compact(schema.get("enum").unwrap())
            ),
        );
    }
    if let Some(object) = value.as_object() {
        let properties = schema
            .get("properties")
            .and_then(serde_json::Value::as_object);
        if let Some(required) = schema.get("required").and_then(serde_json::Value::as_array) {
            for key in required.iter().filter_map(serde_json::Value::as_str) {
                if !object.contains_key(key) {
                    push_error(errors, format!("{path}: missing required key {key:?}"));
                }
            }
        }
        for (key, item) in object {
            match properties.and_then(|properties| properties.get(key)) {
                Some(sub_schema) => {
                    collect_errors(item, sub_schema, &format!("{path}.{key}"), errors);
                }
                None if schema.get("additionalProperties")
                    == Some(&serde_json::Value::Bool(false)) =>
                {
                    push_error(errors, format!("{path}: unexpected key {key:?}"));
                }
                None => {}
            }
        }
    }
    if let Some(array) = value.as_array() {
        let needed = schema
            .get("minItems")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if (array.len() as u64) < needed {
            push_error(errors, format!("{path}: needs at least {needed} items"));
        }
        if let Some(items) = schema.get("items") {
            for (index, item) in array.iter().enumerate() {
                collect_errors(item, items, &format!("{path}[{index}]"), errors);
            }
        }
    }
    // Only a number can be below a minimum; `true` is not a number here, as in Python.
    if let Some(minimum) = schema.get("minimum").and_then(serde_json::Value::as_f64)
        && let Some(number) = value.as_f64()
        && number < minimum
    {
        push_error(
            errors,
            format!(
                "{path}: {} is below {}",
                compact(value),
                compact(schema.get("minimum").unwrap())
            ),
        );
    }
}

fn push_error(errors: &mut Vec<String>, message: String) {
    if errors.len() < MAX_ERRORS {
        errors.push(truncate(message, MAX_ERROR_CHARS));
    }
}

/// The JSON type name of a value, for a message a model reads.
fn json_type_name(value: &serde_json::Value) -> &'static str {
    match value {
        serde_json::Value::Null => "null",
        serde_json::Value::Bool(_) => "boolean",
        serde_json::Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        serde_json::Value::Number(_) => "number",
        serde_json::Value::String(_) => "string",
        serde_json::Value::Array(_) => "array",
        serde_json::Value::Object(_) => "object",
    }
}

/// The same test `workflow.py::schema_errors` makes for each `type`. `true` is not an
/// integer here, as `isinstance(True, int)` is guarded against there.
fn matches_type(kind: &str, value: &serde_json::Value) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        "number" => value.is_number(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        _ => false,
    }
}

/// A value as one compact JSON line, for quoting it back in an error.
fn compact(value: &serde_json::Value) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// `text` cut to `limit` characters with a trailing `…`, so one value cannot flood the
/// model's context.
fn truncate(text: String, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text;
    }
    let mut cut: String = text.chars().take(limit - 1).collect();
    cut.push('…');
    cut
}

/// The tool's default model-facing name.
pub const NAME: &str = "finish";
/// The `RecordedCommands` description.
pub const DESCRIPTION: &str = "End the task by saying, in a tool call, that it is done or blocked.\n`done`: verify first with a command, then name the exact command(s) you ran in `verification`; they must have succeeded after your last file change. Use `[\"none\"]` only when the task changed no files.\n`blocked`: say what you need in `needs` and what you tried; the run stops and reports it.\nA pipe does not count: a command run through a pipe (for example `... | tail`) exits with its last stage's code, so run the check without a pipe. The same goes for `;`, `||`, a single `&` or a new line after the check. Name the command as you ran it; a leading `cd <dir> &&` and spacing differences are ignored.";

/// The `ReportToParent` face (ADR-0051 item 1): the same tool and the same checks,
/// presented to an agent that has no tool that runs commands.
pub const REPORT_DESCRIPTION: &str = "End the task by saying, in a tool call, that it is done or blocked.\nYou have no tool that runs commands, so no command can verify this work: call `done` with `verification: [\"none\"]`, and say in `summary` what you did and what remains unchecked. The result is reported to your parent as \"not verified; parent verification required\".\n`done`: `verification` is `[\"none\"]` — a command you cannot run proves nothing.\n`blocked`: say what you need in `needs` and what you tried; the run stops and reports it.";

/// The three exact rule texts, model-visible.
const ERR_MISSING_VERIFICATION: &str = "Name the commands you ran to verify the work in \"verification\". If nothing can be verified by a command, say why in \"summary\" and pass [\"none\"].";
const ERR_NONE_CHANGED_FILES: &str =
    "This session changed files; verify the result with a command before finishing.";
const ERR_NEEDS: &str = "Say what you need in \"needs\".";

/// The one paragraph a contract appends to the policy's description, so the model
/// knows its answer must be data and that the verdict goes to the parent.
pub const STRUCTURED_RESULT_PARAGRAPH: &str = "\nThis task requires a structured result: pass it as \"result\" together with \"done\". It is checked against the schema of the \"result\" parameter; the check is reported to your parent with the outcome.";

/// Error 1 additionally shows the call shape, so the model has a template.
const CALL_SHAPE: &str =
    "Call finish again with \"verification\": [\"<one of the commands below>\"].";
/// Every verification rejection ends with what would be accepted right now.
const TRAILER_HEADING: &str =
    "Runs that count right now (successful, not piped, after the last file change):";
const TRAILER_NONE: &str =
    "No run counts right now: run your checks (without a pipe) after your last file change.";

/// The policy's own words, plus the contract paragraph when a contract is set: the
/// description a tool built under `policy` and `contract` presents.
pub fn description(policy: CompletionPolicy, contract: Option<&OutputContract>) -> String {
    let mut text = policy.description().to_string();
    if contract.is_some() {
        text.push_str(STRUCTURED_RESULT_PARAGRAPH);
    }
    text
}

/// The input schema, with the contract's schema as `result` when one is set.
pub fn input_schema(contract: Option<&OutputContract>) -> serde_json::Value {
    let mut schema = serde_json::json!({
        "type": "object",
        "properties": {
            "status": {
                "type": "string",
                "enum": ["done", "blocked"],
                "description": "\"done\" when the task is complete and verified, \"blocked\" when something outside your control stops you."
            },
            "summary": {
                "type": "string",
                "description": "Short summary of what you did, or why nothing could be verified."
            },
            "verification": {
                "type": "array",
                "items": { "type": "string" },
                "description": "For \"done\": the exact commands you ran that prove the work. [\"none\"] only when no files changed."
            },
            "needs": {
                "type": "string",
                "description": "For \"blocked\": what you need from outside to continue."
            },
            "tried": {
                "type": "array",
                "items": { "type": "string" },
                "description": "For \"blocked\": what you already tried."
            }
        },
        "required": ["status", "summary"],
        "additionalProperties": false
    });
    if let Some(contract) = contract {
        schema["properties"]["result"] = result_property(contract);
    }
    schema
}

/// The contract's own schema as the `result` parameter. Its `description`, if it has
/// one, says when the parameter is required; `required` stays `["status","summary"]`
/// because a `blocked` call needs no result.
fn result_property(contract: &OutputContract) -> serde_json::Value {
    let mut result = contract.schema().clone();
    let own = contract
        .schema()
        .get("description")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("");
    result["description"] = serde_json::Value::String(format!("Required with \"done\": {own}"));
    result
}

/// A call's raw input, as the caller holds it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RawInput<'a> {
    /// A function call's arguments as JSON text, possibly invalid.
    Json(&'a str),
    /// Freeform text, which `finish` never accepts.
    Text(&'a str),
}

#[derive(Debug, Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Status {
    Done,
    Blocked,
}

impl Status {
    /// The word a call description names the status with.
    pub fn word(self) -> &'static str {
        match self {
            Self::Done => "done",
            Self::Blocked => "blocked",
        }
    }
}

/// A parsed `finish` call.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FinishInput {
    status: Status,
    summary: String,
    #[serde(default)]
    verification: Option<Vec<String>>,
    #[serde(default)]
    needs: Option<String>,
    #[serde(default)]
    tried: Option<Vec<String>>,
    /// The structured answer a contract asks for (ADR-0053 item 5); unknown without
    /// one, and ignored by `blocked`.
    #[serde(default)]
    result: Option<serde_json::Value>,
}

impl FinishInput {
    /// The status the call reports.
    pub fn status(&self) -> Status {
        self.status
    }
}

/// Parse a call's input; `tool` is the model-facing name the rejection names.
pub fn parse_input(tool: &str, raw: RawInput<'_>) -> Result<FinishInput, String> {
    let raw = match raw {
        RawInput::Json(raw) => raw,
        RawInput::Text(_) => {
            return Err(invalid(
                tool,
                "expected a JSON object input, got freeform text",
            ));
        }
    };
    serde_json::from_str(raw).map_err(|error| invalid(tool, &error.to_string()))
}

fn invalid(tool: &str, reason: &str) -> String {
    format!("Invalid input for {tool}: {reason}")
}

/// Rule 6's error: the task asked for data, so the call is incomplete. It shows the
/// schema itself, because that is what the model has to fill in.
fn missing_result_error(schema: &serde_json::Value) -> String {
    let pretty = serde_json::to_string_pretty(schema).unwrap_or_else(|_| "{}".to_string());
    format!(
        "This task requires a structured \"result\". Call finish again with \"result\" filled in to match this schema:\n{}",
        truncate(pretty, MAX_SCHEMA_PRINT_CHARS)
    )
}

/// The reply to a `result` that does not match: the call is accepted, the errors travel
/// with the outcome, and one more call may correct it before the turn ends.
fn failed_result_reply(errors: &[String]) -> String {
    let mut text = String::from("Finished. The result does not match the schema:");
    for error in errors {
        text.push_str("\n- ");
        text.push_str(error);
    }
    text.push_str("\nYou may call finish again with a corrected result before you stop.");
    text
}

/// An accepted call: what it stores and what the model reads.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Verdict {
    /// The accepted outcome.
    pub accepted: Accepted,
    /// The structured result of an accepted `done`; `None` after a `blocked`.
    pub structured: Option<StructuredResult>,
    /// The model-visible text of the accepted call.
    pub reply: String,
}

/// Apply the §2 rules. `Ok` is the accepted call; `Err` is a rule violation's
/// model-visible text, which stores nothing. The policy decides what `["none"]` means
/// (ADR-0051 item 1); every other rule is shared, so invalid evidence never downgrades to
/// an accepted unverified result.
///
/// With an output contract (ADR-0053 item 5) the verification rules run FIRST, as
/// before; then `result` is required and checked, and the verdict is stored next to the
/// accepted outcome — a mismatch is still an ACCEPTED call, so the turn may end and the
/// parent reads the errors.
pub fn evaluate(
    tool: &str,
    input: FinishInput,
    policy: CompletionPolicy,
    contract: Option<&OutputContract>,
    record: &Record,
) -> Result<Verdict, String> {
    match input.status {
        Status::Done => {
            let verification = input.verification.unwrap_or_default();
            if verification.is_empty() {
                return Err(error_one(record, ERR_MISSING_VERIFICATION));
            }
            let evidence = if verification.len() == 1 && verification[0].trim() == "none" {
                match policy {
                    // An agent with no command tool cannot verify anything itself;
                    // it ends honestly and the parent verifies (ADR-0051 item 1).
                    CompletionPolicy::ReportToParent => {
                        Evidence::NotRun(NOT_RUN_NO_COMMAND_TOOL.to_string())
                    }
                    CompletionPolicy::RecordedCommands => {
                        if record.last_file_change.is_some() {
                            return Err(with_trailer(record, ERR_NONE_CHANGED_FILES));
                        }
                        // Not writing a file is no proof that an answer is right.
                        Evidence::NotRun(NOT_RUN_NO_FILE_CHANGED.to_string())
                    }
                }
            } else {
                verify(record, &verification)?;
                Evidence::CommandsPassed(
                    verification
                        .iter()
                        .map(|named| normalise_command(named))
                        .collect(),
                )
            };
            let (structured, reply) = match contract {
                None => {
                    if input.result.is_some() {
                        return Err(invalid(
                            tool,
                            "no structured result was requested for this task; remove \"result\".",
                        ));
                    }
                    (
                        StructuredResult::checked(None, None),
                        "Finished.".to_string(),
                    )
                }
                Some(contract) => match input.result {
                    None => return Err(missing_result_error(contract.schema())),
                    Some(value) => {
                        let structured = StructuredResult::checked(Some(contract), Some(value));
                        let reply = match &structured.schema {
                            SchemaCheck::Failed(errors) => failed_result_reply(errors),
                            _ => "Finished.".to_string(),
                        };
                        (structured, reply)
                    }
                },
            };
            Ok(Verdict {
                accepted: Accepted::Done {
                    summary: input.summary,
                    evidence,
                },
                structured: Some(structured),
                reply,
            })
        }
        Status::Blocked => {
            let needs = input.needs.unwrap_or_default();
            if needs.trim().is_empty() {
                return Err(ERR_NEEDS.to_string());
            }
            // A blocked call needs no result: a present one is ignored, and the
            // structured cell is emptied so it cannot describe this call.
            Ok(Verdict {
                accepted: Accepted::Blocked {
                    summary: input.summary,
                    needs,
                    tried: input.tried.unwrap_or_default(),
                },
                structured: None,
                reply: "Recorded as blocked.".to_string(),
            })
        }
    }
}

/// Every named command must match, after normalisation, the LAST recorded run of that
/// command, and that run must be a success newer than the last file change. EVERY
/// failing command is reported in one error, in the order named, so one mistake costs
/// one call.
fn verify(record: &Record, verification: &[String]) -> Result<(), String> {
    let failures: Vec<String> = verification
        .iter()
        .filter_map(|named| command_failure(named, &record.runs, record.last_file_change))
        .collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(with_trailer(record, failures.join("\n")))
    }
}

/// Error 1 additionally shows the call shape before the trailer.
fn error_one(record: &Record, message: &str) -> String {
    format!("{message}\n\n{CALL_SHAPE}\n\n{}", trailer(record))
}

/// Errors 1–3 end with a blank line and the runs that would be accepted now.
fn with_trailer(record: &Record, message: impl AsRef<str>) -> String {
    format!("{}\n\n{}", message.as_ref(), trailer(record))
}

fn trailer(record: &Record) -> String {
    let commands = counting_commands(record);
    if commands.is_empty() {
        return TRAILER_NONE.to_string();
    }
    let mut text = String::from(TRAILER_HEADING);
    for command in commands {
        text.push_str("\n- ");
        text.push_str(&command);
    }
    text
}

/// The commands `finish` would accept right now: one entry per normalised spelling (the
/// LAST run decides), successful, unpiped, unmasked and newer than the last file change;
/// newest last, at most five.
fn counting_commands(record: &Record) -> Vec<String> {
    let last_change = record.last_file_change;
    let mut last: HashMap<String, &ShellRun> = HashMap::new();
    for run in &record.runs {
        last.insert(normalise_command(&run.command), run);
    }
    let mut counting: Vec<(u64, String)> = last
        .into_iter()
        .filter(|(_, run)| {
            run.exit_code == Some(0)
                && !is_piped(&run.command)
                && !is_masked(&run.command)
                && last_change.is_none_or(|change| run.order > change)
        })
        .map(|(command, run)| (run.order, command))
        .collect();
    counting.sort();
    let keep_from = counting.len().saturating_sub(5);
    counting
        .into_iter()
        .skip(keep_from)
        .map(|(_, command)| command)
        .collect()
}

fn no_successful_run(named: &str) -> String {
    format!(
        "No successful run of `{named}` is recorded in this session. Run it, read the result, then finish."
    )
}

fn pipe_error(named: &str) -> String {
    format!(
        "`{named}` was run through a pipe, so its exit code says nothing about it. Run it without a pipe, then finish."
    )
}

fn masked_error(named: &str) -> String {
    format!(
        "`{named}` continues after a failure (`;`, `||`, `&` or a new line), so its exit code says nothing about the check. Run the check on its own, then finish."
    )
}

/// `None` when the named command passes rules 2, 3, the pipe rule and the masked rule
/// against `runs`; `Some` with the model-visible message of the FIRST rule it breaks.
/// The LAST run of the command decides, so a failing re-run invalidates an earlier
/// success.
pub fn command_failure(named: &str, runs: &[ShellRun], last_change: Option<u64>) -> Option<String> {
    let wanted = normalise_command(named);
    let Some(run) = runs
        .iter()
        .rev()
        .find(|run| normalise_command(&run.command) == wanted)
    else {
        return Some(no_successful_run(named));
    };
    // A pipe hides the check's exit code behind its last stage's, so the recorded
    // status says nothing even when it is zero.
    if is_piped(&run.command) {
        return Some(pipe_error(named));
    }
    // `;`, `||`, a newline or a single `&` lets something else run last, with the
    // same effect.
    if is_masked(&run.command) {
        return Some(masked_error(named));
    }
    if run.exit_code != Some(0) {
        return Some(no_successful_run(named));
    }
    if let Some(change) = last_change
        && run.order < change
    {
        return Some(format!(
            "You changed files after running `{named}`. Run it again, then finish."
        ));
    }
    None
}

/// Normalise a command for comparison: trim, collapse every run of whitespace to
/// one space, and drop ONE leading `cd <path> &&` segment. Applied to both the
/// recorded command and the named one, so matching is symmetric.
pub fn normalise_command(command: &str) -> String {
    let collapsed = command.split_whitespace().collect::<Vec<_>>().join(" ");
    match collapsed.find(" && ") {
        // `at > 3` keeps the path between `cd ` and ` && ` non-empty.
        Some(at) if collapsed.starts_with("cd ") && at > 3 => collapsed[at + 4..].to_string(),
        _ => collapsed,
    }
}

/// Every character of `command` that sits OUTSIDE `'…'`/`"…"`, with the characters
/// on either side of it (quoted or not). Quoting is a simple scan for quotes, not a
/// shell parser (a stated limit in `docs/design/completion.md` §2); the pipe and the
/// masking test share this one scan.
fn outside_quotes(command: &str) -> Vec<(char, Option<char>, Option<char>)> {
    let chars: Vec<char> = command.chars().collect();
    let mut unquoted = Vec::new();
    let mut quote: Option<char> = None;
    for (index, &character) in chars.iter().enumerate() {
        match quote {
            Some(open) if character == open => quote = None,
            Some(_) => {}
            None => match character {
                '\'' => quote = Some('\''),
                '"' => quote = Some('"'),
                _ => unquoted.push((
                    character,
                    index.checked_sub(1).map(|i| chars[i]),
                    chars.get(index + 1).copied(),
                )),
            },
        }
    }
    unquoted
}

/// True when the command contains an unquoted `|` that is not part of `||`.
pub fn is_piped(command: &str) -> bool {
    outside_quotes(command)
        .into_iter()
        .any(|(character, previous, next)| {
            character == '|' && previous != Some('|') && next != Some('|')
        })
}

/// True when the exit status is masked: outside quotes the command contains `;`, `||`,
/// a newline, or a single `&` that is not part of `&&`. Each of those runs something
/// else afterwards, which decides the exit code, so the recorded status says nothing
/// about the check that ran first. `&&` chains stay honest, and so does an `&` that
/// belongs to a redirection (`2>&1`, `>&2` after the `>`, `&>file` before it).
pub fn is_masked(command: &str) -> bool {
    outside_quotes(command)
        .into_iter()
        .any(|(character, previous, next)| match character {
            ';' | '\n' => true,
            '|' => next == Some('|'),
            '&' => !matches!(previous, Some('&' | '>')) && !matches!(next, Some('&' | '>')),
            _ => false,
        })
}

/// ADR-0057: the status a call reports (`done`/`blocked`), from its own parsed input;
/// `None` when the input does not parse.
pub fn describe_target(tool: &str, raw: RawInput<'_>) -> Option<&'static str> {
    parse_input(tool, raw).ok().map(|input| input.status.word())
}

/// How a call's result ended, as far as its description cares.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResultStatus {
    Ok,
    Error,
    /// Any other status (denied, cancelled, …): described by its first line only.
    Other,
}

/// A result's description: the one-line summary and, when there is one, the text
/// detail (`lines\t…`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResultSummary {
    pub summary: String,
    pub detail: Option<String>,
}

/// Describe the result of a call from the call's own input and what the model was shown.
pub fn describe_result(
    tool: &str,
    raw: RawInput<'_>,
    status: ResultStatus,
    content: &str,
) -> ResultSummary {
    let first = content.lines().next().unwrap_or_default();
    let plain = |summary: String| ResultSummary {
        summary,
        detail: None,
    };
    match status {
        ResultStatus::Error => return plain(format!("rejected · {first}")),
        ResultStatus::Other => return plain(first.into()),
        ResultStatus::Ok => {}
    }
    match parse_input(tool, raw) {
        Ok(input) => match input.status {
            Status::Done => {
                let commands = input.verification.unwrap_or_default();
                ResultSummary {
                    summary: format!("verified · {}", commands.join(", ")),
                    detail: Some(format!(
                        "lines\t{}",
                        commands
                            .iter()
                            .map(|cmd| format!("✓ {cmd}"))
                            .collect::<Vec<_>>()
                            .join("\n")
                    )),
                }
            }
            Status::Blocked => ResultSummary {
                summary: format!("blocked · needs {}", input.needs.unwrap_or_default()),
                detail: Some(format!("lines\t{}", input.summary)),
            },
        },
        Err(_) => plain(first.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalise_trims_collapses_and_drops_one_leading_cd() {
        assert_eq!(normalise_command("  cargo   test  "), "cargo test");
        assert_eq!(
            normalise_command("cd /w/x && cargo fmt --check"),
            "cargo fmt --check"
        );
        assert_eq!(normalise_command("cargo fmt --check"), "cargo fmt --check");
        assert_eq!(normalise_command("cd a && cd b && x"), "cd b && x");
        assert_eq!(normalise_command("cd a &&  cd b && x"), "cd b && x");
        assert_eq!(normalise_command("cdx a && b"), "cdx a && b");
        assert_eq!(normalise_command("cd && b"), "cd && b");
    }

    #[test]
    fn pipe_detection_ignores_quoted_and_double_pipes() {
        assert!(is_piped("cargo test 2>&1 | tail -5"));
        assert!(is_piped("grep x f | wc -l"));
        assert!(!is_piped("a || b"));
        assert!(!is_piped("echo 'a|b'"));
        assert!(!is_piped("echo \"a|b\""));
        assert!(!is_piped("cargo test"));
    }

    #[test]
    fn masked_detection_sees_sequencing_and_background_runs() {
        assert!(is_masked("cargo test; echo done"));
        assert!(is_masked("cargo test || true"));
        assert!(is_masked("cargo test &"));
        assert!(is_masked("cargo test\ncargo fmt --check"));
        assert!(is_masked("cd /w && cargo test; echo done"));
        assert!(!is_masked("cargo fmt --check && cargo test"));
        assert!(!is_masked("cd /w && cargo test"));
        assert!(!is_masked("echo \"a;b\""));
        assert!(!is_masked("echo 'x || y'"));
        assert!(!is_masked("cargo test"));
    }

    #[test]
    fn a_redirected_stream_is_not_backgrounding() {
        // An `&` that belongs to a redirection leaves the exit code to the check.
        assert!(!is_masked("cargo test 2>&1"));
        assert!(!is_masked("cargo test >&2"));
        assert!(!is_masked("cargo test &> log.txt"));
        assert!(!is_masked("cargo test &>> log.txt"));
        assert!(!is_masked("cargo test 2>&1 | tail -5"));
        // Backgrounding still masks.
        assert!(is_masked("cargo test &"));
        assert!(is_masked("cargo test & echo x"));
    }

    fn run(command: &str, exit_code: i32, order: u64) -> ShellRun {
        ShellRun {
            command: command.into(),
            exit_code: Some(exit_code),
            order,
        }
    }

    /// The last run decides: a failing re-run invalidates an earlier success, and a run
    /// older than the last file change is stale.
    #[test]
    fn the_last_run_decides_and_a_file_change_makes_it_stale() {
        let runs = vec![run("cargo test", 0, 1), run("cargo test", 1, 2)];
        assert!(command_failure("cargo test", &runs, None).is_some());
        let runs = vec![run("cargo test", 0, 1)];
        assert_eq!(command_failure("cargo test", &runs, None), None);
        assert!(command_failure("cargo test", &runs, Some(2)).is_some());
        assert!(command_failure("cargo build", &runs, None).is_some());
    }

    #[test]
    fn the_description_follows_the_policy_and_the_contract() {
        let contract = OutputContract::new(serde_json::json!({"type": "object"})).unwrap();
        assert_eq!(
            description(CompletionPolicy::RecordedCommands, None),
            DESCRIPTION
        );
        assert_eq!(
            description(CompletionPolicy::ReportToParent, Some(&contract)),
            format!("{REPORT_DESCRIPTION}{STRUCTURED_RESULT_PARAGRAPH}")
        );
        assert!(input_schema(None)["properties"].get("result").is_none());
        assert_eq!(
            input_schema(Some(&contract))["properties"]["result"]["description"],
            "Required with \"done\": "
        );
    }
}
