//! The `finish` component over the host's completion hub (ADR-0083 §2, S3.7).
//!
//! The component is the built `p1/finish` package, loaded through the host's loader entry
//! point from a test release selected by a `modules.lock`, so its identity is loader-built;
//! the hub is the host's real `CompletionHub`, whose record is fed through the host's
//! `ActivityTee` exactly as a turn's events feed it. What a component could fabricate is
//! played by a stand-in that submits a chosen candidate through the same grant's service and
//! runs through the same generic gate, because the shipped component refuses those calls
//! itself: either way the hub re-verifies, and only what passes its own rules commits.

use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use p1_assembly::ModulesLock;
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    AgentEvent, BoxFuture, CancellationToken, DeclarationKind, Effect, EventSink, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolInput, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_host::activity::{
    ActivityLog, ActivityTee, AgentRole, Completion, CompletionGate, CompletionGrant,
    CompletionHub, CompletionRule, completion_policy, finish_component,
};
use p1_host::catalog::modules::{ModulesError, load_locked_modules};
use p1_module_runtime::completion::{
    Candidate, Evidence as CandidateEvidence, SchemaCheck as CandidateSchema,
    StructuredResult as CandidateStructured,
};
use p1_module_runtime::{
    ExecutionLimits, LinkError, LoadError, LoadedModule, Services, ToolError, wasm_tool,
};
use p1_module_tests::{Release, lock_text, within_deadline};
use p1_redact::MaskCounter;
use p1_testkit::{FakeTool, RecordingEvents};
use p1_tool_finish::{
    Accepted, CompletionPolicy, Evidence, FinishOutcome, FinishTool, OutputContract, SchemaCheck,
    StructuredResult,
};

/// The package directory and manifest name of the component under test.
const PACKAGE: &str = "p1-module-finish";
const NAME: &str = "p1/finish";
/// The module name the test lock gives it.
const MODULE: &str = "finish";

/// The built component and the manifest `scripts/build-modules.sh` wrote for it. A missing
/// artifact fails the case with the instruction; it never skips.
fn artifact() -> (Vec<u8>, Value) {
    let dir = p1_module_tests::fixture_dir()
        .parent()
        .expect("the publish directory")
        .join(PACKAGE);
    let read = |file: &str| {
        let path = dir.join(file);
        std::fs::read(&path).unwrap_or_else(|error| {
            panic!(
                "{} is missing ({error}): run scripts/build-modules.sh first",
                path.display()
            )
        })
    };
    let wasm = read(&format!("{PACKAGE}.wasm"));
    let manifest = serde_json::from_slice(&read(&format!("{PACKAGE}.manifest.json")))
        .expect("the package manifest is JSON");
    (wasm, manifest)
}

/// The release entry of the component: the package manifest's frozen fields, with
/// `capabilities` replaced when a case withholds a grant.
fn entry(manifest: &Value, capabilities: Option<Value>) -> Value {
    json!({
        "name": NAME,
        "digest": manifest["digest"],
        "path": "packages/p1-finish/p1-finish.wasm",
        "kind": manifest["kind"],
        "world": manifest["world"],
        "protocol": manifest["protocol"],
        "capabilities": capabilities.unwrap_or_else(|| manifest["capabilities"].clone()),
        "variant": manifest["variant"],
    })
}

/// Loads the component through the host's entry point: a release holding it and the
/// `modules.lock` that selects it by module name.
fn load(capabilities: Option<Value>) -> Result<LoadedModule, ModulesError> {
    let (wasm, manifest) = artifact();
    let mut release = Release::empty();
    let entry = entry(&manifest, capabilities);
    release.add(entry.clone(), &wasm);
    let lock = ModulesLock::parse(
        &release.root().join("modules.lock"),
        &lock_text(MODULE, &entry),
    )
    .expect("the test lock parses");
    let mut packages = load_locked_modules(&lock, &release.manifest_file())?;
    assert_eq!(packages.len(), 1);
    Ok(packages.remove(0).loaded)
}

fn loaded() -> LoadedModule {
    let module = load(None).expect("the finish component loads");
    assert_eq!(
        module.identity(),
        &ToolIdentity {
            implementation: NAME.to_owned(),
            variant: "claude".to_owned(),
        },
        "the identity is the loader's, from the release manifest"
    );
    module
}

fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

fn finish_call(input: Value) -> ToolCall {
    ToolCall {
        call_id: "finish-1".to_owned(),
        name: "finish".to_owned(),
        input: ToolInput::Json(input.to_string()),
    }
}

fn done(commands: &[&str]) -> ToolCall {
    finish_call(json!({"status": "done", "summary": "did it", "verification": commands}))
}

/// The native shell's identity under another face: a tool that records command evidence.
const EVIDENCE_TOOL: &str = "exec";
/// A tool merely NAMED `shell`, with an identity that records nothing.
const IMPOSTOR_TOOL: &str = "shell";

/// One agent's session: the hub, the agent's record and cell, and the tee the host feeds
/// the record through.
struct Session {
    hub: CompletionHub,
    completion: Completion,
    tee: ActivityTee,
    tools: Vec<Arc<dyn Tool>>,
    next_call: AtomicU32,
}

impl Session {
    /// A session whose assembled tools are the evidence shell (the native shell's
    /// implementation under the face `exec`), a shell impostor and a writer.
    fn new() -> Self {
        Self::with_tools(vec![
            Arc::new(
                FakeTool::new(EVIDENCE_TOOL)
                    .with_identity("p1-tool-shell", "gpt")
                    .with_effect(Effect::Executes),
            ),
            Arc::new(
                FakeTool::new(IMPOSTOR_TOOL)
                    .with_identity("fake-shell", "claude")
                    .with_effect(Effect::Executes),
            ),
            Arc::new(FakeTool::new("write").with_effect(Effect::WritesFiles)),
        ])
    }

    fn with_tools(tools: Vec<Arc<dyn Tool>>) -> Self {
        let hub = CompletionHub::new();
        let completion = hub.issue();
        let tee = ActivityTee::new(
            Arc::new(RecordingEvents::new()),
            completion.log.clone(),
            &tools,
        );
        Self {
            hub,
            completion,
            tee,
            tools,
            next_call: AtomicU32::new(0),
        }
    }

    fn outcome(&self) -> &FinishOutcome {
        &self.completion.outcome
    }

    fn grant(&self, role: AgentRole, contract: Option<OutputContract>) -> CompletionGrant {
        self.hub
            .grant(self.completion.clone(), &self.tools, role, contract)
    }

    /// One finished call of `tool`, fed through the tee as the turn's events are.
    fn call(&self, tool: &str, input: Value, status: ToolStatus, content: &str) {
        let call_id = format!("c{}", self.next_call.fetch_add(1, Ordering::SeqCst));
        self.tee.emit(AgentEvent::ToolStarted {
            call: ToolCall {
                call_id: call_id.clone(),
                name: tool.to_owned(),
                input: ToolInput::Json(input.to_string()),
            },
        });
        self.tee.emit(AgentEvent::ToolFinished {
            result: ToolResultItem {
                call_id,
                name: tool.to_owned(),
                status,
                content: content.to_owned(),
            },
        });
    }

    fn run(&self, tool: &str, command: &str, exit_code: i32) {
        self.call(
            tool,
            json!({ "command": command }),
            ToolStatus::Ok,
            &format!("output\n[exit code: {exit_code}]"),
        );
    }

    fn write(&self) {
        self.call(
            "write",
            json!({"file_path": "a", "content": "x"}),
            ToolStatus::Ok,
            "Wrote a (1 bytes).",
        );
    }
}

/// The shipped component assembled by the finish entry under `grant`.
fn component(module: &LoadedModule, grant: &CompletionGrant) -> Arc<dyn Tool> {
    finish_component(module, grant, &Arc::new(MaskCounter::new())).expect("the component builds")
}

