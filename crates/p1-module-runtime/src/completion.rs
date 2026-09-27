//! The host side of the `completion` import (`modules/wit/session.wit`, interface
//! `completion`): the values that cross it and the [`CompletionService`] a caller passes in
//! ([`Services::completion`](crate::Services::completion)), as `summary` is linked for a
//! context policy.
//!
//! The service is the host's completion hub (`CompletionHub` in `p1-host`, ADR-0083 §2): it
//! owns the session record, the policy, the output contract and the accepted cell. This
//! file only adapts it; `accept` hands the service a CANDIDATE, which the hub re-verifies
//! against its own record before anything commits, so nothing here decides a completion.
//! The frozen `accept` returns nothing, so the component never learns the decision.
//!
//! Every function of the interface is synchronous in the WIT, so the trait is too; the
//! import is still linked asynchronously, like every import of this runtime.

use wasmtime::bail;
use wasmtime::component::{Linker, Val};

use crate::capabilities::CallState;
use crate::loader::interface_import;

/// Which completion rule applies to the agent (`completion.completion-policy`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompletionPolicy {
    /// `done` needs a successful command run recorded after the last file change.
    RecordedCommands,
    /// `done` may pass `["none"]` after a file change; the parent is told it is unverified.
    ReportToParent,
}

/// One finished `executes` call of the session (`completion.shell-run`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShellRun {
    /// The command as the model passed it.
    pub command: String,
    /// The parsed exit code; `None` never counts as success.
    pub exit_code: Option<i32>,
    /// Increasing order of the finished call within the session.
    pub order: u64,
}

/// What a candidate `done` claims it established (`completion.evidence`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Evidence {
    /// These commands ran successfully after the last file change.
    CommandsPassed(Vec<String>),
    /// Nothing was established, and why.
    NotRun(String),
}

/// A candidate the component submits (`completion.accepted`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Candidate {
    /// `done`.
    Done {
        /// The model's summary.
        summary: String,
        /// What the component claims it established.
        evidence: Evidence,
    },
    /// `blocked`.
    Blocked {
        /// The model's summary.
        summary: String,
        /// What the model needs.
        needs: String,
        /// What it tried.
        tried: Vec<String>,
    },
}

/// The component's claim about a structured result (`completion.schema-check`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaCheck {
    /// No contract was requested.
    NotRequested,
    /// The value met the contract.
    Passed,
    /// The value failed it, with these errors.
    Failed(Vec<String>),
}

/// A candidate's structured result (`completion.structured-result`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuredResult {
    /// The result as JSON text; `None` only when no contract was requested.
    pub value: Option<String>,
    /// The component's check.
    pub schema: SchemaCheck,
}

/// The native service a module's `completion` capability is linked to: the host's
/// completion hub. A read reports the host's own record; `accept` submits a candidate the
/// service decides on itself.
pub trait CompletionService: Send + Sync {
    /// The order of the last file change, if any (`last-file-change`).
    fn last_file_change(&self) -> Option<u64>;
    /// Every finished `executes` call so far, oldest first (`shell-runs`).
    fn shell_runs(&self) -> Vec<ShellRun>;
    /// The policy the host chose (`policy`).
    fn policy(&self) -> CompletionPolicy;
    /// The output contract's JSON Schema text, if the host set one (`output-contract`).
    fn output_contract(&self) -> Option<String>;
    /// Submits a candidate (`accept`). It commits only if the service's own checks pass.
    fn accept(&self, candidate: Candidate, structured: Option<StructuredResult>);
}

/// Links the `completion` interface to the call's [`CompletionService`]. Called by
/// [`capability_linker`](crate::capabilities::capability_linker) only when the manifest
/// grants `completion` and a service was given.
pub(crate) fn link_completion(linker: &mut Linker<CallState>) -> wasmtime::Result<()> {
    let mut completion = linker.instance(&interface_import("completion"))?;
    completion.func_wrap_async("last-file-change", |store, (): ()| {
        let change = store
            .data()
            .completion
            .as_ref()
            .map(|service| service.last_file_change());
        Box::new(async move {
            match change {
                Some(change) => Ok((change,)),
                None => bail!("completion.last-file-change called without a completion service"),
            }
        })
    })?;
    completion.func_new_async("shell-runs", |store, _ty, _params, results| {
        let runs = store
            .data()
            .completion
            .as_ref()
            .map(|service| service.shell_runs());
        Box::new(async move {
            let Some(runs) = runs else {
                bail!("completion.shell-runs called without a completion service");
            };
            results[0] = Val::List(runs.into_iter().map(run_val).collect());
            Ok(())
        })
    })?;
    completion.func_new_async("policy", |store, _ty, _params, results| {
        let policy = store
            .data()
            .completion
            .as_ref()
            .map(|service| service.policy());
        Box::new(async move {
            let Some(policy) = policy else {
                bail!("completion.policy called without a completion service");
            };
            results[0] = Val::Enum(
                match policy {
                    CompletionPolicy::RecordedCommands => "recorded-commands",
                    CompletionPolicy::ReportToParent => "report-to-parent",
                }
                .to_owned(),
            );
            Ok(())
        })
    })?;
    completion.func_wrap_async("output-contract", |store, (): ()| {
        let contract = store
            .data()
            .completion
            .as_ref()
            .map(|service| service.output_contract());
        Box::new(async move {
            match contract {
                Some(contract) => Ok((contract,)),
                None => bail!("completion.output-contract called without a completion service"),
            }
        })
    })?;
    completion.func_new_async("accept", |store, _ty, params, _results| {
        let service = store.data().completion.clone();
        let submitted =
            candidate(&params[0]).and_then(|candidate| Ok((candidate, structured(&params[1])?)));
        Box::new(async move {
            let Some(service) = service else {
                bail!("completion.accept called without a completion service");
            };
            // A malformed value traps the call: the WIT types make it impossible for a
            // well-formed component, so it is never read as a candidate.
            let (candidate, structured) = submitted?;
            service.accept(candidate, structured);
            Ok(())
        })
    })
}

