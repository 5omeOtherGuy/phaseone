//! Journal projection and agent resume.
//!
//! `project` turns a session journal back into the model-visible state the core
//! would have held: history, the next sequence number, whether the environment is
//! committed, and the tool calls of the last assistant item still without a result.
//! `Agent::resume` installs that state; it commits nothing itself. The unresolved
//! calls are answered by the core's existing R5 reconciliation at the start of the
//! next turn, so there is exactly one reconciliation behaviour (journal.md, R5).
//!
//! Nothing old is ever dispatched: resume only rebuilds state and the report.

use std::collections::HashSet;

use p1_contracts::{
    Item, JournalRecord, RecordBody, ToolCall, ToolDeclaration, ToolIdentity, Usage,
};

use crate::{Agent, AgentParts, BuildError};

/// A tool call of the last assistant item that has no `ToolFinished` yet.
#[derive(Debug, Clone, PartialEq)]
pub struct UnresolvedCall {
    pub call: ToolCall,
    /// The identity recorded by a `ToolStarted` for this call, if one exists after
    /// the assistant record: the side effect may have happened, so the outcome is
    /// unknown rather than cancelled.
    pub started: Option<ToolIdentity>,
}

/// The model-visible state a journal projects back to.
#[derive(Debug, Clone, PartialEq)]
pub struct Projection {
    pub history: Vec<Item>,
    pub next_seq: u64,
    pub environment_committed: bool,
    pub unresolved_calls: Vec<UnresolvedCall>,
    /// Usage of the last journalled `AssistantCompleted`, or `None` when there was
    /// none or it reported no usage. Supplied to the context policy on resume.
    pub last_usage: Option<Usage>,
}

/// Why a journal cannot be the output of a core session.
#[derive(Debug, Clone, PartialEq, thiserror::Error)]
pub enum ResumeError {
    #[error("journal is not a core session: the first record must be `Environment`")]
    MissingEnvironment,
    #[error("journal sequence is not dense: expected seq {expected}, got {got}")]
    Sequence { expected: u64, got: u64 },
    #[error("journal has a `ToolFinished` for unknown call id `{call_id}`")]
    UnknownCall { call_id: String },
    /// The assembled parts could not be validated against the projected history.
    /// A changed origin is no longer its own error (ADR-0049): the provider decides,
    /// exactly as it does for a live switch, and says what it cannot carry.
    #[error("the resumed environment is invalid: {0}")]
    Build(#[from] BuildError),
}

/// What changed between the journalled environment and the newly assembled parts.
#[derive(Debug, Clone, PartialEq)]
pub struct ResumeReport {
    pub unresolved_calls: Vec<UnresolvedCall>,
    /// Tool names whose `ToolIdentity` differs from the journalled one.
    pub changed_tools: Vec<String>,
    /// Tool names that were journalled but are no longer assembled.
    pub missing_tools: Vec<String>,
    /// The resolved environment differs and the next turn must commit it again.
    pub environment_changed: bool,
}

/// Rebuild the model-visible state from journal records.
///
/// Rules (journal.md "Projection rules"): `UserInput`, `Inbox`,
/// `AssistantCompleted` and `ToolFinished` append their item; `ContextReplaced`
/// replaces the history; `AssistantInterrupted`, `ToolStarted` and `Environment`
/// append nothing.
pub fn project(records: &[JournalRecord]) -> Result<Projection, ResumeError> {
    if let Some(first) = records.first()
        && !matches!(first.body, RecordBody::Environment { .. })
    {
        return Err(ResumeError::MissingEnvironment);
    }
    let mut history: Vec<Item> = Vec::new();
    let mut last_assistant_record: Option<usize> = None;
    let mut last_assistant_calls: Vec<ToolCall> = Vec::new();
    let mut last_usage: Option<Usage> = None;
    for (index, record) in records.iter().enumerate() {
        if record.seq != index as u64 {
            return Err(ResumeError::Sequence {
                expected: index as u64,
                got: record.seq,
            });
        }
        match &record.body {
            RecordBody::UserInput { text } => history.push(Item::User { text: text.clone() }),
            RecordBody::Inbox { kind, text } => history.push(Item::Inbox {
                kind: *kind,
                text: text.clone(),
            }),
            RecordBody::AssistantCompleted { item, usage, .. } => {
                last_assistant_calls = item.tool_calls().cloned().collect();
                last_assistant_record = Some(index);
                // The most recent completed response wins, and `None` resets it.
                last_usage = *usage;
                history.push(Item::Assistant(item.clone()));
            }
            RecordBody::ToolFinished { result } => {
                // A core only ever finishes a call of its last assistant item.
                if !last_assistant_calls
                    .iter()
                    .any(|call| call.call_id == result.call_id)
                {
                    return Err(ResumeError::UnknownCall {
                        call_id: result.call_id.clone(),
                    });
                }
                history.push(Item::ToolResult(result.clone()));
            }
            RecordBody::ContextReplaced { items, .. } => history = items.clone(),
            RecordBody::AssistantInterrupted { .. }
            | RecordBody::ToolStarted { .. }
            | RecordBody::Environment { .. } => {}
        }
    }
    let unresolved_calls = unresolved_calls(&history, records, last_assistant_record);
    Ok(Projection {
        history,
        next_seq: records.last().map_or(0, |record| record.seq + 1),
        environment_committed: !records.is_empty(),
        unresolved_calls,
        last_usage,
    })
}

/// The tool calls of the history's last assistant item that have no `ToolFinished`,
/// in block order, with the identity of a `ToolStarted` recorded after that
/// assistant record when there is one.
fn unresolved_calls(
    history: &[Item],
    records: &[JournalRecord],
    last_assistant_record: Option<usize>,
) -> Vec<UnresolvedCall> {
    let Some(assistant) = history.iter().rev().find_map(|item| match item {
        Item::Assistant(item) => Some(item),
        _ => None,
    }) else {
        return Vec::new();
    };
    let resolved: HashSet<&str> = history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) => Some(result.call_id.as_str()),
            _ => None,
        })
        .collect();
    // `ToolStarted` records that matter appear after the assistant record that
    // produced the item; earlier ones belong to older, already-finished calls.
    let start = last_assistant_record.map_or(records.len(), |index| index + 1);
    assistant
        .tool_calls()
        .filter(|call| !resolved.contains(call.call_id.as_str()))
        .map(|call| UnresolvedCall {
            call: call.clone(),
            started: records[start..]
                .iter()
                .find_map(|record| match &record.body {
                    RecordBody::ToolStarted { call_id, identity } if call_id == &call.call_id => {
                        Some(identity.clone())
                    }
                    _ => None,
                }),
        })
        .collect()
}

