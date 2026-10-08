//! Operator-defined subagents. Configuration and prompts are snapshotted together;
//! only names and one-line use cases are exposed to a parent model.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use p1_contracts::BoxFuture;
use p1_workers::subagents::{SubagentDefinition, SubagentDefinitions, SubagentRequest};
use p1_workers::{ChildId, ChildSpec, WorkerError, WorkersStart};
use serde::Deserialize;

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Configuration {
    #[serde(default)]
    subagents: Vec<Entry>,
}

#[derive(Deserialize)]
struct Entry {
    subagent_type: String,
    #[serde(flatten)]
    definition: SubagentDefinition,
}

#[derive(Default)]
pub(super) struct Subagents {
    pub definitions: SubagentDefinitions,
    prompts: BTreeMap<String, String>,
}

impl Subagents {
    /// First search directory containing subagents.toml wins, as with environments.
    pub fn load(directories: &[PathBuf]) -> Result<Self, String> {
        for directory in directories {
            let path = directory.join("subagents.toml");
            let text = match p1_assembly::read_configuration(&path) {
                Ok(text) => text,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
                Err(error) => return Err(format!("cannot read {}: {error}", path.display())),
            };
            // Do not include TOML diagnostics: they can quote private configuration.
            let configuration: Configuration = toml::from_str(&text)
                .map_err(|_| format!("invalid subagent configuration: {}", path.display()))?;
            let mut result = Self::default();
            for entry in configuration.subagents {
                let name = entry.subagent_type;
                if result
                    .definitions
                    .subagents
                    .insert(name.clone(), entry.definition)
                    .is_some()
                {
                    return Err(format!("duplicate subagent_type `{name}`"));
                }
            }
            result.definitions.validate()?;
            for (name, definition) in &result.definitions.subagents {
                let path = directory.join(&definition.prompt_file);
                let prompt = p1_assembly::read_configuration(&path)
                    .map_err(|error| format!("cannot read subagent `{name}` prompt: {error}"))?;
                result.prompts.insert(name.clone(), prompt);
            }
            return Ok(result);
        }
        Ok(Self::default())
    }

    pub fn resolve(
        &self,
        request: SubagentRequest,
        grant: &[String],
        allowed: Option<&[String]>,
    ) -> Result<ChildSpec, String> {
        let name = request.subagent_type.clone();
        let mut spec = self.definitions.resolve(request, grant, allowed)?;
        if spec.options.system_prompt.is_none() {
            spec.options.system_prompt = self.prompts.get(&name).cloned();
        }
        Ok(spec)
    }
}

/// One parent's start interface. Permissions come from its real assembly,
/// not the catalog's list of everything the operator could grant.
pub(super) struct ConfiguredStart {
    pub inner: Arc<dyn WorkersStart>,
    pub subagents: Arc<Subagents>,
    pub grant: Vec<String>,
    pub allowed: Option<Vec<String>>,
    pub builtin: bool,
    pub workspace: PathBuf,
}

