//! Load and execute the actual, separately built agent components over the host's
//! scoped worker capability. No live provider or GitHub traffic.
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use p1_assembly::{
    Catalog, ModulesLock, Substitutions, ToolSpec, assemble_for_agent, load_environment,
};
use p1_contracts::serde_json::{self, Value, json};
use p1_contracts::{
    CancellationToken, Effect, ModelOptions, ToolCall, ToolContext, ToolInput, ToolStatus,
};
use p1_core::{Agent, AgentParts};
use p1_host::catalog::modules::{load_locked_modules, register_modules};
use p1_host::workflow::{MemberScopes, worker_member_services};
use p1_module_tests::{Release, within_deadline};
use p1_redact::MaskCounter;
use p1_testkit::{
    PassthroughContext, RecordingEvents, RecordingJournal, ScriptedAuthorization, ScriptedProvider,
    Step, text_response,
};
use p1_workers::{
    ChildAgent, ChildId, ChildSpec, ChildStatus, InProcessWorkers, WorkerReport, WorkerService,
};

fn built() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../modules/target/p1-modules")
}

#[tokio::test]
async fn separate_agent_packages_start_fixed_workers_and_return_their_answers() {
    within_deadline("subagent packages", async {
        for (key, name, input, expected_task, expected_tools) in [
            ("finder", "finder", json!({"query":"locate parser","context":"only Rust"}), "Context: only Rust\n\nQuery: locate parser", vec!["read", "grep"]),
            ("librarian", "librarian", json!({"query":"explain history"}), "explain history", vec!["shell", "read_output"]),
            ("task", "Task", json!({"prompt":"fix parser","description":"parser fix"}), "fix parser", vec!["read", "edit", "write", "grep", "shell", "shell_job", "read_output"]),
        ] {
            let package = format!("p1-module-{key}");
            let manifest: Value = serde_json::from_slice(&std::fs::read(built().join(&package).join(format!("{package}.manifest.json"))).unwrap()).unwrap();
            assert_eq!(manifest["capabilities"], json!(["control", "workers-start", "workers-observe", "workers-control"]));
            let bytes = std::fs::read(built().join(&package).join(format!("{package}.wasm"))).unwrap();
            let mut release = Release::empty();
            release.add(json!({
                "name":manifest["name"], "digest":manifest["digest"],
                "path":format!("packages/{package}.wasm"), "kind":manifest["kind"],
                "world":manifest["world"], "protocol":manifest["protocol"],
                "capabilities":manifest["capabilities"], "variant":manifest["variant"]
            }), &bytes);
            let lock = ModulesLock::parse(Path::new("modules.lock"), &format!(
                "format = \"p1-modules-lock/1\"\n[modules.{key}]\npackage = \"p1/{key}\"\nversion = \"0.0.1\"\ndigest = {}\nworld = {}\nprotocol = {}\n",
                manifest["digest"], manifest["world"], manifest["protocol"]
            )).unwrap();
            let seen = Arc::new(Mutex::new(Vec::<ChildSpec>::new()));
            let captured = seen.clone();
            let pending = Arc::new(ScriptedProvider::new(vec![Step::EventsThenAwaitCancel(Vec::new())]));
            let pending_child = pending.clone();
            let service = InProcessWorkers::new(Arc::new(move |spec| {
                let provider = {
                    let mut specs = captured.lock().unwrap();
                    specs.push(spec.clone());
                    if specs.len() == 1 {
                        Arc::new(ScriptedProvider::new(vec![text_response("evidence-backed answer")]))
                    } else {
                        pending_child.clone()
                    }
                };
                let agent = Agent::new(AgentParts {
                    provider,
                    tools: Vec::new(), system_prompt: "fake child".into(), options: ModelOptions::default(),
                    context: Arc::new(PassthroughContext), authorization: Arc::new(ScriptedAuthorization::permit_all()),
                    journal: Arc::new(RecordingJournal::new()), events: Arc::new(RecordingEvents::new()),
                }).unwrap();
                Ok(ChildAgent {agent, description:"fake/model".into(), report:Arc::new(WorkerReport::default), regrant:None, fallback:None})
            }), 1);
            let scopes = MemberScopes::new(service.clone() as Arc<dyn WorkerService>);
            let mut catalog = Catalog::new();
            catalog.provider("scripted", Box::new(|_| Ok(Arc::new(ScriptedProvider::new(Vec::new())))));
            register_modules(&mut catalog, load_locked_modules(&lock, &release.manifest_file()).unwrap(), worker_member_services(scopes, None)).unwrap();
            let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../environments");
            let mut environment = load_environment(key, &[root]).unwrap();
            environment.provider = "scripted".into();
            environment.profile = None;
            environment.options = ModelOptions::default();
            environment.prompt_template = "{{tool_names}}".into();
            environment.tools = vec![ToolSpec {module:key.into(), name:None, description:None, variant:None}];
            let workspace = tempfile::tempdir().unwrap();
            let substitutions = Substitutions {workspace:workspace.path().display().to_string(), date:"2026-01-01".into(), os:"linux".into(), scratch:String::new()};
            let mask = Arc::new(MaskCounter::new());
            let assembled = assemble_for_agent(&catalog, &environment, workspace.path(), &substitutions, &mask, Some("parent"), |_| ModelOptions::default()).unwrap();
            let tool = &assembled.tools[0];
            assert_eq!(tool.declaration().name, name);
            assert_eq!(tool.identity().implementation, format!("p1/{key}"));
            let call = ToolCall {call_id:"c1".into(), name:name.into(), input:ToolInput::Json(input.to_string())};
            assert_eq!(tool.effect(&call), Effect::Delegates);
            let result = tool.execute(&call, ToolContext {cancel:CancellationToken::new()}).await;
            assert_eq!(result.status, ToolStatus::Ok, "{}", result.content);
            assert!(result.content.starts_with(&format!("Started worker w1 on {key}.")));
            assert!(result.content.ends_with("evidence-backed answer"));
            {
                let specs = seen.lock().unwrap();
                assert_eq!(specs.len(), 1);
                assert_eq!(specs[0].environment, key);
                assert_eq!(specs[0].task, expected_task);
                assert_eq!(specs[0].tools, expected_tools);
            }
            // Cancel only after the second child's provider is streaming. A tool that
            // merely abandons its wait leaves w2 Running and fails this check.
            let cancel = CancellationToken::new();
            let (result, ()) = tokio::join!(
                tool.execute(&call, ToolContext {cancel:cancel.clone()}),
                async {
                    pending.drained.notified().await;
                    cancel.cancel();
                }
            );
            assert_eq!(result.status, ToolStatus::Cancelled, "{}", result.content);
            assert!(result.content.starts_with(&format!("Started worker w2 on {key}.")), "{}", result.content);
            assert!(matches!(service.wait(&ChildId("w2".into()), CancellationToken::new()).await.unwrap(), ChildStatus::Cancelled));
            // Child assemblies have no agent id: a nested agent module cannot link.
            assert!(p1_assembly::assemble(&catalog, &environment, workspace.path(), &substitutions).is_err());
            service.shutdown().await;
        }
    }).await;
}
