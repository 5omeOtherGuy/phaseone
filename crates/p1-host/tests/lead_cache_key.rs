//! The host OFFERS a generated prompt-cache key; whether a route takes one is the route's
//! knowledge. A route whose `validate` refuses the key still assembles — without it —
//! while a key set explicitly in the environment file is never dropped silently.

mod common;

use std::sync::Arc;

use common::{Harness, provider_hook_arc, run_args, write_environment};
use p1_contracts::{
    BoxFuture, CancellationToken, Provider, ProviderError, ProviderErrorKind, ProviderRequest,
    ProviderStream, RouteDescription,
};
use p1_testkit::{ScriptedProvider, text_response};
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