/// A component that ignores the record and submits whatever it is told to, through its
/// grant's service, and says it finished: what the hub must not take at its word.
struct Fabricating {
    grant: CompletionGrant,
    candidate: Candidate,
    structured: Option<CandidateStructured>,
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

fn fabricating(
    grant: &CompletionGrant,
    candidate: Candidate,
    structured: Option<CandidateStructured>,
) -> Arc<dyn Tool> {
    let inner = Arc::new(Fabricating {
        grant: grant.clone(),
        candidate,
        structured,
        declaration: ToolDeclaration {
            name: "finish".to_owned(),
            description: "claims anything".to_owned(),
            kind: DeclarationKind::Function {
                input_schema: json!({"type": "object"}),
            },
        },
        identity: ToolIdentity {
            implementation: "p1/fabricating".to_owned(),
            variant: "claude".to_owned(),
        },
    });
    Arc::new(CompletionGate::new(inner, grant.clone()))
}

impl Tool for Fabricating {
    fn declaration(&self) -> &ToolDeclaration {
        &self.declaration
    }

    fn identity(&self) -> &ToolIdentity {
        &self.identity
    }

    fn effect(&self, _call: &ToolCall) -> Effect {
        Effect::ReadOnly
    }

    fn execute<'a>(
        &'a self,
        _call: &'a ToolCall,
        _context: ToolContext,
    ) -> BoxFuture<'a, ToolOutcome> {
        Box::pin(async move {
            self.grant
                .service()
                .accept(self.candidate.clone(), self.structured.clone());
            ToolOutcome::ok("Finished.")
        })
    }
}

fn commands_passed(commands: &[&str]) -> Candidate {
    Candidate::Done {
        summary: "did it".to_owned(),
        evidence: CandidateEvidence::CommandsPassed(
            commands
                .iter()
                .map(|command| (*command).to_owned())
                .collect(),
        ),
    }
}

fn not_run(reason: &str) -> Candidate {
    Candidate::Done {
        summary: "did it".to_owned(),
        evidence: CandidateEvidence::NotRun(reason.to_owned()),
    }
}

/// The error text a refusal under `rule` starts with.
fn refused_by(rule: CompletionRule) -> String {
    format!(
        "The host did not accept this completion (completion rule {}",
        rule.number()
    )
}

fn assert_refused(outcome: &ToolOutcome, rule: CompletionRule) {
    assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
    assert!(
        outcome.content.starts_with(&refused_by(rule)),
        "the model reads the rule: {}",
        outcome.content
    );
}

fn done_with(commands: &[&str]) -> Accepted {
    Accepted::Done {
        summary: "did it".to_owned(),
        evidence: Evidence::CommandsPassed(commands.iter().map(|c| (*c).to_owned()).collect()),
    }
}

// --- (a) a fake command ---------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_fake_command_is_refused_and_nothing_commits() {
    within_deadline("a_fake_command_is_refused_and_nothing_commits", async {
        let session = Session::new();
        session.run(EVIDENCE_TOOL, "cargo test", 0);
        let grant = session.grant(AgentRole::Worker, None);

        // The shipped component checks the record itself and refuses with the native text.
        let finish = component(&loaded(), &grant);
        let outcome = finish.execute(&done(&["cargo build"]), context()).await;
        assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
        assert!(
            outcome
                .content
                .starts_with("No successful run of `cargo build` is recorded in this session."),
            "{}",
            outcome.content
        );
        assert_eq!(session.outcome().get(), None);

        // A component that submits the fake command anyway is refused by the hub, and the
        // model reads the rule instead of the component's "Finished.".
        let lying = fabricating(&grant, commands_passed(&["cargo build"]), None);
        let outcome = lying.execute(&done(&["cargo build"]), context()).await;
        assert_refused(&outcome, CompletionRule::CommandsPassed);
        assert!(
            outcome.content.contains("cargo build"),
            "{}",
            outcome.content
        );
        assert_eq!(session.outcome().get(), None, "nothing commits");

        // An empty list is no verification either.
        let empty = fabricating(&grant, commands_passed(&[]), None);
        assert_refused(
            &empty.execute(&done(&[]), context()).await,
            CompletionRule::CommandsPassed,
        );
        assert_eq!(session.outcome().get(), None);
    })
    .await;
}

