//! Redaction at the untrusted WebAssembly boundary (S3.6).
//!
//! `crates/p1-redact` owns the one credential-shape matcher and the `RedactingTool`
//! decorator (ADR-0068) that the composition root wraps every assembled tool in. ADR-0083 §4
//! extends that layer to everything a component produces that reaches the model, the journal
//! or the UI: the declaration description, the call and result descriptions, and (through the
//! outcome) the safe diagnostics a failed module call carries. Masking still happens before
//! anything durable is built, so history, the journal, a summary and the UI only ever see the
//! masked form.
//!
//! Opaque replay payloads are the one thing the layer must NOT touch: they are version-tagged
//! provider data that has to round-trip byte for byte (PLAN.md §9), and `p1-redact` never
//! receives an assistant item, so a thinking signature is never passed to the matcher.
//!
//! These cases drive the built fixture component over the harness ([`Release::with_fixture`],
//! `fake_processes`, `call`) the way `runtime_spike.rs` does, and build every key-shaped value
//! at runtime so no key-shaped literal is committed (`scripts/secret-scan.sh` refuses one).

use std::sync::Arc;
use std::time::Duration;

use p1_context::{ContextConfig, SummarizingContext};
use p1_contracts::{
    AssistantBlock, AssistantItem, BoxFuture, CancellationToken, ContextInput, ContextPolicy,
    DeclarationKind, Effect, Item, ModelOptions, Origin, Prepared, ReplayData, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolOutcome, ToolResultItem, ToolStatus,
};
use p1_module_runtime::{ExecutionLimits, Loader, Services, wasm_tool};
use p1_module_tests::{FIXTURE_NAME, Release, call, fake_processes, within_deadline};
use p1_redact::{MaskCounter, RedactingTool};
use p1_testkit::ScriptedProvider;
use tokio::time::timeout;

/// A bound far below `DEADLOCK_LIMIT`; every case here is immediate, so only a hang reaches it.
const LIMIT: Duration = Duration::from_secs(10);

/// A key-shaped value built at runtime: never a literal in the tree.
fn key() -> String {
    format!("sk-{}", "a".repeat(24))
}

fn context() -> ToolContext {
    ToolContext {
        cancel: CancellationToken::new(),
    }
}

/// Assembles the fixture as `WasmTool` wrapped by the redacting decorator, and the counter the
/// wrapper adds to. Every constructor path is `wasm_tool`, which never returns an unwrapped
/// adapter (ADR-0083 §4).
fn wrapped(loader: &Loader) -> (Arc<dyn Tool>, Arc<MaskCounter>) {
    let module = loader.load(FIXTURE_NAME).expect("load the fixture");
    let counter = Arc::new(MaskCounter::new());
    let tool = wasm_tool(
        &module,
        Services {
            process: Some(fake_processes().0),
            summary: None,
            completion: None,
        },
        ExecutionLimits::default(),
        &counter,
    )
    .expect("the fixture is a tool");
    (tool, counter)
}

/// As [`wrapped`], over a fresh release holding the fixture.
fn fixture_tool(release: &Release) -> (Arc<dyn Tool>, Arc<MaskCounter>) {
    wrapped(&release.loader())
}

/// A native tool the declaration case controls completely.
struct NativeTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl Tool for NativeTool {
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
        Box::pin(async move { ToolOutcome::ok("unused") })
    }
}