fn run_val(run: ShellRun) -> Val {
    Val::Record(vec![
        ("command".to_owned(), Val::String(run.command)),
        (
            "exit-code".to_owned(),
            Val::Option(run.exit_code.map(|code| Box::new(Val::S32(code)))),
        ),
        ("order".to_owned(), Val::U64(run.order)),
    ])
}

/// The named fields of a record, or a trap naming what was expected.
fn fields<'a>(value: &'a Val, what: &str) -> wasmtime::Result<&'a [(String, Val)]> {
    match value {
        Val::Record(fields) => Ok(fields),
        _ => bail!("completion.accept: {what} is not a record"),
    }
}

fn field<'a>(fields: &'a [(String, Val)], name: &str) -> wasmtime::Result<&'a Val> {
    fields
        .iter()
        .find(|(key, _)| key == name)
        .map(|(_, value)| value)
        .ok_or_else(|| wasmtime::format_err!("completion.accept: the field {name} is missing"))
}

fn string(value: &Val, what: &str) -> wasmtime::Result<String> {
    match value {
        Val::String(text) => Ok(text.clone()),
        _ => bail!("completion.accept: {what} is not a string"),
    }
}

fn strings(value: &Val, what: &str) -> wasmtime::Result<Vec<String>> {
    match value {
        Val::List(items) => items.iter().map(|item| string(item, what)).collect(),
        _ => bail!("completion.accept: {what} is not a list"),
    }
}

fn candidate(value: &Val) -> wasmtime::Result<Candidate> {
    let Val::Variant(case, Some(payload)) = value else {
        bail!("completion.accept: accepted is not a variant with a payload");
    };
    let record = fields(payload, case)?;
    let summary = string(field(record, "summary")?, "summary")?;
    match case.as_str() {
        "done" => {
            let evidence = match field(record, "evidence")? {
                Val::Variant(case, Some(payload)) if case == "commands-passed" => {
                    Evidence::CommandsPassed(strings(payload, "commands-passed")?)
                }
                Val::Variant(case, Some(payload)) if case == "not-run" => {
                    Evidence::NotRun(string(payload, "not-run")?)
                }
                _ => bail!("completion.accept: evidence is not a known case"),
            };
            Ok(Candidate::Done { summary, evidence })
        }
        "blocked" => Ok(Candidate::Blocked {
            summary,
            needs: string(field(record, "needs")?, "needs")?,
            tried: strings(field(record, "tried")?, "tried")?,
        }),
        other => bail!("completion.accept: accepted case {other} is not known"),
    }
}

fn structured(value: &Val) -> wasmtime::Result<Option<StructuredResult>> {
    let Val::Option(value) = value else {
        bail!("completion.accept: structured is not an option");
    };
    let Some(value) = value.as_deref() else {
        return Ok(None);
    };
    let record = fields(value, "structured")?;
    let json = match field(record, "value")? {
        Val::Option(None) => None,
        Val::Option(Some(text)) => Some(string(text, "value")?),
        _ => bail!("completion.accept: value is not an option"),
    };
    let schema = match field(record, "schema")? {
        Val::Variant(case, None) if case == "not-requested" => SchemaCheck::NotRequested,
        Val::Variant(case, None) if case == "passed" => SchemaCheck::Passed,
        Val::Variant(case, Some(errors)) if case == "failed" => {
            SchemaCheck::Failed(strings(errors, "failed")?)
        }
        _ => bail!("completion.accept: schema is not a known case"),
    };
    Ok(Some(StructuredResult {
        value: json,
        schema,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_done_candidate_is_read_by_field_name() {
        let value = Val::Variant(
            "done".to_owned(),
            Some(Box::new(Val::Record(vec![
                ("summary".to_owned(), Val::String("s".to_owned())),
                (
                    "evidence".to_owned(),
                    Val::Variant(
                        "commands-passed".to_owned(),
                        Some(Box::new(Val::List(vec![Val::String(
                            "cargo test".to_owned(),
                        )]))),
                    ),
                ),
            ]))),
        );
        assert_eq!(
            candidate(&value).unwrap(),
            Candidate::Done {
                summary: "s".to_owned(),
                evidence: Evidence::CommandsPassed(vec!["cargo test".to_owned()]),
            }
        );
        assert!(candidate(&Val::Variant("done".to_owned(), None)).is_err());
    }

    #[test]
    fn a_structured_result_is_optional_and_read_by_case() {
        assert_eq!(structured(&Val::Option(None)).unwrap(), None);
        let failed = Val::Option(Some(Box::new(Val::Record(vec![
            (
                "value".to_owned(),
                Val::Option(Some(Box::new(Val::String("{}".to_owned())))),
            ),
            (
                "schema".to_owned(),
                Val::Variant(
                    "failed".to_owned(),
                    Some(Box::new(Val::List(vec![Val::String("e".to_owned())]))),
                ),
            ),
        ]))));
        assert_eq!(
            structured(&failed).unwrap(),
            Some(StructuredResult {
                value: Some("{}".to_owned()),
                schema: SchemaCheck::Failed(vec!["e".to_owned()]),
            })
        );
    }
}
