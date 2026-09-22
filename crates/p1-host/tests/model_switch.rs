//! Switching the model of a running interactive session (ADR-0049 stage 3, line
//! mode: `docs/design/model-selection.md` §3 and §4): `/model` and `/effort`.
//!
//! Every assertion is on a provider request, a journal record, a tool result or a
//! host line — never on assistant prose. No test reads the real home or config: the
//! harness points `HOME` and `XDG_CONFIG_HOME` at scratch directories, and every
//! environment, route and profile it loads is written into a tempdir.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Harness, run_args};
use futures_util::StreamExt;
use p1_assembly::{Catalog, ProviderSpec};
use p1_contracts::{
    AssistantBlock, BoxFuture, CacheKeySupport, CancellationToken, Effort, Item, Origin, Outcome,
    Provider, ProviderError, ProviderRequest, ProviderStream, RecordBody, RouteDescription,
    StreamEvent,
};
use p1_host::models::Model;
use p1_testkit::{ScriptedProvider, json_call, text_response, tool_call_response};
use tempfile::TempDir;

// ------------------------------------------------------------------ fixtures

const ENVIRONMENT_ONE: &str = r#"
route   = "route-one"
profile = "p-one"

[options]
reasoning_effort = "low"

[[tools]]
module = "write"
[[tools]]
module = "shell"
[[tools]]
module = "finish"
"#;

const ENVIRONMENT_TWO: &str = r#"
route   = "route-two"
profile = "p-two"

[options]
reasoning_effort = "medium"

[[tools]]
module = "write"
[[tools]]
module = "shell"
[[tools]]
module = "finish"
"#;

/// The same route and profile as `e-one`, but with NO `finish` tool: a session
/// that starts here has no completion of its own to keep.
const ENVIRONMENT_THREE: &str = r#"
route   = "route-one"
profile = "p-one"

[options]
reasoning_effort = "low"

[[tools]]
module = "write"
[[tools]]
module = "shell"
"#;

const ROUTE_ONE: &str = r#"
id           = "route-one"
origin_route = "openai-chat/one"
adapter      = "openai-chat"
endpoint     = "https://example.invalid/v1/chat/completions"

[credential]
kind = "api-key"
env  = "ONE_API_KEY"

[adapter_settings]
dialect = "thinking-with-reasoning-alias"

[models."p-one"]
wire_model = "wire-one"
[models."p-two"]
wire_model = "wire-two"
"#;

const ROUTE_TWO: &str = r#"
id           = "route-two"
origin_route = "openai-chat/two"
adapter      = "openai-chat"
endpoint     = "https://example.invalid/v1/chat/completions"

[credential]
kind = "api-key"
env  = "TWO_API_KEY"

[adapter_settings]
dialect = "thinking-with-reasoning-alias"

[models."p-two"]
wire_model = "wire-two"
"#;

const PROFILE_ONE: &str = r#"
id             = "p-one"
revision       = 1
model_id       = "p-one-model"
family         = "temp"
thinking       = "enabled"
efforts        = ["low", "high"]
default_effort = "high"
"#;

const PROFILE_TWO: &str = r#"
id             = "p-two"
revision       = 1
model_id       = "p-two-model"
family         = "temp"
thinking       = "enabled"
efforts        = ["low", "medium"]
default_effort = "medium"
"#;

/// A scratch host tree: `<root>/environments`, `<root>/routes`, `<root>/profiles`,
/// plus a scratch `XDG_CONFIG_HOME`. `e-one` runs `route-one` (which binds `p-one`
/// and `p-two`) and `e-two` runs `route-two` (`p-two`), so the session has two
/// routes — two origins — and a profile both environments bind.
struct Scratch {
    root: TempDir,
    config: TempDir,
}

