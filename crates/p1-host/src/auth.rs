//! The host's credential seam: this crate composes `p1-auth`, and nowhere else does
//! (ADR-0040, spec §2). The host composes; the crate that owns the chain resolves.
//!
//! Nothing here reads a credential file: a [`CredentialSource`] is built lazily and
//! reads its source on every `access`. Where it reads from is decided by the
//! `[credential]` table of the route file, never by this crate.

use std::sync::Arc;

use p1_auth::Locations;
use p1_contracts::{BoxFuture, ProviderError};
use p1_provider_http::{Credential, CredentialSource, Transport};

/// The credential locations the host composes (spec §2). The injected home and the
/// injected environment snapshot win over the process ones, so a host test never
/// reads a real login: an injected snapshot replaces the process environment
/// entirely, and `deps.home == None` means "this host has no home".
pub fn locations(deps: &crate::HostDeps) -> Locations {
    let locations = match &deps.shell_env {
        Some(snapshot) => Locations::from_environment(snapshot.iter().cloned()),
        None => Locations::from_process(),
    };
    locations.with_home(deps.home.clone())
}

/// The credential source a route file's `[credential]` table describes, reading the
/// process environment (the production value).
pub fn credential_source(
    route: &crate::routes::RouteFile,
    transport: Arc<dyn Transport>,
) -> Arc<dyn CredentialSource> {
    credential_source_at(route, transport, &Locations::from_process())
}

/// The same, with locations the caller composed. `catalog` uses this so the whole
/// host can run against injected locations.
pub fn credential_source_at(
    route: &crate::routes::RouteFile,
    transport: Arc<dyn Transport>,
    locations: &Locations,
) -> Arc<dyn CredentialSource> {
    p1_auth::resolve(&route.id, &route.credential, transport, locations)
}

/// `source`, registering every credential it hands out in `secrets` (issue #484), so
/// every mask the host composes knows the exact value — a credential needs no shape
/// to be masked once p1 has used it. The value is registered before the adapter sees
/// it, so no request can echo a credential the masks do not know yet.
pub(crate) fn registering(
    source: Arc<dyn CredentialSource>,
    secrets: p1_redact::SecretSet,
) -> Arc<dyn CredentialSource> {
    Arc::new(RegisteringSource {
        inner: source,
        secrets,
    })
}

struct RegisteringSource {
    inner: Arc<dyn CredentialSource>,
    secrets: p1_redact::SecretSet,
}

impl RegisteringSource {
    fn register(
        &self,
        result: Result<Credential, ProviderError>,
    ) -> Result<Credential, ProviderError> {
        if let Ok(credential) = &result {
            self.secrets.register(&credential.bearer);
        }
        result
    }
}

impl CredentialSource for RegisteringSource {
    fn access<'a>(&'a self) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.register(self.inner.access().await) })
    }

    fn refresh<'a>(
        &'a self,
        rejected: &'a Credential,
    ) -> BoxFuture<'a, Result<Credential, ProviderError>> {
        Box::pin(async move { self.register(self.inner.refresh(rejected).await) })
    }

    fn proxy_injected(&self) -> bool {
        self.inner.proxy_injected()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use p1_provider_http::testing::ScriptedTransport;

    /// A key the chain resolves is in the set once `access` returned it, and a
    /// proxy-injected route stays proxy-injected through the wrapper.
    #[tokio::test]
    async fn every_credential_handed_out_is_registered() {
        let key = format!("FAKE-{}", "registered-key".repeat(2));
        let lookup = key.clone();
        let locations = Locations::from_environment(std::iter::empty())
            .with_home(None)
            .with_env_lookup(move |name| (name == "P1_HOST_AUTH_TEST_KEY").then(|| lookup.clone()));
        let spec: p1_auth::CredentialSpec =
            serde_json::from_str(r#"{"kind":"api-key","env":"P1_HOST_AUTH_TEST_KEY"}"#).unwrap();
        let transport: Arc<dyn Transport> = Arc::new(ScriptedTransport::new(Vec::new()));
        let secrets = p1_redact::SecretSet::new();
        let source = registering(
            p1_auth::resolve("test-route", &spec, transport.clone(), &locations),
            secrets.clone(),
        );
        assert!(secrets.is_empty());
        assert_eq!(source.access().await.unwrap().bearer, key);
        assert!(secrets.contains_secret(&format!("echo: {key}")));

        let none: p1_auth::CredentialSpec = serde_json::from_str(r#"{"kind":"none"}"#).unwrap();
        let proxied = registering(
            p1_auth::resolve("test-route", &none, transport, &locations),
            secrets,
        );
        assert!(proxied.proxy_injected());
    }
}
