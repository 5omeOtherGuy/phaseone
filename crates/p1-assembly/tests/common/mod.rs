//! Shared fakes and helpers for the `p1-assembly` integration tests.
//!
//! Providers and tools here are FAKE (from `p1-testkit` plus two tiny local
//! stand-ins); the only real inputs are the shipped `environments/` files.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::sync::Arc;

use p1_assembly::{
    Assembled, Catalog, EnvironmentFile, ProviderSpec, Substitutions, ToolServices, ToolSpec,
};
use p1_contracts::{
    BoxFuture, CancellationToken, DeclarationKind, Effect, ModelOptions, Provider, ProviderError,
    ProviderErrorKind, ProviderRequest, ProviderStream, RouteDescription, Tool, ToolCall,
    ToolContext, ToolDeclaration, ToolIdentity, ToolOutcome,
};
use p1_testkit::{FakeTool, ScriptedProvider, origin};

/// Every tool key the test catalogs register.
pub const TOOL_KEYS: [&str; 6] = ["apply_patch", "edit", "grep", "read", "shell", "write"];

/// The repository's own `environments/` directory.
pub fn shipped_environments() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../environments")
}

pub fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
    }
}

pub fn tool_names(tools: &[Arc<dyn Tool>]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| tool.declaration().name.clone())
        .collect()
}

/// Build an in-memory environment: one tool per key, no per-tool overrides.
pub fn environment_file(
    name: &str,
    provider: &str,
    tools: &[&str],
    prompt: &str,
) -> EnvironmentFile {
    EnvironmentFile {
        name: name.into(),
        family: "test".into(),
        provider: provider.into(),
        model: "test-model".into(),
        options: ModelOptions::default(),
        tools: tools
            .iter()
            .map(|module| ToolSpec {
                module: (*module).into(),
                name: None,
                description: None,
                variant: None,
            })
            .collect(),
        prompt_template: prompt.into(),
        context: None,
        summarize_prompt: None,
    }
}

/// Register a `FakeTool` factory for each key. The factory applies `spec.name`
/// when present, which is what a host-side factory must do for a `ToolFace`.
pub fn register_fake_tools(catalog: &mut Catalog, keys: &[&str]) {
    for key in keys {
        let key = key.to_string();
        let default_name = key.clone();
        catalog.tool(
            &key,
            Box::new(move |spec: &ToolSpec, _services: &ToolServices| {
                let name = spec.name.clone().unwrap_or_else(|| default_name.clone());
                Ok(Arc::new(FakeTool::new(&name)) as Arc<dyn Tool>)
            }),
        );
    }
}

/// Register a permissive scripted provider under `key` and return a probe clone
/// that shares its recorded requests/validations.
pub fn register_scripted_provider(catalog: &mut Catalog, key: &str) -> ScriptedProvider {
    let provider = ScriptedProvider::new(Vec::new());
    let probe = provider.clone();
    catalog.provider(
        key,
        Box::new(move |_spec: &ProviderSpec| Ok(Arc::new(provider.clone()) as Arc<dyn Provider>)),
    );
    probe
}

/// Write `<root>/<name>/environment.toml` and `prompt.md`.
pub fn write_environment(root: &Path, name: &str, toml: &str, prompt: &str) -> PathBuf {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), prompt).unwrap();
    dir
}

/// Assert the prompt/tool coherence rule: after substitution, every catalog tool
/// key that appears in the prompt as a backticked identifier must be an assembled
/// tool of THIS environment.
pub fn assert_prompt_coherent(catalog: &Catalog, assembled: &Assembled) {
    assert!(
        !assembled.system_prompt.contains("{{"),
        "prompt still contains a placeholder"
    );
    let names: Vec<&str> = assembled
        .tools
        .iter()
        .map(|tool| tool.declaration().name.as_str())
        .collect();
    for key in catalog.tool_keys() {
        if names.contains(&key.as_str()) {
            continue;
        }
        assert!(
            !assembled.system_prompt.contains(&format!("`{key}`")),
            "prompt of `{}` mentions `{key}` but it is not assembled",
            assembled.resolved.environment
        );
    }
}

fn route(supports_freeform_tools: bool) -> RouteDescription {
    RouteDescription {
        origin: origin(),
        supports_freeform_tools,
        mandatory_prompt_prefix: None,
        reports_cost: false,
    }
}

/// A provider that can carry function declarations only; rejects freeform tools
/// exactly like a function-only route must.
pub struct FunctionOnlyProvider;

impl Provider for FunctionOnlyProvider {
    fn describe(&self) -> RouteDescription {
        route(false)
    }

    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        for tool in &request.tools {
            if matches!(tool.kind, DeclarationKind::Freeform { .. }) {
                return Err(ProviderError::new(
                    ProviderErrorKind::InvalidRequest,
                    format!("route cannot carry the freeform tool `{}`", tool.name),
                ));
            }
        }
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        _request: ProviderRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::new(
                ProviderErrorKind::Transport,
                "not scripted",
            ))
        })
    }
}

/// A provider whose factory captured a sentinel secret. It never exposes the
/// secret through any contract method.
pub struct HoldingProvider {
    pub secret: String,
}

impl Provider for HoldingProvider {
    fn describe(&self) -> RouteDescription {
        route(true)
    }

    fn validate(&self, _request: &ProviderRequest) -> Result<(), ProviderError> {
        Ok(())
    }

    fn stream<'a>(
        &'a self,
        _request: ProviderRequest,
        _cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        Box::pin(async {
            Err(ProviderError::new(
                ProviderErrorKind::Transport,
                "not scripted",
            ))
        })
    }
}

/// A freeform tool, used to trigger a function-only route's rejection.
pub struct FreeformTool {
    declaration: ToolDeclaration,
    identity: ToolIdentity,
}

impl FreeformTool {
    pub fn new(name: &str) -> Self {
        Self {
            declaration: ToolDeclaration {
                name: name.into(),
                description: format!("freeform {name}"),
                kind: DeclarationKind::Freeform { grammar: None },
            },
            identity: ToolIdentity {
                implementation: format!("fake-{name}"),
                variant: "test".into(),
            },
        }
    }
}

impl Tool for FreeformTool {
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
        Box::pin(async { ToolOutcome::ok("ok") })
    }
}