#[tokio::test(flavor = "current_thread")]
async fn an_outcome_from_a_component_is_masked_and_counted() {
    within_deadline("an_outcome_from_a_component_is_masked_and_counted", async {
        let release = Release::with_fixture();
        let (tool, counter) = fixture_tool(&release);
        let secret = key();

        let outcome = tool
            .execute(&call(&format!("echo:{secret}")), context())
            .await;

        assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
        assert!(!outcome.content.contains(&secret));
        assert_eq!(outcome.content, "<redacted:sk-:24 chars>");
        assert!(counter.take() > 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_call_description_from_a_component_is_masked() {
    within_deadline("a_call_description_from_a_component_is_masked", async {
        let release = Release::with_fixture();
        let (tool, counter) = fixture_tool(&release);
        let secret = key();

        // The fixture's `describe` puts the mode text in `target`.
        let described = tool.describe(&call(&format!("echo:{secret}")));

        let target = described.target.expect("the fixture describes a target");
        assert!(!target.contains(&secret), "{target}");
        assert_eq!(target, "echo:<redacted:sk-:24 chars>");
        assert!(counter.take() > 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_result_description_from_a_component_is_masked() {
    within_deadline("a_result_description_from_a_component_is_masked", async {
        let release = Release::with_fixture();
        let (tool, counter) = fixture_tool(&release);
        let secret = key();

        // The fixture's `describe_result` summarises the result's first line, which here
        // carries the key.
        let described = tool.describe_result(
            &call("echo:hi"),
            &ToolResultItem {
                call_id: "c1".to_owned(),
                name: "fixture".to_owned(),
                status: ToolStatus::Ok,
                content: format!("{secret}\nsecond line"),
            },
        );

        assert!(
            !described.summary.contains(&secret),
            "{}",
            described.summary
        );
        assert_eq!(described.summary, "<redacted:sk-:24 chars>");
        assert!(counter.take() > 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_declaration_description_is_masked() {
    within_deadline("a_declaration_description_is_masked", async {
        // A native tool is wrapped the same way as a component: the description is masked
        // once, at construction, and the count is added then.
        let secret = key();
        let counter = Arc::new(MaskCounter::new());
        let native = RedactingTool::new(
            Arc::new(NativeTool {
                declaration: ToolDeclaration {
                    name: "native".to_owned(),
                    description: format!("reads {secret}"),
                    kind: DeclarationKind::Freeform { grammar: None },
                },
                identity: ToolIdentity {
                    implementation: "native".to_owned(),
                    variant: "test".to_owned(),
                },
            }),
            counter.clone(),
        );

        assert_eq!(native.declaration().name, "native");
        assert!(!native.declaration().description.contains(&secret));
        assert_eq!(
            native.declaration().description,
            "reads <redacted:sk-:24 chars>"
        );
        assert_eq!(counter.take(), 1);

        // The fixture's declaration is fixed text with no credential shape: it passes through
        // unchanged and adds nothing.
        let release = Release::with_fixture();
        let (fixture, counter) = fixture_tool(&release);
        assert!(
            fixture
                .declaration()
                .description
                .contains("Test fixture tool for the p1 module runtime"),
            "{}",
            fixture.declaration().description
        );
        assert_eq!(counter.take(), 0);
    })
    .await;
}

#[tokio::test(flavor = "current_thread")]
async fn a_module_failure_diagnostic_is_masked() {
    within_deadline("a_module_failure_diagnostic_is_masked", async {
        let release = Release::with_fixture();
        let (tool, counter) = fixture_tool(&release);
        let secret = key();

        // An unknown mode is an error outcome whose text names the input: the failure text
        // reaches the model as a `ToolOutcome` through `WasmTool`, so it is masked like any
        // other outcome (ADR-0083 §4).
        let outcome = tool.execute(&call(&secret), context()).await;

        assert_eq!(outcome.status, ToolStatus::Error, "{}", outcome.content);
        assert!(
            outcome.content.contains("unknown mode"),
            "{}",
            outcome.content
        );
        assert!(!outcome.content.contains(&secret), "{}", outcome.content);
        assert!(
            outcome.content.contains("<redacted:sk-:24 chars>"),
            "{}",
            outcome.content
        );
        assert!(counter.take() > 0);
    })
    .await;
}

/// A config that makes `prepare` summarize: the tool result sits in a unit older than the
/// verbatim tail, so it appears in the transcript the summarizer is handed.
fn summary_config() -> ContextConfig {
    ContextConfig {
        window_tokens: 10_000,
        output_headroom_tokens: 1_000,
        summarize_at_tokens: 1,
        keep_recent_tokens: 0,
        user_verbatim_tokens: 100,
        tool_result_excerpt_chars: 2_000,
    }
}

fn assistant_text(text: &str) -> Item {
    Item::Assistant(AssistantItem {
        origin: Origin {
            route: "fake-route".to_owned(),
            model: "fake-model".to_owned(),
        },
        blocks: vec![AssistantBlock::Text {
            text: text.to_owned(),
        }],
    })
}

fn summary_text(item: &Item) -> String {
    match item {
        Item::User { text } => text.clone(),
        other => panic!("expected a user item, got {other:?}"),
    }
}

async fn prepare(
    policy: &SummarizingContext,
    history: &[Item],
) -> Result<Option<Prepared>, p1_contracts::ContextError> {
    let cancel = CancellationToken::new();
    timeout(
        LIMIT,
        policy.prepare(ContextInput {
            history,
            last_usage: None,
            cancel: &cancel,
        }),
    )
    .await
    .expect("prepare hung")
}

#[tokio::test(flavor = "current_thread")]
async fn a_summary_transcript_from_masked_history_holds_no_key() {
    within_deadline(
        "a_summary_transcript_from_masked_history_holds_no_key",
        async {
            // The host masks a tool's output before p1-core turns it into a history item.
            let release = Release::with_fixture();
            let (tool, _counter) = fixture_tool(&release);
            let tool_secret = key();
            let outcome = tool
                .execute(&call(&format!("echo:{tool_secret}")), context())
                .await;
            assert!(!outcome.content.contains(&tool_secret));

            // Build the items the way p1-core would from the masked outcome, with the result
            // in an older unit so it lands in the transcript, and a newer assistant unit as
            // the verbatim tail.
            let history = vec![
                Item::User {
                    text: "the task".to_owned(),
                },
                assistant_text("first"),
                Item::ToolResult(ToolResultItem {
                    call_id: "c1".to_owned(),
                    name: "fixture".to_owned(),
                    status: outcome.status,
                    content: outcome.content.clone(),
                }),
                assistant_text("second"),
            ];

            // The summarizer's own answer carries a key: p1-context masks it before it becomes
            // a history item (ADR-0068 rule 3).
            let answer_secret = key();
            let provider = Arc::new(ScriptedProvider::new(vec![p1_testkit::text_response(
                &format!("summary holds {answer_secret}"),
            )]));
            let policy = SummarizingContext::new(
                provider.clone(),
                ModelOptions::default(),
                summary_config(),
                "summary prompt".to_owned(),
            )
            .expect("the context policy");

            let prepared = prepare(&policy, &history)
                .await
                .expect("prepare failed")
                .expect("the history summarises");

            // (a) The transcript the summarizer was handed is rendered from the masked history:
            // it holds the masked tool output and never the key.
            let requests = provider.requests();
            let request = requests.last().expect("the summarizer request");
            let transcript = summary_text(
                request
                    .history
                    .first()
                    .expect("the rendered transcript is the one history item"),
            );
            assert!(!transcript.contains(&tool_secret), "{transcript}");
            assert!(
                transcript.contains("<redacted:sk-:24 chars>"),
                "{transcript}"
            );

            // (b) p1-context masked the summarizer's own answer.
            let summary = summary_text(prepared.items.first().expect("the summary item"));
            assert!(!summary.contains(&answer_secret), "{summary}");
            assert!(summary.contains("<redacted:sk-:24 chars>"), "{summary}");
        },
    )
    .await;
}

#[test]
fn opaque_replay_is_left_unmasked() {
    // PLAN.md §9: a replay payload is version-tagged provider data that must round-trip byte
    // for byte. `p1-redact`'s layer only ever sees tool boundary values (a `ToolOutcome`, a
    // declaration, a call or result description); it is never handed an assistant item, so a
    // key-shaped string inside replay data is never passed to the matcher and stays verbatim.
    let secret = key();
    let origin = Origin {
        route: "anthropic-messages/claude".to_owned(),
        model: "claude".to_owned(),
    };
    let item = Item::Assistant(AssistantItem {
        origin: origin.clone(),
        blocks: vec![AssistantBlock::Reasoning {
            text: "thinking".to_owned(),
            replay: Some(ReplayData {
                origin,
                version: 1,
                payload: p1_contracts::serde_json::json!({ "signature": secret }),
            }),
        }],
    });

    let before = p1_contracts::serde_json::to_string(&item).expect("serialize");
    // Nothing the layer exposes takes an assistant item, so the payload is unchanged and the
    // key-shaped string survives in it. Read the serialized item back through the serde path
    // history and the journal use and re-serialize that value, so this proves the round trip.
    let reread: Item = p1_contracts::serde_json::from_str(&before).expect("deserialize");
    let after = p1_contracts::serde_json::to_string(&reread).expect("serialize");
    assert_eq!(after, before);
    assert!(before.contains(&secret), "{before}");
}

#[tokio::test(flavor = "current_thread")]
async fn a_reassembled_component_is_wrapped_again() {
    within_deadline("a_reassembled_component_is_wrapped_again", async {
        let release = Release::with_fixture();
        let loader = release.loader();
        let secret = key();

        // A first assembly and a worker re-grant (the same module loaded and wrapped anew);
        // both go through `wasm_tool`, which never returns an unwrapped adapter.
        let first = wrapped(&loader);
        let second = wrapped(&loader);

        for (tool, counter) in [first, second] {
            let outcome = tool
                .execute(&call(&format!("echo:{secret}")), context())
                .await;
            assert_eq!(outcome.status, ToolStatus::Ok, "{}", outcome.content);
            assert!(!outcome.content.contains(&secret));
            assert_eq!(outcome.content, "<redacted:sk-:24 chars>");
            assert!(counter.take() > 0);
        }
    })
    .await;
}
