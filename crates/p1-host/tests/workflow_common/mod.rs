//! The scratch host tree every `workflow_*` test runs in: a parent environment on a
//! whole fake provider, and ONE routed environment `fake` whose route binds two
//! profiles, `main` and `other`, so a role names `fake/main` or `fake/other` exactly as
//! a real role names `claude/claude-opus-5-5`. The catalog hook answers the route with
//! a scripted provider per PROFILE, so a test sees which model a step really ran on.
//!
//! No test reads the real home, config or state directory: `HOME`, `XDG_CONFIG_HOME`
//! and `XDG_STATE_HOME` point into the scratch tree. Include with `mod common; mod
//! workflow_common;`.

#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use p1_assembly::{Catalog, ProviderSpec};
use p1_contracts::{Item, Provider, ProviderRequest};
use p1_testkit::{ScriptedProvider, Step, json_call, text_response, tool_call_response};
use tempfile::TempDir;

use crate::common::{Harness, write_environment};

const FAKE_ENVIRONMENT: &str = r#"
route   = "route-fake"
profile = "main"
"#;

const FAKE_ROUTE: &str = r#"
id           = "route-fake"
origin_route = "openai-chat/fake"
adapter      = "openai-chat"
endpoint     = "https://example.invalid/v1/chat/completions"

[credential]
kind = "api-key"
env  = "FAKE_API_KEY"

[adapter_settings]
dialect = "thinking-with-reasoning-alias"

[models."main"]
wire_model = "wire-main"
[models."other"]
wire_model = "wire-other"
"#;

fn profile(id: &str) -> String {
    format!(
        "id             = \"{id}\"\nrevision       = 1\nmodel_id       = \"{id}-model\"\nfamily         = \"temp\"\nthinking       = \"enabled\"\nefforts        = [\"low\", \"high\"]\ndefault_effort = \"low\"\n"
    )
}

/// Every shipped role resolves at preflight, so the scratch settings point all four
/// at the fake route; a test's own `[workflows]` text is appended after this.
pub const ROLES: &str = r#"
[workflows.roles.worker]
model = "fake/main"
tools = ["read", "grep"]
[workflows.roles.reviewer]
model = "fake/main"
tools = ["read"]
[workflows.roles.verifier]
model = "fake/main"
tools = ["read"]
[workflows.roles.judge]
model = "fake/main"
tools = ["read"]
"#;

pub struct Scratch {
    pub root: TempDir,
    pub workspace: TempDir,
}

impl Scratch {
    /// The tree with [`ROLES`] as the settings.
    pub fn new() -> Self {
        Self::with_settings(ROLES)
    }

    pub fn with_settings(settings: &str) -> Self {
        let root = tempfile::tempdir().unwrap();
        let workspace = tempfile::tempdir().unwrap();
        let path = root.path();
        write_environment(
            &path.join("environments"),
            "parent",
            "fake-parent",
            "model-parent",
            &["read"],
            "PARENT {{tool_names}}",
        );
        write(
            &path.join("environments/fake/environment.toml"),
            FAKE_ENVIRONMENT,
        );
        write(
            &path.join("environments/fake/prompt.md"),
            "STEP {{tool_names}}\n",
        );
        write(&path.join("routes/route-fake.toml"), FAKE_ROUTE);
        write(&path.join("profiles/main.toml"), &profile("main"));
        write(&path.join("profiles/other.toml"), &profile("other"));
        write(&path.join("config/p1/settings.toml"), settings);
        Self { root, workspace }
    }

    pub fn environment_dirs(&self) -> Vec<PathBuf> {
        vec![self.root.path().join("environments")]
    }

    /// The session file a test passes with `--session`; its runs go to
    /// `<session>.workflows/`.
    pub fn session(&self) -> PathBuf {
        self.root.path().join("session.jsonl")
    }

    pub fn run_dir(&self, id: &str) -> PathBuf {
        self.root
            .path()
            .join(format!("session.jsonl.workflows/{id}"))
    }

    pub fn harness(&self) -> Harness {
        let mut harness = Harness::new(self.environment_dirs(), &[]);
        let root = self.root.path();
        harness.deps.shell_env = Some(vec![
            ("HOME".into(), root.as_os_str().to_os_string()),
            (
                "XDG_CONFIG_HOME".into(),
                root.join("config").into_os_string(),
            ),
            ("XDG_STATE_HOME".into(), root.join("state").into_os_string()),
        ]);
        harness
    }