impl Agent {
    /// Build an agent whose state is the projection of `records`.
    ///
    /// Construction is exactly `Agent::new`'s (same `BuildError`s, wrapped), with
    /// the projected history, sequence, environment flag and started calls
    /// installed. The parts are validated against the PROJECTED history — the
    /// transcript this agent would send — so a session resumes on another model or
    /// route exactly when that provider accepts what the journal holds, and a
    /// refusal is `ResumeError::Build(ProviderRejected)` before anything is
    /// committed (ADR-0049). This commits NOTHING: the unresolved calls are
    /// answered by R5 at the start of the next turn, and a changed environment is
    /// committed then, before that turn's input.
    pub fn resume(
        parts: AgentParts,
        records: &[JournalRecord],
    ) -> Result<(Self, ResumeReport), ResumeError> {
        let Projection {
            history,
            next_seq,
            environment_committed,
            unresolved_calls,
            last_usage,
        } = project(records)?;
        let started_calls: HashSet<String> = unresolved_calls
            .iter()
            .filter(|call| call.started.is_some())
            .map(|call| call.call.call_id.clone())
            .collect();
        let report = compare_environment(&parts, records, unresolved_calls)?;
        // A changed environment is re-committed at the next turn's start, before
        // its input, exactly like the first turn of a new agent (spec §2).
        let environment_committed = environment_committed && !report.environment_changed;
        let agent = Self::assemble(
            parts,
            history,
            next_seq,
            environment_committed,
            started_calls,
            last_usage,
        )?;
        Ok((agent, report))
    }
}

/// Compare the LAST journalled `Environment` with the newly assembled parts. A
/// changed origin is an ordinary environment change here: whether the transcript
/// can continue on it is the provider's own `validate`, run by `assemble` against
/// the projected history (ADR-0049).
fn compare_environment(
    parts: &AgentParts,
    records: &[JournalRecord],
    unresolved_calls: Vec<UnresolvedCall>,
) -> Result<ResumeReport, ResumeError> {
    let new_route = parts.provider.describe();
    let new_tools: Vec<(ToolDeclaration, ToolIdentity)> = parts
        .tools
        .iter()
        .map(|tool| (tool.declaration().clone(), tool.identity().clone()))
        .collect();
    for record in records.iter().rev() {
        if let RecordBody::Environment {
            route,
            system_prompt,
            tools,
            options,
        } = &record.body
        {
            let mut changed_tools = Vec::new();
            let mut missing_tools = Vec::new();
            for (declaration, identity) in tools {
                match new_tools
                    .iter()
                    .find(|(new_declaration, _)| new_declaration.name == declaration.name)
                {
                    Some((_, new_identity)) => {
                        if new_identity != identity {
                            changed_tools.push(declaration.name.clone());
                        }
                    }
                    None => missing_tools.push(declaration.name.clone()),
                }
            }
            return Ok(ResumeReport {
                unresolved_calls,
                changed_tools,
                missing_tools,
                // `route` carries the origin, so another route or model is a change
                // like any other: the next turn commits the new `Environment`.
                environment_changed: *route != new_route
                    || *system_prompt != parts.system_prompt
                    || *tools != new_tools
                    || *options != parts.options,
            });
        }
    }
    Ok(ResumeReport {
        unresolved_calls,
        changed_tools: Vec::new(),
        missing_tools: Vec::new(),
        environment_changed: false,
    })
}