impl Scratch {
    fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        let config = tempfile::tempdir().unwrap();
        write(
            &root.path().join("environments/e-one/environment.toml"),
            ENVIRONMENT_ONE,
        );
        write(&root.path().join("environments/e-one/prompt.md"), "one\n");
        write(
            &root.path().join("environments/e-two/environment.toml"),
            ENVIRONMENT_TWO,
        );
        write(&root.path().join("environments/e-two/prompt.md"), "two\n");
        write(
            &root.path().join("environments/e-three/environment.toml"),
            ENVIRONMENT_THREE,
        );
        write(
            &root.path().join("environments/e-three/prompt.md"),
            "three\n",
        );
        write(&root.path().join("routes/route-one.toml"), ROUTE_ONE);
        write(&root.path().join("routes/route-two.toml"), ROUTE_TWO);
        write(&root.path().join("profiles/p-one.toml"), PROFILE_ONE);
        write(&root.path().join("profiles/p-two.toml"), PROFILE_TWO);
        Self { root, config }
    }

    fn environment_dirs(&self) -> Vec<PathBuf> {
        vec![self.root.path().join("environments")]
    }

    fn route_file(&self, id: &str) -> PathBuf {
        self.root.path().join(format!("routes/{id}.toml"))
    }

    fn settings(&self, text: &str) {
        write(&self.config.path().join("p1/settings.toml"), text);
    }

    /// A harness whose credential locations are this scratch tree, so no test can
    /// read the real home or the real config directory.
    fn harness(&self, lines: &[&str]) -> Harness {
        let mut harness = Harness::new(self.environment_dirs(), lines);
        harness.deps.shell_env = Some(vec![
            ("HOME".into(), self.root.path().as_os_str().to_os_string()),
            (
                "XDG_CONFIG_HOME".into(),
                self.config.path().as_os_str().to_os_string(),
            ),
        ]);
        harness
    }

    /// The catalog hook: each fake provider replaces one route's factory and
    /// describes itself as the REAL route and the WIRE model the SELECTED profile
    /// binds (`RouteFile::binding`), so the journal records what a run on that
    /// model really is.
    fn fakes(&self, entries: &[(&str, ScriptedProvider)]) -> p1_host::catalog::CatalogHook {
        let routes: Vec<(String, p1_host::routes::RouteFile, ScriptedProvider)> = entries
            .iter()
            .map(|(id, provider)| {
                let route = p1_host::routes::load_route(&self.route_file(id))
                    .expect("the test route file loads");
                (route.id.clone(), route, provider.clone())
            })
            .collect();
        Box::new(move |catalog: &mut Catalog| {
            for (id, route, provider) in &routes {
                let route = route.clone();
                let provider = provider.clone();
                catalog.provider(
                    id,
                    Box::new(move |spec: &ProviderSpec| {
                        let profile = spec
                            .profile
                            .clone()
                            .ok_or_else(|| "this route is reached with a profile".to_string())?;
                        let binding = route.binding(&profile.id)?;
                        Ok(Arc::new(Described {
                            inner: provider.clone(),
                            origin: Origin {
                                route: route.origin_route.clone(),
                                model: binding.wire_model.clone(),
                            },
                        }) as Arc<dyn Provider>)
                    }),
                );
            }
        })
    }
}

/// A [`ScriptedProvider`] that describes itself as one concrete route + wire model.
///
/// A real route also builds each response ITEM with that same origin
/// (`route.origin(wire_model)`), which is the model the per-response line names. The
/// scripted provider stamps its own fixed fake origin instead, so the wrapper
/// relabels the finished item here — otherwise the fixture would report a model no
/// route ever served.
struct Described {
    inner: ScriptedProvider,
    origin: Origin,
}

impl Provider for Described {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            origin: self.origin.clone(),
            // This fake route consumes a prompt-cache key (ADR-0039), so every
            // assembly — the start and every switch — must generate one under the
            // host's policy: the parent's ordinal, in this workspace, for this
            // environment.
            cache_key: CacheKeySupport::Optional,
            ..self.inner.describe()
        }
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.inner.validate(request)
    }

    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        let inner = self.inner.clone();
        let origin = self.origin.clone();
        Box::pin(async move {
            let stream = inner.stream(request, cancel).await?;
            let relabelled = stream.map(move |event| match event {
                StreamEvent::Finished(Outcome::Completed(mut response)) => {
                    response.item.origin = origin.clone();
                    StreamEvent::Finished(Outcome::Completed(response))
                }
                other => other,
            });
            Ok(Box::pin(relabelled) as ProviderStream)
        })
    }
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

// ------------------------------------------------------------------ observables

const DONE_RUN: &str = r#"{"status":"done","summary":"wrote it","verification":["true"]}"#;
const DONE_NONE: &str = r#"{"status":"done","summary":"looked","verification":["none"]}"#;

/// The environments the journal committed, in order: the origin (route + wire
/// model) of each `Environment` record.
fn journal_origins(session: &Path) -> Vec<Origin> {
    let loaded = p1_journal::load(session).expect("the session loads");
    loaded
        .records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::Environment { route, .. } => Some(route.origin.clone()),
            _ => None,
        })
        .collect()
}

