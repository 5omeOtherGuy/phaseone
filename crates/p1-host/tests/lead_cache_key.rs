//! The host's prompt-cache-key policy (ADR-0039): the RESOLVED route decides.
//! A key is generated only for a route that reports `CacheKeySupport::Optional`
//! and only when the environment sets none; one assembly builds the provider and
//! the tools exactly once, an assembly error is reported as it is, and a key set
//! explicitly in the environment file is never dropped silently.

mod common;

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use common::{Harness, provider_hook_arc, run_args, write_environment};
use p1_assembly::Catalog;
use p1_contracts::{
    BoxFuture, CacheKeySupport, CancellationToken, Provider, ProviderError, ProviderErrorKind,
    ProviderRequest, ProviderStream, RouteDescription, Tool,
};
use p1_testkit::{FakeTool, ScriptedProvider, text_response};
use tempfile::tempdir;

/// A route with automatic caching only: any explicit cache key is an invalid request.
struct NoCacheKey(ScriptedProvider);

impl Provider for NoCacheKey {
    fn describe(&self) -> RouteDescription {
        self.0.describe()
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        if request.options.cache_key.is_some() {
            return Err(ProviderError {
                kind: ProviderErrorKind::InvalidRequest,
                message: "this route takes no cache key".into(),
            });
        }
        self.0.validate(request)
    }
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.0.stream(request, cancel)
    }
}

#[tokio::test]
async fn a_route_that_refuses_the_generated_key_runs_without_it() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(environments.path(), "plain", "nokey", "m", &[], "prompt");
    let inner = ScriptedProvider::new(vec![text_response("hello")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![(
        "nokey",
        Arc::new(NoCacheKey(inner.clone())) as Arc<dyn Provider>,
    )]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(inner.requests()[0].options.cache_key, None);
}

#[tokio::test]
async fn an_explicit_key_the_route_refuses_fails_assembly() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(environments.path(), "plain", "nokey", "m", &[], "prompt");
    let file = environments.path().join("plain/environment.toml");
    let mut toml = std::fs::read_to_string(&file).unwrap();
    toml.push_str("\n[options]\ncache_key = \"mine\"\n");
    std::fs::write(&file, toml).unwrap();
    let inner = ScriptedProvider::new(vec![text_response("never")]);
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook_arc(vec![(
        "nokey",
        Arc::new(NoCacheKey(inner.clone())) as Arc<dyn Provider>,
    )]));

    let code = run_args(
        &mut harness,
        &[
            "--env",
            "plain",
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await;

    assert_eq!(code, 1);
    assert!(
        harness.stderr.text().contains("takes no cache key"),
        "{}",
        harness.stderr.text()
    );
    assert!(inner.requests().is_empty());
}

// ------------------------------------------------------- the description-driven policy

/// A route that CONSUMES a prompt-cache key (reports `Optional`) and accepts any.
struct TakesCacheKey(ScriptedProvider);

impl Provider for TakesCacheKey {
    fn describe(&self) -> RouteDescription {
        RouteDescription {
            cache_key: CacheKeySupport::Optional,
            ..self.0.describe()
        }
    }
    fn validate(&self, request: &ProviderRequest) -> Result<(), ProviderError> {
        self.0.validate(request)
    }
    fn stream<'a>(
        &'a self,
        request: ProviderRequest,
        cancel: CancellationToken,
    ) -> BoxFuture<'a, Result<ProviderStream, ProviderError>> {
        self.0.stream(request, cancel)
    }
}

/// How often the provider and the tool factories ran for ONE agent: the
/// cache-key policy must need a single assembly (ADR-0039).
#[derive(Default)]
struct Builds {
    providers: AtomicUsize,
    tools: AtomicUsize,
}

fn counting_hook(
    key: &'static str,
    provider: Arc<dyn Provider>,
    builds: Arc<Builds>,
) -> p1_host::catalog::CatalogHook {
    Box::new(move |catalog: &mut Catalog| {
        let provider = provider.clone();
        let provider_builds = builds.clone();
        catalog.provider(
            key,
            Box::new(move |_spec| {
                provider_builds.providers.fetch_add(1, Ordering::SeqCst);
                Ok(provider.clone())
            }),
        );
        let tool_builds = builds.clone();
        catalog.tool(
            "read",
            Box::new(move |_spec, _services| {
                tool_builds.tools.fetch_add(1, Ordering::SeqCst);
                Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>)
            }),
        );
    })
}

fn builds_of(builds: &Builds) -> (usize, usize) {
    (
        builds.providers.load(Ordering::SeqCst),
        builds.tools.load(Ordering::SeqCst),
    )
}

async fn run_one_turn(harness: &mut Harness, workspace: &tempfile::TempDir, env: &str) -> i32 {
    run_args(
        harness,
        &[
            "--env",
            env,
            "--workspace",
            workspace.path().to_str().unwrap(),
            "hi",
        ],
    )
    .await
}

#[tokio::test]
async fn an_optional_route_with_no_configured_key_gets_a_generated_key_and_builds_once() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "cached",
        "m",
        &["read"],
        "prompt",
    );
    let inner = ScriptedProvider::new(vec![text_response("hello")]);
    let builds = Arc::new(Builds::default());
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(counting_hook(
        "cached",
        Arc::new(TakesCacheKey(inner.clone())),
        builds.clone(),
    ));

    let code = run_one_turn(&mut harness, &workspace, "plain").await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    // The key was generated, reached the provider, and is stable-looking.
    let key = inner.requests()[0].options.cache_key.clone();
    let key = key.expect("an Optional route is handed a generated key");
    assert!(key.starts_with("p1-") && key.len() > "p1-".len(), "{key}");
    // ONE assembly: the provider and the tool were each built exactly once.
    assert_eq!(builds_of(&builds), (1, 1));
}

#[tokio::test]
async fn an_unsupported_route_is_built_once_and_never_offered_a_key() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "nokey",
        "m",
        &["read"],
        "prompt",
    );
    let inner = ScriptedProvider::new(vec![text_response("hello")]);
    let builds = Arc::new(Builds::default());
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(counting_hook(
        "nokey",
        Arc::new(NoCacheKey(inner.clone())),
        builds.clone(),
    ));

    let code = run_one_turn(&mut harness, &workspace, "plain").await;

    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    assert_eq!(inner.requests()[0].options.cache_key, None);
    // The description said `Unsupported`, so no key was generated and no second
    // assembly was needed to recover from one.
    assert_eq!(builds_of(&builds), (1, 1));
}

#[tokio::test]
async fn an_explicit_key_on_an_unsupported_route_fails_after_one_build() {
    let workspace = tempdir().unwrap();
    let environments = tempdir().unwrap();
    write_environment(
        environments.path(),
        "plain",
        "nokey",
        "m",
        &["read"],
        "prompt",
    );
    let file = environments.path().join("plain/environment.toml");
    let mut toml = std::fs::read_to_string(&file).unwrap();
    toml.push_str("\n[options]\ncache_key = \"mine\"\n");
    std::fs::write(&file, toml).unwrap();
    let inner = ScriptedProvider::new(Vec::new());
    let builds = Arc::new(Builds::default());
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(counting_hook(
        "nokey",
        Arc::new(NoCacheKey(inner.clone())),
        builds.clone(),
    ));

    let code = run_one_turn(&mut harness, &workspace, "plain").await;

    assert_eq!(code, 1, "stderr: {}", harness.stderr.text());
    assert!(inner.requests().is_empty());
    // The refusal is reported as it is: one build, no retry without the key.
    assert_eq!(builds_of(&builds), (1, 1));
}
