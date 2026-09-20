//! The host's credential seam: this crate composes `p1-auth`, and nowhere else does
//! (ADR-0040, spec §2). The host composes; the crate that owns the chain resolves.
//!
//! Nothing here reads a credential file: a [`CredentialSource`] is built lazily and
//! reads its source on every `access`. Where it reads from is decided by the
//! `[credential]` table of the route file, never by this crate.

use std::sync::Arc;

use p1_auth::Locations;
use p1_provider_http::{CredentialSource, Transport};

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
