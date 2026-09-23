//! ADR-0053 items 4 and 7: the `[workflows]` table of `settings.toml` over the shipped
//! defaults, and the host's model resolver — `environment/profile[:effort]` only, the
//! WIRE model from the environment's route.
#![cfg(feature = "workflows")]

mod common;
mod workflow_common;

use p1_host::workflow::HostModelResolver;
use p1_workflow::ModelResolver;
use workflow_common::Scratch;

fn settings(scratch: &Scratch) -> Result<p1_workflow::WorkflowSettings, String> {
    let harness = scratch.harness();
    p1_host::models::workflow_settings(&p1_host::auth::locations(&harness.deps))
}

#[test]
fn the_user_table_overrides_a_role_and_keeps_the_rest_shipped() {
    let scratch = Scratch::with_settings(
        "[workflows.roles.worker]\nmodel = \"fake/other:high\"\ntools = [\"read\"]\n",
    );
    let settings = settings(&scratch).expect("the table loads");
    let shipped = p1_workflow::WorkflowSettings::shipped();
    assert_eq!(settings.roles["worker"].model, "fake/other:high");
    assert_eq!(settings.roles["worker"].tools, ["read"]);
    assert_eq!(settings.roles["judge"], shipped.roles["judge"]);
    assert_eq!(settings.caps, shipped.caps);
}

#[test]
fn no_table_means_the_shipped_defaults() {
    let scratch = Scratch::with_settings("");
    assert_eq!(
        settings(&scratch).unwrap(),
        p1_workflow::WorkflowSettings::shipped()
    );
}

#[test]
fn an_unknown_key_is_an_error_that_names_it() {
    let scratch = Scratch::with_settings("[workflows]\nmax_stepz = 3\n");
    let error = settings(&scratch).unwrap_err();
    assert!(error.contains("max_stepz"), "{error}");
}

#[test]
fn the_resolver_refuses_a_bare_profile() {
    let scratch = Scratch::new();
    let resolver = HostModelResolver {
        environment_dirs: scratch.environment_dirs(),
    };
    assert_eq!(
        resolver.resolve("other").unwrap_err(),
        "a role names environment/profile[:effort], got \"other\""
    );
    assert_eq!(
        resolver.resolve("other:high").unwrap_err(),
        "a role names environment/profile[:effort], got \"other:high\""
    );
}

#[test]
fn the_resolver_returns_the_routes_wire_model_and_the_effort() {
    let scratch = Scratch::new();
    let resolver = HostModelResolver {
        environment_dirs: scratch.environment_dirs(),
    };
    let resolved = resolver.resolve("fake/other:high").unwrap();
    assert_eq!(resolved.reference, "fake/other:high");
    assert_eq!(resolved.environment, "fake");
    assert_eq!(resolved.profile, "other");
    assert_eq!(resolved.effort.as_deref(), Some("high"));
    assert_eq!(resolved.wire_model, "wire-other");

    let plain = resolver.resolve("fake/main").unwrap();
    assert_eq!(plain.wire_model, "wire-main");
    assert_eq!(plain.effort, None);

    assert!(resolver.resolve("fake/missing").is_err());
    assert!(resolver.resolve("nowhere/main").is_err());
}