/// The options of every committed `Environment` record, in order.
fn journal_efforts(session: &Path) -> Vec<Option<Effort>> {
    let loaded = p1_journal::load(session).expect("the session loads");
    loaded
        .records
        .iter()
        .filter_map(|record| match &record.body {
            RecordBody::Environment { options, .. } => Some(options.reasoning_effort),
            _ => None,
        })
        .collect()
}

/// Every `finish` tool result this process produced, in history order.
fn finish_results(provider: &ScriptedProvider) -> Vec<String> {
    provider
        .requests()
        .last()
        .expect("at least one request")
        .history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == "finish" => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

/// The row of the `p1 models` table that names `id`.
fn table_row<'a>(text: &'a str, id: &str) -> &'a str {
    text.lines()
        .find(|line| line.contains(id))
        .unwrap_or_else(|| panic!("no `{id}` row in:\n{text}"))
}

fn user_texts(provider: &ScriptedProvider, request: usize) -> Vec<String> {
    provider.requests()[request]
        .history
        .iter()
        .filter_map(|item| match item {
            Item::User { text } => Some(text.clone()),
            _ => None,
        })
        .collect()
}

/// The per-response usage lines of a run, in order. The host's own `· model: …`
/// line and the totals line are not response lines.
fn model_lines(stderr: &str) -> Vec<String> {
    stderr
        .lines()
        .map(rendered)
        .filter(|line| line.starts_with("model "))
        .map(|line| line.to_string())
        .collect()
}

/// The totals line of a run.
fn total_line(stderr: &str) -> String {
    stderr
        .lines()
        .map(rendered)
        .find(|line| line.starts_with("total"))
        .unwrap_or_else(|| panic!("no totals line in:\n{stderr}"))
        .to_string()
}

/// A line as the renderer wrote it, without the interactive prompt. The loop prints
/// `p1> ` before reading a line, so the rendered line that follows it on the same
/// terminal row carries that prompt.
fn rendered(line: &str) -> &str {
    line.strip_prefix("p1> ").unwrap_or(line)
}

/// The usage every `text_response` reports: nothing.
const UNKNOWN_USAGE: &str = "· in ? (cached ?) · out ? · cost unknown";

// ------------------------------------------------------------------ the switch