impl WorkersStart for ConfiguredStart {
    fn start<'a>(&'a self, mut spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        if !self
            .subagents
            .definitions
            .subagents
            .contains_key(&spec.environment)
        {
            return Box::pin(async move {
                if self.allowed.is_some() {
                    return Err(WorkerError::InvalidEnvironment(
                        "nested starts require a configured subagent_type".into(),
                    ));
                }
                // The wrapped GrantChecked start owns legacy refusals and their
                // established text; do not layer a different grant error here.
                spec.workspace = Some(self.workspace.clone());
                self.inner.start(spec).await
            });
        }
        // Separate built-in tools use the environment ABI. Resolve their named
        // start through config too, so neither prompts nor child permissions can
        // be bypassed by choosing the older interface.
        self.start_subagent(SubagentRequest {
            subagent_type: spec.environment,
            task: spec.task,
            tools: (!self.builtin).then_some(spec.tools),
            model: None,
            effort: None,
            system_prompt: None,
            background: !self.builtin && spec.options.background,
            isolation: spec.options.isolation,
        })
    }

    fn subagent_definitions(&self) -> &SubagentDefinitions {
        &self.subagents.definitions
    }

    fn start_subagent<'a>(
        &'a self,
        request: SubagentRequest,
    ) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
        Box::pin(async move {
            let mut spec = self
                .subagents
                .resolve(request, &self.grant, self.allowed.as_deref())
                .map_err(WorkerError::InvalidEnvironment)?;
            // Nested shared children stay in this parent's checkout; isolated
            // children fork this parent's HEAD rather than the root agent's.
            spec.workspace = Some(self.workspace.clone());
            self.inner.start(spec).await
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn configured_wasm_start_resolves_overrides_and_refuses_leaf_dispatch() {
        use p1_contracts::serde_json::{Value, json};
        use p1_contracts::{
            CancellationToken, DeclarationKind, ToolCall, ToolContext, ToolInput, ToolStatus,
        };
        use p1_module_runtime::delegation::{WorkerLists, WorkerServices};
        use p1_module_runtime::{ExecutionLimits, Services};
        use p1_testkit::{
            PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization,
            ScriptedProvider, Step, text_response,
        };
        use p1_workers::scope::UnscopedWorkers;
        use p1_workers::{ChildAgent, InProcessWorkers, WorkerReport, WorkerService};

        let package = "p1-module-worker-start";
        let built = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../modules/target/p1-modules")
            .join(package);
        let manifest: Value = serde_json::from_slice(
            &std::fs::read(built.join(format!("{package}.manifest.json"))).unwrap(),
        )
        .unwrap();
        let bytes = std::fs::read(built.join(format!("{package}.wasm"))).unwrap();
        let mut release = p1_module_tests::Release::empty();
        release.add(json!({"name":manifest["name"], "digest":manifest["digest"], "path":"worker.wasm", "kind":manifest["kind"], "world":manifest["world"], "protocol":manifest["protocol"], "capabilities":manifest["capabilities"], "variant":manifest["variant"]}), &bytes);
        let loader = release.loader();
        let module = loader.load("p1/worker-start").unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::<ChildSpec>::new()));
        let captured = seen.clone();
        let pending = Arc::new(ScriptedProvider::new(vec![Step::EventsThenAwaitCancel(
            vec![],
        )]));
        let pending_child = pending.clone();
        let workers = InProcessWorkers::new(
            Arc::new(move |spec| {
                let provider = {
                    let mut specs = captured.lock().unwrap();
                    specs.push(spec.clone());
                    if specs.len() == 1 {
                        Arc::new(ScriptedProvider::new(vec![text_response(
                            "retained answer",
                        )]))
                    } else {
                        pending_child.clone()
                    }
                };
                let agent = p1_core::Agent::new(p1_core::AgentParts {
                    provider,
                    tools: vec![],
                    system_prompt: "test".into(),
                    options: Default::default(),
                    context: Arc::new(PassthroughContext),
                    authorization: Arc::new(ScriptedAuthorization::permit_all()),
                    journal: Arc::new(RecordingJournal::new()),
                    events: Arc::new(RecordingEvents::new()),
                })
                .unwrap();
                Ok(ChildAgent {
                    agent,
                    description: "fake/model".into(),
                    report: Arc::new(WorkerReport::default),
                    regrant: None,
                    fallback: None,
                })
            }),
            1,
        );
        let subagents = Arc::new(Subagents {
            definitions: serde_json::from_value(json!({"subagents":{"search":{"environment":"reader", "description":"Find relevant files", "prompt_file":"prompt.md", "tools":["read","shell"], "models":["reader/primary","reader/backup"]}}})).unwrap(),
            prompts: BTreeMap::from([("search".into(), "Private configured role".into())]),
        });
        let scope = Arc::new(UnscopedWorkers::new(workers.clone()));
        let make_tool = |allowed| {
            let start = Arc::new(ConfiguredStart {
                inner: scope.clone(),
                subagents: subagents.clone(),
                grant: vec!["read".into()],
                allowed,
                builtin: false,
                workspace: built.clone(),
            });
            p1_module_runtime::tool::wasm_tool(
                &module,
                Services {
                    workers: Some(WorkerServices {
                        start: Some(start),
                        observe: Some(scope.clone()),
                        control: None,
                        lists: WorkerLists {
                            grantable: vec!["read".into()],
                            environments: vec!["reader".into()],
                            subagents: BTreeMap::from([(
                                "search".into(),
                                "Find relevant files".into(),
                            )]),
                        },
                    }),
                    ..Default::default()
                },
                ExecutionLimits::default(),
                &Arc::new(p1_redact::MaskCounter::new()),
            )
            .unwrap()
        };
        let tool = make_tool(None);
        let DeclarationKind::Function { input_schema } = &tool.declaration().kind else {
            panic!("function schema")
        };
        assert_eq!(
            input_schema["properties"]["subagent_type"]["enum"],
            json!(["search"])
        );
        assert!(
            tool.declaration()
                .description
                .contains("Find relevant files")
        );
        assert!(!format!("{:?}", tool.declaration()).contains("Private configured role"));
        let call = ToolCall { call_id:"c1".into(), name:"worker_start".into(), input:ToolInput::Json(json!({"subagent_type":"search", "task":"inspect parser", "tools":["shell","read"], "model":"other/replacement", "effort":"high", "system_prompt":"Replacement role", "background":false, "isolation":"worktree"}).to_string()) };
        let result = tool
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
        assert!(
            result.content.ends_with("retained answer"),
            "{}",
            result.content
        );
        {
            let specs = seen.lock().unwrap();
            assert_eq!(specs.len(), 1);
            let spec = &specs[0];
            assert_eq!(spec.tools, ["read"]);
            assert_eq!(spec.environment, "reader");
            assert_eq!(spec.options.models, ["other/replacement"]);
            assert_eq!(spec.options.effort, Some(p1_contracts::Effort::High));
            assert_eq!(
                spec.options.system_prompt.as_deref(),
                Some("Replacement role")
            );
            assert_eq!(
                spec.options.isolation,
                p1_workers::subagents::Isolation::Worktree
            );
            assert!(!spec.options.background);
        }
        let leaf = make_tool(Some(vec![]));
        let refused = leaf
            .execute(
                &call,
                ToolContext {
                    cancel: CancellationToken::new(),
                },
            )
            .await;
        assert_ne!(refused.status, ToolStatus::Ok);
        assert_eq!(seen.lock().unwrap().len(), 1, "no child was allocated");
        let cancel = CancellationToken::new();
        let (interrupted, ()) = tokio::join!(
            tool.execute(
                &call,
                ToolContext {
                    cancel: cancel.clone()
                }
            ),
            async {
                pending.drained.notified().await;
                cancel.cancel();
            }
        );
        assert_eq!(
            interrupted.status,
            ToolStatus::Cancelled,
            "{}",
            interrupted.content
        );
        assert!(
            interrupted.content.contains("w2"),
            "the started id remains visible"
        );
        assert!(matches!(
            workers.status(&ChildId("w2".into())).await.unwrap(),
            p1_workers::ChildStatus::Running
        ));
        workers.shutdown().await;
    }

    fn request() -> SubagentRequest {
        serde_json::from_value(serde_json::json!({"subagent_type":"search", "task":"find"}))
            .unwrap()
    }

    #[derive(Default)]
    struct RecordingStart(std::sync::Mutex<Vec<ChildSpec>>);

    impl WorkersStart for RecordingStart {
        fn start<'a>(&'a self, spec: ChildSpec) -> BoxFuture<'a, Result<ChildId, WorkerError>> {
            Box::pin(async move {
                self.0.lock().unwrap().push(spec);
                Ok(ChildId("w1".into()))
            })
        }
    }

    #[tokio::test]
    async fn both_start_interfaces_enforce_child_policy_before_backend() {
        let backend = Arc::new(RecordingStart::default());
        let subagents = Arc::new(Subagents {
            definitions: serde_json::from_value(serde_json::json!({"subagents": {
                "search": {"environment":"reader", "description":"Find code",
                    "prompt_file":"prompt.md", "tools":["read", "shell"], "models":["reader/model"]}
            }}))
            .unwrap(),
            prompts: BTreeMap::from([("search".into(), "Configured prompt".into())]),
        });
        let mut start = ConfiguredStart {
            inner: backend.clone(),
            subagents,
            grant: vec!["read".into()],
            allowed: Some(vec![]),
            builtin: false,
            workspace: PathBuf::from("/isolated-parent"),
        };
        let legacy = || ChildSpec {
            environment: "search".into(),
            task: "find".into(),
            tools: vec!["read".into(), "shell".into()],
            workspace: None,
            options: Default::default(),
        };
        assert!(start.start_subagent(request()).await.is_err());
        assert!(start.start(legacy()).await.is_err());
        assert!(backend.0.lock().unwrap().is_empty());
        start.allowed = Some(vec!["search".into()]);
        start.start(legacy()).await.unwrap();
        let calls = backend.0.lock().unwrap();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].tools, ["read"]);
        assert_eq!(calls[0].environment, "reader");
        assert_eq!(
            calls[0].workspace.as_deref(),
            Some(std::path::Path::new("/isolated-parent"))
        );
        assert_eq!(
            calls[0].options.system_prompt.as_deref(),
            Some("Configured prompt")
        );
        assert_eq!(calls[0].options.models, ["reader/model"]);
    }

    #[test]
    fn configuration_snapshots_prompt_and_clamps_real_grant() {
        let root = tempfile::tempdir().unwrap();
        std::fs::write(root.path().join("prompt.md"), "Original prompt").unwrap();
        std::fs::write(
            root.path().join("subagents.toml"),
            r#"
[[subagents]]
subagent_type = "search"
environment = "reader"
description = "Find relevant files"
prompt_file = "prompt.md"
tools = ["read", "shell"]
models = ["reader/primary", "reader/backup"]
"#,
        )
        .unwrap();
        let loaded = Subagents::load(&[root.path().into()]).unwrap();
        std::fs::write(root.path().join("prompt.md"), "Changed prompt").unwrap();
        let spec = loaded.resolve(request(), &["read".into()], None).unwrap();
        assert_eq!(spec.tools, ["read"]);
        assert_eq!(
            spec.options.system_prompt.as_deref(),
            Some("Original prompt")
        );
        assert!(spec.options.allowed_children.is_empty());
        assert!(
            loaded
                .resolve(request(), &["read".into()], Some(&[]))
                .is_err()
        );
        let mut replacement = request();
        replacement.system_prompt = Some("Replacement".into());
        assert_eq!(
            loaded
                .resolve(replacement, &[], None)
                .unwrap()
                .options
                .system_prompt
                .as_deref(),
            Some("Replacement")
        );
    }

    #[test]
    fn absent_config_is_legacy_but_invalid_config_is_not() {
        let root = tempfile::tempdir().unwrap();
        assert!(
            Subagents::load(&[root.path().into()])
                .unwrap()
                .definitions
                .subagents
                .is_empty()
        );
        std::fs::write(
            root.path().join("subagents.toml"),
            "private-content = [ invalid",
        )
        .unwrap();
        let error = Subagents::load(&[root.path().into()]).err().unwrap();
        assert!(!error.contains("private-content"));
    }

    #[test]
    fn shipped_configuration_reuses_only_the_three_ampi_companions() {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../environments");
        let loaded = Subagents::load(std::slice::from_ref(&root)).unwrap();
        assert_eq!(
            loaded
                .definitions
                .subagents
                .keys()
                .map(String::as_str)
                .collect::<Vec<_>>(),
            ["finder", "librarian", "task"]
        );
        for name in ["finder", "librarian", "task"] {
            let definition = &loaded.definitions.subagents[name];
            assert!(definition.allowed_children.is_empty());
            assert_eq!(definition.environment, name);
            assert_eq!(
                loaded.prompts[name],
                std::fs::read_to_string(root.join(name).join("prompt.md")).unwrap()
            );
        }
        assert_eq!(
            loaded.definitions.subagents["finder"].models,
            ["gpt/gpt-5.6-terra:low"]
        );
        assert_eq!(
            loaded.definitions.subagents["librarian"].models,
            ["gpt/gpt-5.6-sol"]
        );
        assert_eq!(
            loaded.definitions.subagents["task"].models,
            ["claude/claude-opus-5-5:medium"]
        );
    }

    #[tokio::test]
    async fn builtin_legacy_start_uses_configured_defaults_not_compiled_grant() {
        let backend = Arc::new(RecordingStart::default());
        let subagents = Arc::new(Subagents {
            definitions: serde_json::from_value(serde_json::json!({"subagents": {
                "search": {"environment":"reader", "description":"Find code", "prompt_file":"prompt.md", "tools":["shell"], "models":["reader/model"]}
            }})).unwrap(),
            prompts: BTreeMap::from([("search".into(), "Configured role".into())]),
        });
        let start = ConfiguredStart {
            inner: backend.clone(),
            subagents,
            grant: vec!["shell".into()],
            allowed: None,
            builtin: true,
            workspace: PathBuf::from("/parent"),
        };
        start
            .start(ChildSpec {
                environment: "search".into(),
                task: "find".into(),
                tools: vec!["read".into()],
                workspace: None,
                options: Default::default(),
            })
            .await
            .unwrap();
        let calls = backend.0.lock().unwrap();
        assert_eq!(calls[0].tools, ["shell"]);
        assert!(!calls[0].options.background);
        assert_eq!(
            calls[0].options.system_prompt.as_deref(),
            Some("Configured role")
        );
    }
}
