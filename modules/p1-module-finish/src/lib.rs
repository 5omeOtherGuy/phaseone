//! The `finish` tool as a component (`p1/finish`, ADR-0071, ADR-0083 §2).
//!
//! Everything the tool decides is `p1-finish-guest`, the same code the native adapter
//! (`FinishTool` in `p1-tool-finish`) runs; this crate only binds it to the `tool` world and
//! to the `completion` capability. It reads the host's record through `completion`
//! (`policy`, `output-contract`, `last-file-change`, `shell-runs`), checks the call and
//! writes the call's texts as the native tool does, and submits what it accepted with
//! `accept`.
//!
//! What it submits is a CANDIDATE: the host's completion hub re-verifies it against its own
//! record and commits only what passes, with its own evidence and its own schema verdict, and
//! it replaces this call's outcome by an error naming the rule when it refuses. So nothing
//! this component says can end a run by itself.
//!
//! The declaration is read on the restricted path, where no capability is linked, so it is
//! the default one (`recorded-commands`, no output contract); the host presents the policy's
//! and the contract's declaration when it assembles the component.
#![forbid(unsafe_code)]

mod wire;

use p1_bindings_tool::generated::p1::module::completion::{self, CompletionPolicy};
use p1_bindings_tool::generated::p1::module::types::DeclarationKind;
use p1_bindings_tool::generated::{
    CallDescription, CallEffect, Guest, HistoryItem, ResultDescription, ToolCall, ToolDeclaration,
    ToolOutcome,
};
use p1_finish_guest::{
    Accepted, Evidence, OutputContract, Record, ResultStatus, SchemaCheck, ShellRun,
    StructuredResult,
};

struct Finish;

impl Guest for Finish {
    fn declaration() -> ToolDeclaration {
        ToolDeclaration {
            name: p1_finish_guest::NAME.to_owned(),
            description: p1_finish_guest::DESCRIPTION.to_owned(),
            kind: DeclarationKind::Function(p1_finish_guest::input_schema(None).to_string()),
        }
    }

    fn effect(_call: ToolCall) -> CallEffect {
        CallEffect::ReadOnly
    }

    fn describe(call: ToolCall) -> CallDescription {
        // Restricted path: from the input alone.
        let target = serde_json::from_str::<wire::Call>(&call)
            .ok()
            .and_then(|call| p1_finish_guest::describe_target(&call.name, call.input.raw()));
        wire::text(&wire::CallDescription {
            verb: "finish",
            target,
            destructive: false,
        })
    }

    fn describe_result(call: ToolCall, tool_result: HistoryItem) -> ResultDescription {
        let (content, status) = match serde_json::from_str::<wire::ResultItem>(&tool_result) {
            Ok(item) => {
                let status = match item.status.as_str() {
                    "ok" => ResultStatus::Ok,
                    "error" => ResultStatus::Error,
                    _ => ResultStatus::Other,
                };
                (item.content, status)
            }
            Err(_) => (String::new(), ResultStatus::Other),
        };
        let described = match serde_json::from_str::<wire::Call>(&call) {
            Ok(call) => {
                p1_finish_guest::describe_result(&call.name, call.input.raw(), status, &content)
            }
            // An unreadable call is described by what the model was shown, as an input that
            // does not parse is.
            Err(_) => p1_finish_guest::describe_result(
                p1_finish_guest::NAME,
                p1_finish_guest::RawInput::Text(""),
                status,
                &content,
            ),
        };
        wire::text(&wire::ResultDescription {
            summary: described.summary,
            detail: described.detail.map(|text| wire::TextDetail { text }),
        })
    }

    fn execute(call: ToolCall) -> ToolOutcome {
        match run(&call) {
            Ok(reply) => wire::outcome(true, &reply),
            Err(message) => wire::outcome(false, &message),
        }
    }
}

p1_bindings_tool::generated::export!(Finish);

/// One call: parse, read the host's record, check, and submit what was accepted.
fn run(call: &str) -> Result<String, String> {
    let call = serde_json::from_str::<wire::Call>(call)
        .map_err(|error| format!("Invalid input for {}: {error}", p1_finish_guest::NAME))?;
    let input = p1_finish_guest::parse_input(&call.name, call.input.raw())?;
    let policy = match completion::policy() {
        CompletionPolicy::RecordedCommands => p1_finish_guest::CompletionPolicy::RecordedCommands,
        CompletionPolicy::ReportToParent => p1_finish_guest::CompletionPolicy::ReportToParent,
    };
    let contract = match completion::output_contract() {
        None => None,
        Some(text) => Some(contract(&text)?),
    };
    let record = Record {
        last_file_change: completion::last_file_change(),
        runs: completion::shell_runs()
            .into_iter()
            .map(|run| ShellRun {
                command: run.command,
                exit_code: run.exit_code,
                order: run.order,
            })
            .collect(),
    };
    let verdict = p1_finish_guest::evaluate(&call.name, input, policy, contract.as_ref(), &record)?;
    completion::accept(
        &candidate(verdict.accepted),
        verdict.structured.map(structured).as_ref(),
    );
    Ok(verdict.reply)
}

/// The host's contract, as the host sent it. The host validated it before it assembled the
/// agent, so a refusal here means the host and this component disagree, which the model
/// cannot fix; it is reported rather than guessed around.
fn contract(text: &str) -> Result<OutputContract, String> {
    serde_json::from_str(text)
        .map_err(|error| error.to_string())
        .and_then(OutputContract::new)
        .map_err(|error| format!("The host's output contract cannot be read: {error}"))
}

fn candidate(accepted: Accepted) -> completion::Accepted {
    match accepted {
        Accepted::Done { summary, evidence } => completion::Accepted::Done(completion::Done {
            summary,
            evidence: match evidence {
                Evidence::CommandsPassed(commands) => {
                    completion::Evidence::CommandsPassed(commands)
                }
                Evidence::NotRun(reason) => completion::Evidence::NotRun(reason),
            },
        }),
        Accepted::Blocked {
            summary,
            needs,
            tried,
        } => completion::Accepted::Blocked(completion::Blocked {
            summary,
            needs,
            tried,
        }),
    }
}

fn structured(result: StructuredResult) -> completion::StructuredResult {
    completion::StructuredResult {
        value: result.value.map(|value| value.to_string()),
        schema: match result.schema {
            SchemaCheck::NotRequested => completion::SchemaCheck::NotRequested,
            SchemaCheck::Passed => completion::SchemaCheck::Passed,
            SchemaCheck::Failed(errors) => completion::SchemaCheck::Failed(errors),
        },
    }
}