    /// Put `script` in the workspace and return its path.
    pub fn script(&self, name: &str, text: &str) -> PathBuf {
        let path = self.workspace.path().join(name);
        std::fs::write(&path, text).unwrap();
        path
    }
}

/// The providers a test runs: the parent (whole provider `fake-parent`) and one
/// scripted provider per profile of `route-fake`. `builds` counts the step providers
/// the catalog constructed, i.e. the step workers assembled.
pub struct Fakes {
    pub parent: ScriptedProvider,
    pub main: ScriptedProvider,
    pub other: ScriptedProvider,
    pub builds: Arc<AtomicUsize>,
}

impl Fakes {
    pub fn new(parent: Vec<Step>, main: Vec<Step>, other: Vec<Step>) -> Self {
        Self {
            parent: ScriptedProvider::new(parent),
            main: ScriptedProvider::new(main),
            other: ScriptedProvider::new(other),
            builds: Arc::new(AtomicUsize::new(0)),
        }
    }

    pub fn builds(&self) -> usize {
        self.builds.load(Ordering::SeqCst)
    }

    pub fn hook(&self) -> p1_host::catalog::CatalogHook {
        let parent: Arc<dyn Provider> = Arc::new(self.parent.clone());
        let main: Arc<dyn Provider> = Arc::new(self.main.clone());
        let other: Arc<dyn Provider> = Arc::new(self.other.clone());
        let builds = self.builds.clone();
        Box::new(move |catalog: &mut Catalog| {
            let parent = parent.clone();
            catalog.provider("fake-parent", Box::new(move |_spec| Ok(parent.clone())));
            let (main, other, builds) = (main.clone(), other.clone(), builds.clone());
            catalog.provider(
                "route-fake",
                Box::new(move |spec: &ProviderSpec| {
                    builds.fetch_add(1, Ordering::SeqCst);
                    match spec.profile.as_ref().map(|profile| profile.id.as_str()) {
                        Some("other") => Ok(other.clone()),
                        _ => Ok(main.clone()),
                    }
                }),
            );
        })
    }
}

/// A step worker's `finish done` without a command tool, then the turn's last text.
pub fn done(summary: &str) -> Vec<Step> {
    vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            &format!(r#"{{"status":"done","summary":"{summary}","verification":["none"]}}"#),
        )]),
        text_response("step over"),
    ]
}

/// A step worker's `finish done` with a structured `result`.
pub fn done_with(summary: &str, result: &str) -> Vec<Step> {
    vec![
        tool_call_response(vec![json_call(
            "f1",
            "finish",
            &format!(
                r#"{{"status":"done","summary":"{summary}","verification":["none"],"result":{result}}}"#
            ),
        )]),
        text_response("step over"),
    ]
}

/// The parent's steps: start `script`, end its turn, then — woken by the ONE
/// notification — read the result and end.
pub fn parent_that_runs(script: &str, extra: &str) -> Vec<Step> {
    let input = serde_json::json!({ "script": script }).to_string();
    let input = if extra.is_empty() {
        input
    } else {
        format!("{},{extra}}}", &input[..input.len() - 1])
    };
    vec![
        tool_call_response(vec![json_call("c1", "workflow_start", &input)]),
        text_response("started"),
        tool_call_response(vec![json_call("c2", "workflow_result", r#"{"id":"wf1"}"#)]),
        text_response("parent done"),
    ]
}

pub fn tool_names(request: &ProviderRequest) -> Vec<String> {
    request.tools.iter().map(|tool| tool.name.clone()).collect()
}

/// Every tool result `name` answered in `request`'s history.
pub fn results_of(request: &ProviderRequest, name: &str) -> Vec<String> {
    request
        .history
        .iter()
        .filter_map(|item| match item {
            Item::ToolResult(result) if result.name == name => Some(result.content.clone()),
            _ => None,
        })
        .collect()
}

/// The text of `request`'s history, for "did this reach the model" assertions.
pub fn history_text(request: &ProviderRequest) -> String {
    format!("{:?}", request.history)
}

/// The host's `workflow wf1 …` step lines on stderr (not the phase, log or end lines).
/// An interactive session's `p1> ` prompt may precede one on the same line.
pub fn step_lines(stderr: &str, run: &str) -> Vec<String> {
    let prefix = format!("· workflow {run} ");
    stderr
        .lines()
        .filter_map(|line| line.find(&prefix).map(|at| &line[at..]))
        .filter(|line| line.contains(" → "))
        .map(str::to_string)
        .collect()
}

pub fn read_json(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
}

pub fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}
