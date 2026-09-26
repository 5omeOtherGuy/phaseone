//! The `finish` tool: completion as an OBSERVABLE ACT instead of a phrase.
//!
//! An unattended model ends its work by calling this tool. The tool does NOT take
//! the model's word for it: it reads the session through the [`SessionActivity`]
//! trait (implemented by the host from its event stream) and refuses `done` until
//! each named verification command really ran, succeeded, and ran after the last
//! file change. A `blocked` call records what the model needs and stops the run.
//!
//! The accepted outcome is stored in a shared [`FinishOutcome`] cell the host
//! reads after the turn; a rejected call stores nothing. Invalid input is an
//! ordinary tool result the model can act on — never a panic.
//!
//! A host may also give the tool an [`OutputContract`] (ADR-0053 item 5): then an
//! accepted `done` must carry `result`, checked in Rust after the `done` is accepted,
//! and the verdict travels next to the outcome in the same cell.
//!
//! Every rule and every text is `p1-finish-guest`'s, the code the `p1/finish` component
//! ships (decision S0-R3); this crate is the native adapter that runs it over the host's
//! session view and stores what it accepts.

use std::sync::{Arc, Mutex};

use p1_contracts::tool::{ResultDescription, ResultDetail};
use p1_contracts::{
    BoxFuture, CallDescription, DeclarationKind, Effect, Tool, ToolCall, ToolContext,
    ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
pub use p1_finish_guest::{
    Accepted, CompletionPolicy, Evidence, OutputContract, SchemaCheck, ShellRun, StructuredResult,
};
use p1_finish_guest::{RawInput, Record, ResultStatus};

/// What the `finish` tool can see of the session so far. Implemented by the host
/// from the event stream it already receives.
pub trait SessionActivity: Send + Sync {
    /// `order` of the last finished tool call whose effect was `WritesFiles`, if any.
    fn last_file_change(&self) -> Option<u64>;
    /// Every finished `Executes` call so far, oldest first.
    fn shell_runs(&self) -> Vec<ShellRun>;
}

/// Both values an accepted call writes, under ONE lock, so the host can never read a
/// `get` and a `structured` that belong to different calls.
#[derive(Default)]
struct OutcomeCell {
    accepted: Option<Accepted>,
    structured: Option<StructuredResult>,
}

/// The shared cell the host reads after a turn. Cheap to clone; all clones share
/// one value. The tool writes it; the host reads and clears it.
#[derive(Clone, Default)]
pub struct FinishOutcome {
    inner: Arc<Mutex<OutcomeCell>>,
}

impl FinishOutcome {
    /// The last accepted outcome, if any.
    pub fn get(&self) -> Option<Accepted> {
        self.inner.lock().unwrap().accepted.clone()
    }

    /// The `result` of the last accepted `done` with its check; `None` after an
    /// accepted `blocked`, after [`FinishOutcome::clear`], or before any call.
    pub fn structured(&self) -> Option<StructuredResult> {
        self.inner.lock().unwrap().structured.clone()
    }

    /// Drop both values, so an earlier turn cannot end a later one.
    pub fn clear(&self) {
        let mut cell = self.inner.lock().unwrap();
        cell.accepted = None;
        cell.structured = None;
    }

    /// Store an accepted call; the last one wins. The native tool writes what it
    /// accepted; for a component, only the host's completion hub writes here, after it
    /// re-verified the candidate itself (ADR-0083 §2).
    pub fn set(&self, accepted: Accepted, structured: Option<StructuredResult>) {
        let mut cell = self.inner.lock().unwrap();
        cell.accepted = Some(accepted);
        cell.structured = structured;
    }
}

pub use p1_contracts::tool::ToolFace;

/// The `finish` tool. Holds the session view, the outcome cell, the completion policy
/// and the optional output contract the host chose for this agent.
pub struct FinishTool {
    activity: Arc<dyn SessionActivity>,
    outcome: FinishOutcome,
    policy: CompletionPolicy,
    contract: Option<OutputContract>,
    name: String,
    /// `Some` once the host pinned the description with [`FinishTool::with_face`]: the
    /// environment's own words replace the policy's, and never gain the contract
    /// paragraph. `None` means the description follows the policy.
    description_override: Option<String>,
    identity: ToolIdentity,
    declaration: ToolDeclaration,
}

impl FinishTool {
    /// Build the tool with the default (`finish`, Claude-family) face and the
    /// default policy, [`CompletionPolicy::RecordedCommands`].
    pub fn new(activity: Arc<dyn SessionActivity>, outcome: FinishOutcome) -> Self {
        let face = default_face();
        Self {
            activity,
            outcome,
            policy: CompletionPolicy::RecordedCommands,
            contract: None,
            name: face.name.clone(),
            description_override: None,
            identity: identity("claude"),
            declaration: declaration(face, None),
        }
    }

    /// Apply the policy the host chose for this agent (ADR-0051 item 1). The
    /// model-facing description follows the policy, because it is what tells the
    /// model which completion rule applies to it; a later [`FinishTool::with_face`]
    /// still overrides it.
    pub fn with_policy(mut self, policy: CompletionPolicy) -> Self {
        self.policy = policy;
        self.refresh();
        self
    }

    /// Require a structured `result` with `done`, checked against `contract`
    /// (ADR-0053 item 5). The input schema gains the contract's schema as `result`,
    /// and the description gains the one paragraph that says so.
    pub fn with_output_contract(mut self, contract: OutputContract) -> Self {
        self.contract = Some(contract);
        self.refresh();
        self
    }

    /// The contract the host set, if any.
    pub fn output_contract(&self) -> Option<&OutputContract> {
        self.contract.as_ref()
    }

    /// Present the same implementation under another name/description and
    /// variant. The input schema and the semantics do not change.
    pub fn with_face(mut self, face: ToolFace, variant: &str) -> Self {
        self.name = face.name;
        self.description_override = Some(face.description);
        self.identity = identity(variant);
        self.refresh();
        self
    }

    /// Rebuild the declaration from the policy, the contract and the face, in ONE
    /// place, so `with_policy`, `with_output_contract` and `with_face` work in any
    /// order.
    fn refresh(&mut self) {
        self.declaration.name = self.name.clone();
        self.declaration.description = self.description();
        self.declaration.kind = DeclarationKind::Function {
            input_schema: p1_finish_guest::input_schema(self.contract.as_ref()),
        };
    }

    /// The policy's own words, plus the contract paragraph when a contract is set. A
    /// description the host pinned with `with_face` stands as it is.
    fn description(&self) -> String {
        if let Some(pinned) = &self.description_override {
            return pinned.clone();
        }
        p1_finish_guest::description(self.policy, self.contract.as_ref())
    }

    /// The session record as the guest checks it, read once per call.
    fn record(&self) -> Record {
        Record {
            last_file_change: self.activity.last_file_change(),
            runs: self.activity.shell_runs(),
        }
    }
}

fn default_face() -> ToolFace {
    ToolFace::new(p1_finish_guest::NAME, p1_finish_guest::DESCRIPTION)
}

fn declaration(face: ToolFace, contract: Option<&OutputContract>) -> ToolDeclaration {
    ToolDeclaration {
        name: face.name,
        description: face.description,
        kind: DeclarationKind::Function {
            input_schema: p1_finish_guest::input_schema(contract),
        },
    }
}

fn identity(variant: &str) -> ToolIdentity {
    ToolIdentity {
        implementation: env!("CARGO_PKG_NAME").to_string(),
        variant: variant.to_string(),
    }
}

fn raw_input(call: &ToolCall) -> RawInput<'_> {
    match &call.input {
        ToolInput::Json(raw) => RawInput::Json(raw),
        ToolInput::Text(raw) => RawInput::Text(raw),
    }
}