// --- (b) an old command and a failing re-run -----------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_command_older_than_the_last_file_change_is_refused() {
    within_deadline(
        "a_command_older_than_the_last_file_change_is_refused",
        async {
            let session = Session::new();
            session.run(EVIDENCE_TOOL, "cargo test", 0);
            session.write();
            let grant = session.grant(AgentRole::Worker, None);

            let finish = component(&loaded(), &grant);
            let outcome = finish.execute(&done(&["cargo test"]), context()).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
            assert!(
                outcome
                    .content
                    .starts_with("You changed files after running `cargo test`."),
                "{}",
                outcome.content
            );

            let lying = fabricating(&grant, commands_passed(&["cargo test"]), None);
            let outcome = lying.execute(&done(&["cargo test"]), context()).await;
            assert_refused(&outcome, CompletionRule::CommandsPassed);
            assert_eq!(session.outcome().get(), None);

            // Run again after the change: now it counts.
            session.run(EVIDENCE_TOOL, "cargo test", 0);
            let outcome = finish.execute(&done(&["cargo test"]), context()).await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
            assert_eq!(session.outcome().get(), Some(done_with(&["cargo test"])));
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_failing_re_run_invalidates_an_earlier_success() {
    within_deadline("a_failing_re_run_invalidates_an_earlier_success", async {
        let session = Session::new();
        session.run(EVIDENCE_TOOL, "cargo test", 0);
        session.run(EVIDENCE_TOOL, "cargo test", 101);
        let grant = session.grant(AgentRole::Worker, None);

        let finish = component(&loaded(), &grant);
        let outcome = finish.execute(&done(&["cargo test"]), context()).await;
        assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);

        let lying = fabricating(&grant, commands_passed(&["cargo test"]), None);
        assert_refused(
            &lying.execute(&done(&["cargo test"]), context()).await,
            CompletionRule::CommandsPassed,
        );
        assert_eq!(session.outcome().get(), None);
    })
    .await;
}

// --- (c) a run of a tool without records-command-evidence ----------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_run_of_a_tool_without_command_evidence_never_counts() {
    within_deadline(
        "a_run_of_a_tool_without_command_evidence_never_counts",
        async {
            let session = Session::new();
            // A tool NAMED `shell`, presented as the shell, that records nothing: its run is in
            // the record the component reads (`shell-runs` reports every executes call), so the
            // shipped component accepts the call — and the hub refuses it.
            session.run(IMPOSTOR_TOOL, "cargo test", 0);
            let grant = session.grant(AgentRole::Worker, None);
            let finish = component(&loaded(), &grant);

            let outcome = finish.execute(&done(&["cargo test"]), context()).await;
            assert_refused(&outcome, CompletionRule::CommandEvidence);
            assert_eq!(session.outcome().get(), None, "nothing commits");

            // The same command by the evidence tool, under whatever face, counts.
            session.run(EVIDENCE_TOOL, "cargo test", 0);
            let outcome = finish.execute(&done(&["cargo test"]), context()).await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
            assert_eq!(session.outcome().get(), Some(done_with(&["cargo test"])));

            // A later impostor run does not replace the evidence tool's, either way round.
            session.outcome().clear();
            session.run(IMPOSTOR_TOOL, "cargo clippy", 0);
            let lying = fabricating(
                &grant,
                commands_passed(&["cargo test", "cargo clippy"]),
                None,
            );
            assert_refused(
                &lying.execute(&done(&[]), context()).await,
                CompletionRule::CommandEvidence,
            );
            assert_eq!(session.outcome().get(), None);
        },
    )
    .await;
}