/// `/model REF` then a turn: the second provider gets the first turn's history and
/// the journal commits a NEW `Environment` with the new origin before the input.
#[tokio::test]
async fn a_switch_then_a_turn_keeps_the_history_and_journals_the_new_origin() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let session = workspace.path().join("session.jsonl");
    let one = ScriptedProvider::new(vec![text_response("first reply")]);
    let two = ScriptedProvider::new(vec![text_response("second reply")]);
    let mut harness = scratch.harness(&["hello", "/model e-two/p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness
            .stderr
            .text()
            .contains("· model: e-two/p-two:medium"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert_eq!(
        one.requests().len(),
        1,
        "the first turn ran on the first model"
    );

    // The switched model's provider got the first turn's history: the operator's
    // line and the first model's own response, in order.
    let requests = two.requests();
    assert_eq!(requests.len(), 1, "the second turn ran on the second model");
    let seen: Vec<String> = requests[0]
        .history
        .iter()
        .filter_map(|item| match item {
            Item::User { text } => Some(format!("user: {text}")),
            Item::Assistant(item) => item.blocks.iter().find_map(|block| match block {
                AssistantBlock::Text { text } => Some(format!("assistant: {text}")),
                _ => None,
            }),
            _ => None,
        })
        .collect();
    assert_eq!(
        seen,
        vec!["user: hello", "assistant: first reply", "user: go"],
        "the history continues: {:#?}",
        requests[0].history
    );

    assert_eq!(
        journal_origins(&session),
        vec![
            Origin {
                route: "openai-chat/one".into(),
                model: "wire-one".into(),
            },
            Origin {
                route: "openai-chat/two".into(),
                model: "wire-two".into(),
            },
        ],
        "the switched turn commits the new Environment before its input"
    );
}

/// A bare profile resolves within the CURRENT environment (§1 rule 2): `p-two` is
/// bound by both environments, so the session's own one wins — never a guess.
#[tokio::test]
async fn a_bare_profile_resolves_in_the_current_environment() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let session = workspace.path().join("session.jsonl");
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&["hello", "/model p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness.stderr.text().contains("· model: e-one/p-two"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(
        two.requests().is_empty(),
        "the other environment was not chosen"
    );
    assert_eq!(one.requests().len(), 2, "both turns ran on route-one");
    assert_eq!(
        journal_origins(&session),
        vec![
            Origin {
                route: "openai-chat/one".into(),
                model: "wire-one".into(),
            },
            Origin {
                route: "openai-chat/one".into(),
                model: "wire-two".into(),
            },
        ],
        "the same route, the new profile's wire model"
    );
}

/// An unknown reference changes NOTHING: the next turn still goes to the first
/// provider, and no new `Environment` record is committed.
#[tokio::test]
async fn an_unknown_reference_changes_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let session = workspace.path().join("session.jsonl");
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&["hello", "/model nope/nope", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness
            .stderr
            .text()
            .contains("· model not changed: unknown model `nope/nope`"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(two.requests().is_empty());
    assert_eq!(
        one.requests().len(),
        2,
        "the second turn kept the first model"
    );
    assert_eq!(
        journal_origins(&session).len(),
        1,
        "no new Environment record"
    );
}

/// An effort the current profile does not list changes nothing: the model keeps its
/// effort — visible in the request the next turn sends.
#[tokio::test]
async fn an_unsupported_effort_changes_nothing() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let session = workspace.path().join("session.jsonl");
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let mut harness = scratch.harness(&["hello", "/effort max", "go", "/exit"]);
    harness.deps.catalog_hook = Some(scratch.fakes(&[("route-one", one.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness.stderr.text().contains(
            "· model not changed: profile `p-one` does not list effort `max`; it lists low, high"
        ),
        "stderr: {}",
        harness.stderr.text()
    );
    assert_eq!(one.requests().len(), 2);
    assert_eq!(
        one.requests()[1].options.reasoning_effort,
        Some(Effort::Low),
        "the next turn still asks for the session's own effort"
    );
    assert_eq!(journal_efforts(&session), vec![Some(Effort::Low)]);
}

/// `/effort high` changes ONLY the options: the same route, the same model, one
/// more `Environment` record whose options carry the new effort.
#[tokio::test]
async fn effort_changes_only_the_options() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let session = workspace.path().join("session.jsonl");
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&["hello", "/effort high", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "--session",
            session.to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness.stderr.text().contains("· model: e-one/p-one:high"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert!(two.requests().is_empty(), "the model itself did not change");
    assert_eq!(one.requests().len(), 2, "both turns ran on route-one");
    assert_eq!(
        one.requests()[1].options.reasoning_effort,
        Some(Effort::High),
        "the next turn asks for the new effort"
    );
    assert_eq!(
        journal_origins(&session),
        vec![
            Origin {
                route: "openai-chat/one".into(),
                model: "wire-one".into(),
            },
            Origin {
                route: "openai-chat/one".into(),
                model: "wire-one".into(),
            },
        ],
        "same origin, a new Environment record"
    );
    assert_eq!(
        journal_efforts(&session),
        vec![Some(Effort::Low), Some(Effort::High)]
    );
}

/// The switched tool set's `finish` reaches the SAME session activity: the run the
/// FIRST model did still counts, and the file change it made still forbids `["none"]`.
#[tokio::test]
async fn after_a_switch_finish_and_file_changes_are_still_seen() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call(
            "w1",
            "write",
            r#"{"file_path":"out.txt","content":"hi"}"#,
        )]),
        tool_call_response(vec![json_call("s1", "shell", r#"{"command":"true"}"#)]),
        text_response("wrote it and checked"),
    ]);
    let two = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("f1", "finish", DONE_RUN)]),
        tool_call_response(vec![json_call("f2", "finish", DONE_NONE)]),
        text_response("done"),
    ]);
    let mut harness = scratch.harness(&["hello", "/model e-two/p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("out.txt")).unwrap(),
        "hi",
        "the first model's file change really happened"
    );
    let results = finish_results(&two);
    assert_eq!(results.len(), 2, "both finish calls reached the provider");
    assert_eq!(
        results[0], "Finished.",
        "the verification run made BEFORE the switch still counts"
    );
    assert!(
        results[1].starts_with(
            "This session changed files; verify the result with a command before finishing."
        ),
        "the file change made BEFORE the switch is still seen: {}",
        results[1]
    );
}