impl Tool for FinishTool {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    /// ADR-0057: the status this call reports (`done`/`blocked`), from the tool's
    /// own parsed input.
    fn describe(&self, call: &ToolCall) -> CallDescription {
        CallDescription {
            verb: "finish",
            target: p1_finish_guest::describe_target(&self.declaration.name, raw_input(call))
                .map(str::to_string),
            edit: None,
            destructive: false,
        }
    }

    fn describe_result(&self, call: &ToolCall, result: &ToolResultItem) -> ResultDescription {
        let status = match result.status {
            ToolStatus::Ok => ResultStatus::Ok,
            ToolStatus::Error => ResultStatus::Error,
            _ => ResultStatus::Other,
        };
        let described = p1_finish_guest::describe_result(
            &self.declaration.name,
            raw_input(call),
            status,
            &result.content,
        );
        ResultDescription {
            summary: described.summary,
            detail: described.detail.map(ResultDetail::Text),
        }
    }

    fn execute<'a>(
        &'a self,
        call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            let tool = &self.declaration.name;
            let input = match p1_finish_guest::parse_input(tool, raw_input(call)) {
                Ok(input) => input,
                Err(message) => return ToolOutcome::error(message),
            };
            match p1_finish_guest::evaluate(
                tool,
                input,
                self.policy,
                self.contract.as_ref(),
                &self.record(),
            ) {
                Ok(verdict) => {
                    self.outcome.set(verdict.accepted, verdict.structured);
                    ToolOutcome::ok(verdict.reply)
                }
                Err(message) => ToolOutcome::error(message),
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_contracts::CancellationToken;

    /// A session with no recorded activity, for the description test.
    struct NoActivity;

    impl SessionActivity for NoActivity {
        fn last_file_change(&self) -> Option<u64> {
            None
        }
        fn shell_runs(&self) -> Vec<ShellRun> {
            Vec::new()
        }
    }

    fn call(raw: &str) -> ToolCall {
        ToolCall {
            call_id: "c1".into(),
            name: "finish".into(),
            input: ToolInput::Json(raw.into()),
        }
    }

    /// ADR-0057: the status this call reports, from this tool's own parsed input.
    #[test]
    fn describe_names_the_reported_status() {
        let tool = FinishTool::new(Arc::new(NoActivity), FinishOutcome::default());
        assert_eq!(
            tool.describe(&call(
                r#"{"status":"done","summary":"x","verification":["cargo test"]}"#
            )),
            CallDescription {
                verb: "finish",
                target: Some("done".into()),
                edit: None,
                destructive: false,
            }
        );
        assert_eq!(
            tool.describe(&call(
                r#"{"status":"blocked","summary":"x","needs":"edit"}"#
            ))
            .target
            .as_deref(),
            Some("blocked")
        );
    }

    #[tokio::test]
    async fn describes_real_accepted_result_without_host_parsing_input() {
        let tool = FinishTool::new(Arc::new(NoActivity), FinishOutcome::default());
        let call = call(r#"{"status":"blocked","summary":"cannot write","needs":"edit tool"}"#);
        assert!(!tool.describe(&call).destructive);
        let outcome = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        let result = ToolResultItem {
            call_id: "c1".into(),
            name: "finish".into(),
            status: outcome.status,
            content: outcome.content,
        };
        assert_eq!(
            tool.describe_result(&call, &result),
            ResultDescription {
                summary: "blocked · needs edit tool".into(),
                detail: Some(ResultDetail::Text("lines\tcannot write".into())),
            }
        );
    }
}