// --- (d) re-grant freshness and the restricted path -----------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_candidate_from_before_a_re_grant_cannot_commit() {
    within_deadline("a_candidate_from_before_a_re_grant_cannot_commit", async {
        let session = Session::new();
        session.run(EVIDENCE_TOOL, "cargo test", 0);
        let module = loaded();
        let first = session.grant(AgentRole::Worker, None);
        let previous = component(&module, &first);

        // The re-grant: a new assembly boundary for the same agent and record.
        let second = session.grant(AgentRole::Worker, None);
        let current = component(&module, &second);

        // An instance of the previous assembly: its own check passes, the hub refuses.
        let outcome = previous.execute(&done(&["cargo test"]), context()).await;
        assert_refused(&outcome, CompletionRule::Freshness);
        assert_eq!(session.outcome().get(), None);

        // A call that started before the next re-grant: its window is the second grant's.
        let window = second.open_window();
        let _third = session.grant(AgentRole::Worker, None);
        second
            .service()
            .accept(commands_passed(&["cargo test"]), None);
        assert_eq!(
            window.close().map(|refusal| refusal.rule),
            Some(CompletionRule::Freshness)
        );
        assert_eq!(session.outcome().get(), None);

        // The current assembly's component commits.
        let fourth = session.grant(AgentRole::Worker, None);
        let fresh = component(&module, &fourth);
        let outcome = fresh.execute(&done(&["cargo test"]), context()).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(session.outcome().get(), Some(done_with(&["cargo test"])));
        drop(current);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn the_restricted_path_and_a_call_outside_execute_cannot_commit() {
    within_deadline(
        "the_restricted_path_and_a_call_outside_execute_cannot_commit",
        async {
            let session = Session::new();
            session.run(EVIDENCE_TOOL, "cargo test", 0);
            let grant = session.grant(AgentRole::Worker, None);
            let finish = component(&loaded(), &grant);

            // The restricted path has no capability linked: it describes, it never commits.
            let call = done(&["cargo test"]);
            assert_eq!(finish.effect(&call), Effect::ReadOnly);
            let described = finish.describe(&call);
            assert_eq!(described.verb, "finish");
            assert_eq!(described.target.as_deref(), Some("done"));
            let result = ToolResultItem {
                call_id: call.call_id.clone(),
                name: "finish".to_owned(),
                status: ToolStatus::Ok,
                content: "Finished.".to_owned(),
            };
            assert_eq!(
                finish.describe_result(&call, &result).summary,
                "verified · cargo test"
            );
            assert_eq!(session.outcome().get(), None);

            // A candidate submitted outside any `execute` window is refused.
            grant
                .service()
                .accept(commands_passed(&["cargo test"]), None);
            assert_eq!(session.outcome().get(), None);
        },
    )
    .await;
}

// --- (e) what commits ---------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn a_qualifying_run_commits_the_hubs_own_evidence() {
    within_deadline("a_qualifying_run_commits_the_hubs_own_evidence", async {
        let session = Session::new();
        session.run(EVIDENCE_TOOL, "cd /work &&  cargo   test", 0);
        let grant = session.grant(AgentRole::Worker, None);

        let finish = component(&loaded(), &grant);
        let outcome = finish.execute(&done(&["cargo test"]), context()).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(outcome.content, "Finished.");
        assert_eq!(session.outcome().get(), Some(done_with(&["cargo test"])));
        assert_eq!(
            session.outcome().structured(),
            Some(StructuredResult {
                value: None,
                schema: SchemaCheck::NotRequested,
            })
        );

        // The committed list is the hub's normalised spelling, not the component's text.
        let lying = fabricating(
            &grant,
            commands_passed(&["  cd /x && cargo    test "]),
            None,
        );
        let outcome = lying.execute(&done(&[]), context()).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(session.outcome().get(), Some(done_with(&["cargo test"])));
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn not_run_commits_only_under_rule_3_with_the_hubs_reason() {
    within_deadline(
        "not_run_commits_only_under_rule_3_with_the_hubs_reason",
        async {
            // Recorded commands, no file changed: the hub's reason, not the component's.
            let session = Session::new();
            let grant = session.grant(AgentRole::Worker, None);
            assert_eq!(grant.policy(), CompletionPolicy::RecordedCommands);
            let lying = fabricating(&grant, not_run("trust me, I checked"), None);
            let outcome = lying.execute(&done(&["none"]), context()).await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
            assert_eq!(
                session.outcome().get(),
                Some(Accepted::Done {
                    summary: "did it".to_owned(),
                    evidence: Evidence::NotRun("no file changed".to_owned()),
                })
            );

            // Recorded commands after a file change: refused.
            session.outcome().clear();
            session.write();
            assert_refused(
                &lying.execute(&done(&["none"]), context()).await,
                CompletionRule::NotRun,
            );
            assert_eq!(session.outcome().get(), None);

            // Report to parent: accepted after a file change, with the hub's reason, and the
            // shipped component reads the host's policy to offer it.
            let without_commands = Session::with_tools(vec![Arc::new(
                FakeTool::new("write").with_effect(Effect::WritesFiles),
            )]);
            without_commands.write();
            let grant = without_commands.grant(AgentRole::Worker, None);
            assert_eq!(grant.policy(), CompletionPolicy::ReportToParent);
            let finish = component(&loaded(), &grant);
            let outcome = finish.execute(&done(&["none"]), context()).await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
            assert_eq!(
                without_commands.outcome().get(),
                Some(Accepted::Done {
                    summary: "did it".to_owned(),
                    evidence: Evidence::NotRun("no command tool granted".to_owned()),
                })
            );
        },
    )
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn blocked_needs_non_empty_needs() {
    within_deadline("blocked_needs_non_empty_needs", async {
        let session = Session::new();
        let grant = session.grant(AgentRole::Worker, None);

        let lying = fabricating(
            &grant,
            Candidate::Blocked {
                summary: "stuck".to_owned(),
                needs: "  ".to_owned(),
                tried: Vec::new(),
            },
            None,
        );
        assert_refused(
            &lying.execute(&done(&[]), context()).await,
            CompletionRule::Blocked,
        );
        assert_eq!(session.outcome().get(), None);

        let finish = component(&loaded(), &grant);
        let outcome = finish
            .execute(
                &finish_call(json!({
                    "status": "blocked",
                    "summary": "stuck",
                    "needs": "an edit tool",
                    "tried": ["read"],
                })),
                context(),
            )
            .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(outcome.content, "Recorded as blocked.");
        assert_eq!(
            session.outcome().get(),
            Some(Accepted::Blocked {
                summary: "stuck".to_owned(),
                needs: "an edit tool".to_owned(),
                tried: vec!["read".to_owned()],
            })
        );
        assert_eq!(session.outcome().structured(), None);
    })
    .await;
}

fn id_contract() -> OutputContract {
    OutputContract::new(json!({
        "type": "object",
        "description": "The item.",
        "required": ["id"],
        "properties": {"id": {"type": "string"}},
    }))
    .expect("a valid contract")
}

#[tokio::test(flavor = "current_thread")]
async fn a_structured_result_is_checked_by_the_hub() {
    within_deadline("a_structured_result_is_checked_by_the_hub", async {
        let session = Session::new();
        session.run(EVIDENCE_TOOL, "cargo test", 0);
        let grant = session.grant(AgentRole::Worker, Some(id_contract()));
        let finish = component(&loaded(), &grant);

        // A failing value is still an accepted `done`, with the errors.
        let outcome = finish
            .execute(
                &finish_call(json!({
                    "status": "done",
                    "summary": "did it",
                    "verification": ["cargo test"],
                    "result": {"id": 7},
                })),
                context(),
            )
            .await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert!(
            outcome
                .content
                .starts_with("Finished. The result does not match the schema:"),
            "{}",
            outcome.content
        );
        let failed = StructuredResult {
            value: Some(json!({"id": 7})),
            schema: SchemaCheck::Failed(vec!["$.id: expected string, got integer".to_owned()]),
        };
        assert_eq!(session.outcome().get(), Some(done_with(&["cargo test"])));
        assert_eq!(session.outcome().structured(), Some(failed.clone()));

        // The verdict is the hub's: a component claiming `passed` for that value commits
        // the hub's `failed`.
        session.outcome().clear();
        let lying = fabricating(
            &grant,
            commands_passed(&["cargo test"]),
            Some(CandidateStructured {
                value: Some(r#"{"id":7}"#.to_owned()),
                schema: CandidateSchema::Passed,
            }),
        );
        let outcome = lying.execute(&done(&[]), context()).await;
        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert_eq!(session.outcome().structured(), Some(failed));

        // A `done` without a value when a contract is set is refused.
        session.outcome().clear();
        let without_value = fabricating(&grant, commands_passed(&["cargo test"]), None);
        assert_refused(
            &without_value.execute(&done(&[]), context()).await,
            CompletionRule::StructuredResult,
        );
        assert_eq!(session.outcome().get(), None);

        // A conforming value passes.
        let outcome = finish
            .execute(
                &finish_call(json!({
                    "status": "done",
                    "summary": "did it",
                    "verification": ["cargo test"],
                    "result": {"id": "a"},
                })),
                context(),
            )
            .await;
        assert_eq!(outcome.content, "Finished.");
        assert_eq!(
            session.outcome().structured(),
            Some(StructuredResult {
                value: Some(json!({"id": "a"})),
                schema: SchemaCheck::Passed,
            })
        );
    })
    .await;
}

// --- (f) the policy is the host's -----------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn the_policy_is_the_hosts_and_the_declaration_follows_it() {
    within_deadline(
        "the_policy_is_the_hosts_and_the_declaration_follows_it",
        async {
            let evidence: Arc<dyn Tool> = Arc::new(
                FakeTool::new("run")
                    .with_identity("p1-tool-shell", "claude")
                    .with_effect(Effect::Executes),
            );
            let impostor: Arc<dyn Tool> = Arc::new(
                FakeTool::new("shell")
                    .with_identity("fake-shell", "claude")
                    .with_effect(Effect::Executes),
            );
            assert_eq!(
                completion_policy(std::slice::from_ref(&evidence), AgentRole::Worker),
                CompletionPolicy::RecordedCommands
            );
            assert_eq!(
                completion_policy(std::slice::from_ref(&impostor), AgentRole::Worker),
                CompletionPolicy::ReportToParent,
                "a name or an executes effect is not the capability"
            );
            assert_eq!(
                completion_policy(&[], AgentRole::Main),
                CompletionPolicy::RecordedCommands,
                "a main agent always gets recorded-commands"
            );

            // A component cannot choose it: under recorded-commands after a file change the
            // shipped component refuses `["none"]` itself, and a component claiming
            // `not-run` anyway is refused by the hub.
            let session = Session::new();
            session.write();
            let module = loaded();
            let grant = session.grant(AgentRole::Main, None);
            let finish = component(&module, &grant);
            let outcome = finish.execute(&done(&["none"]), context()).await;
            assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
            assert!(
                outcome.content.starts_with(
                    "This session changed files; verify the result with a command before finishing."
                ),
                "{}",
                outcome.content
            );
            let lying = fabricating(&grant, not_run("no command tool granted"), None);
            assert_refused(
                &lying.execute(&done(&["none"]), context()).await,
                CompletionRule::NotRun,
            );
            assert_eq!(session.outcome().get(), None);

            // The declaration the host presents is the policy's and the contract's,
            // byte-identical to the native tool's.
            let reporting = Session::with_tools(Vec::new());
            for (grant, native) in [
                (
                    reporting.grant(AgentRole::Worker, Some(id_contract())),
                    FinishTool::new(Arc::new(ActivityLog::default()), FinishOutcome::default())
                        .with_policy(CompletionPolicy::ReportToParent)
                        .with_output_contract(id_contract()),
                ),
                (
                    reporting.grant(AgentRole::Main, None),
                    FinishTool::new(Arc::new(ActivityLog::default()), FinishOutcome::default()),
                ),
            ] {
                let presented = component(&module, &grant);
                assert_eq!(presented.declaration(), native.declaration());
                assert_eq!(presented.identity().implementation, NAME);
            }
        },
    )
    .await;
}

// --- (g) grants and services ------------------------------------------------------------------

#[tokio::test(flavor = "current_thread")]
async fn granted_completion_without_a_service_is_a_missing_service() {
    within_deadline(
        "granted_completion_without_a_service_is_a_missing_service",
        async {
            let module = loaded();
            match wasm_tool(
                &module,
                Services::default(),
                ExecutionLimits::default(),
                &Arc::new(MaskCounter::new()),
            ) {
                Err(ToolError::Link {
                    source: LinkError::MissingService(capability),
                    ..
                }) => assert_eq!(capability, "completion"),
                Err(other) => panic!("wrong error: {other}"),
                Ok(_) => panic!("completion must not link without a completion service"),
            }
        },
    )
    .await;
}

#[test]
fn an_import_the_manifest_does_not_grant_is_refused() {
    match load(Some(json!([]))) {
        Err(ModulesError::Load { source, .. }) => match *source {
            LoadError::UndeclaredImport { name, import } => {
                assert_eq!(name, NAME);
                assert!(import.contains("completion"), "{import}");
            }
            other => panic!("wrong refusal: {other}"),
        },
        Err(other) => panic!("wrong error: {other}"),
        Ok(_) => panic!("a component importing an ungranted completion must not load"),
    }
}