/// A session whose environment assembles no `finish` has no completion of its own
/// to keep: the switch adopts the one the switched assembly's catalog issues, so the
/// switched `finish` reads the log the host feeds — a run made AFTER the switch
/// counts, which it could not if that log were fed by nobody.
#[tokio::test]
async fn a_switch_into_an_environment_with_finish_adopts_its_completion() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("hello there")]);
    let two = ScriptedProvider::new(vec![
        tool_call_response(vec![json_call("s1", "shell", r#"{"command":"true"}"#)]),
        tool_call_response(vec![json_call("f1", "finish", DONE_RUN)]),
        text_response("done"),
    ]);
    let mut harness = scratch.harness(&["hello", "/model e-two/p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-three",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(
        finish_results(&two),
        vec!["Finished.".to_string()],
        "the switched finish reads the log the host feeds"
    );
}

/// A switch assembles under the host's cache-key policy with the PARENT's ordinal:
/// an effort-only switch keeps the key the start path generated, and a switch to
/// another environment gets the key that environment generates (not a worker's).
#[tokio::test]
async fn a_switch_keeps_the_parents_generated_cache_key() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let two = ScriptedProvider::new(vec![text_response("three")]);
    let mut harness = scratch.harness(&[
        "hello",
        "/effort high",
        "go",
        "/model e-two/p-two",
        "go",
        "/exit",
    ]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let start = one.requests()[0]
        .options
        .cache_key
        .clone()
        .expect("an Optional route is handed a generated key");
    assert!(start.starts_with("p1-"), "{start}");
    assert_eq!(
        one.requests()[1].options.cache_key.as_deref(),
        Some(start.as_str()),
        "the same environment and the parent's ordinal keep the same key"
    );
    let switched = two.requests()[0]
        .options
        .cache_key
        .clone()
        .expect("the switched assembly generated a key too");
    assert!(switched.starts_with("p1-"), "{switched}");
    assert_ne!(
        switched, start,
        "the key names the environment the session switched to"
    );
}

// ------------------------------------------------------------------ the lines

/// Bare `/model` prints the `p1 models` table — the same rows, the same `default`
/// and `scoped` markers `p1 models` prints, and the run's own `--models` scope (not
/// `settings.toml`'s) decides which rows are scoped.
#[tokio::test]
async fn bare_model_prints_the_models_table() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    scratch.settings("default_model = \"e-one/p-one\"\nenabled_models = [\"e-one/*\"]\n");
    let one = ScriptedProvider::new(vec![]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&["/model", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--models",
            "e-two/*",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stderr = harness.stderr.text();
    let default_row = table_row(&stderr, "e-one/p-one");
    assert!(default_row.contains("route-one"), "{default_row}");
    assert!(default_row.ends_with("default"), "{default_row}");
    let scoped_row = table_row(&stderr, "e-two/p-two");
    assert!(scoped_row.contains("route-two"), "{scoped_row}");
    assert!(scoped_row.ends_with("scoped"), "{scoped_row}");
    // Every model of the catalog is listed, not only the scoped ones, and the
    // run's `--models` replaced `settings.toml`'s scope.
    let unscoped_row = table_row(&stderr, "e-one/p-two");
    assert!(unscoped_row.contains("route-one"), "{unscoped_row}");
    assert!(!unscoped_row.contains("scoped"), "{unscoped_row}");
    assert!(one.requests().is_empty(), "no turn ran");
    assert!(two.requests().is_empty(), "no turn ran");
}

/// Any other `/…` line stays what it is today: a prompt for the model.
#[tokio::test]
async fn an_unknown_slash_line_still_reaches_the_model() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("a"), text_response("b")]);
    let mut harness = scratch.harness(&["/help", "/models", "/exit"]);
    harness.deps.catalog_hook = Some(scratch.fakes(&[("route-one", one.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(one.requests().len(), 2, "both lines ran a turn");
    assert_eq!(user_texts(&one, 0), vec!["/help".to_string()]);
    assert_eq!(
        user_texts(&one, 1),
        vec!["/help".to_string(), "/models".to_string()]
    );
}

/// The catalog of the fixture is exactly the models the references use: a typo in a
/// fixture would silently change what the tests exercise.
#[test]
fn the_fixture_offers_the_models_the_tests_switch_between() {
    let scratch = Scratch::new();
    let ids: Vec<String> = p1_host::models::enumerate(&scratch.environment_dirs())
        .unwrap()
        .iter()
        .map(Model::id)
        .collect();
    assert_eq!(
        ids,
        vec![
            "e-one/p-one",
            "e-one/p-two",
            "e-three/p-one",
            "e-three/p-two",
            "e-two/p-two"
        ]
    );
}

// ------------------------------------------------------- the usage lines

/// The per-response line names the route AND the model that produced THAT response:
/// after a switch to another route, the second line carries the new route's label
/// (`<adapter>/<account>`, the form the renderer is built with at start), not the
/// one the session started on.
#[tokio::test]
async fn after_a_cross_route_switch_the_response_line_names_the_new_route() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("first reply")]);
    let two = ScriptedProvider::new(vec![text_response("second reply")]);
    let mut harness = scratch.harness(&["hello", "/model e-two/p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness
            .stderr
            .text()
            .contains("· model: e-two/p-two:medium"),
        "the switch itself succeeded: {}",
        harness.stderr.text()
    );
    assert_eq!(
        model_lines(&harness.stderr.text()),
        vec![
            format!("model openai-chat/one/wire-one {UNKNOWN_USAGE}"),
            format!("model openai-chat/two/wire-two {UNKNOWN_USAGE}"),
        ],
        "stderr: {}",
        harness.stderr.text()
    );
}

/// A switch that FAILED left the route label alone: both response lines still name
/// the route the session is assembled on.
#[tokio::test]
async fn a_failed_switch_leaves_the_response_line_on_the_start_route() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&["hello", "/model nope/nope", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert!(
        harness
            .stderr
            .text()
            .contains("· model not changed: unknown model `nope/nope`"),
        "stderr: {}",
        harness.stderr.text()
    );
    assert_eq!(
        model_lines(&harness.stderr.text()),
        vec![
            format!("model openai-chat/one/wire-one {UNKNOWN_USAGE}"),
            format!("model openai-chat/one/wire-one {UNKNOWN_USAGE}"),
        ],
        "stderr: {}",
        harness.stderr.text()
    );
}

/// A session whose responses came from TWO routes cannot name one of them: the
/// totals line drops the `model <route>/<model>` part entirely.
#[tokio::test]
async fn a_two_route_session_totals_line_names_no_single_model() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("first reply")]);
    let two = ScriptedProvider::new(vec![text_response("second reply")]);
    let mut harness = scratch.harness(&["hello", "/model e-two/p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stderr = harness.stderr.text();
    assert!(
        !stderr.contains("total model"),
        "the totals line named a model: {stderr}"
    );
    assert_eq!(total_line(&stderr), format!("total {UNKNOWN_USAGE}"));
}

/// The same on ONE route: a switch between two profiles of it is two models, so the
/// totals line names neither — the per-response lines still name their own route.
#[tokio::test]
async fn a_two_model_session_on_one_route_totals_line_names_no_single_model() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let two = ScriptedProvider::new(vec![]);
    let mut harness = scratch.harness(&["hello", "/model p-two", "go", "/exit"]);
    harness.deps.catalog_hook =
        Some(scratch.fakes(&[("route-one", one.clone()), ("route-two", two.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stderr = harness.stderr.text();
    assert_eq!(
        model_lines(&stderr),
        vec![
            format!("model openai-chat/one/wire-one {UNKNOWN_USAGE}"),
            format!("model openai-chat/one/wire-two {UNKNOWN_USAGE}"),
        ],
        "stderr: {stderr}"
    );
    assert!(!stderr.contains("total model"), "{stderr}");
    assert_eq!(total_line(&stderr), format!("total {UNKNOWN_USAGE}"));
}

/// A session on ONE model is unchanged: every response line and the totals line name
/// today's `model <route>/<model>` part, in the same words.
#[tokio::test]
async fn a_one_model_session_names_its_model_on_every_line() {
    let workspace = tempfile::tempdir().unwrap();
    let scratch = Scratch::new();
    let one = ScriptedProvider::new(vec![text_response("one"), text_response("two")]);
    let mut harness = scratch.harness(&["hello", "go", "/exit"]);
    harness.deps.catalog_hook = Some(scratch.fakes(&[("route-one", one.clone())]));

    let code = run_args(
        &mut harness,
        &[
            "--yes",
            "--env",
            "e-one",
            "--workspace",
            workspace.path().to_str().unwrap(),
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let stderr = harness.stderr.text();
    assert_eq!(
        model_lines(&stderr),
        vec![
            format!("model openai-chat/one/wire-one {UNKNOWN_USAGE}"),
            format!("model openai-chat/one/wire-one {UNKNOWN_USAGE}"),
        ],
        "stderr: {stderr}"
    );
    assert_eq!(
        total_line(&stderr),
        format!("total model openai-chat/one/wire-one {UNKNOWN_USAGE}")
    );
}
