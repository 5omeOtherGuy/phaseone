//! Profile selection through the REAL catalog: an environment names a route and a
//! profile, `p1-host` resolves the model policy from `profiles/<id>.toml`, and each
//! catalog key accepts exactly one form. Assembling touches no network and no
//! credential: a provider is constructed lazily.

mod common;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use common::{Harness, shipped_environments};
use p1_assembly::{Assembled, AssemblyError, Catalog, Substitutions, assemble, load_environment};
use p1_contracts::Origin;
use p1_host::activity::CompletionHub;
use p1_host::cli::SandboxMode;
use tempfile::tempdir;

/// The repository's own `profiles/` directory.
fn shipped_profiles() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles")
}

/// Write `<environments>/<name>/{environment.toml,prompt.md}`.
fn write_environment(environments: &Path, name: &str, toml: &str) {
    let dir = environments.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("environment.toml"), toml).unwrap();
    std::fs::write(dir.join("prompt.md"), "prompt").unwrap();
}

fn substitutions(workspace: &Path) -> Substitutions {
    Substitutions {
        workspace: workspace.display().to_string(),
        date: "2026-09-20".into(),
        os: "linux".into(),
    }
}

/// The real catalog with the real providers; nothing here reads a credential.
fn catalog(harness: &Harness) -> Catalog {
    let completion = Arc::new(CompletionHub::new());
    p1_host::catalog::build_catalog(&harness.deps, SandboxMode::Off, &[], &[], &completion)
}

/// Assemble one environment from the shipped `environments/` directory.
fn assemble_shipped(name: &str) -> Assembled {
    let harness = Harness::new(vec![shipped_environments()], &[]);
    let catalog = catalog(&harness);
    let environment = load_environment(name, &harness.deps.environment_dirs).unwrap();
    let workspace = tempdir().unwrap();
    assemble(
        &catalog,
        &environment,
        workspace.path(),
        &substitutions(workspace.path()),
    )
    .unwrap_or_else(|error| panic!("{name} must assemble: {error}"))
}

fn origin(route: &str, model: &str) -> Origin {
    Origin {
        route: route.into(),
        model: model.into(),
    }
}

// ------------------------------------------------------ the shipped environments

#[test]
fn the_shipped_deepseek_environment_selects_its_route_and_profile() {
    let assembled = assemble_shipped("deepseek");
    assert_eq!(assembled.resolved.family, "deepseek");
    // Byte-for-byte what the pre-split host recorded.
    assert_eq!(
        assembled.resolved.route.origin,
        origin(
            "openai-chat/opencode-go-subscription",
            "deepseek-v4.1-flash"
        )
    );
}

#[test]
fn the_shipped_glm_environment_selects_its_route_and_profile() {
    let assembled = assemble_shipped("glm");
    assert_eq!(assembled.resolved.family, "glm");
    assert_eq!(
        assembled.resolved.route.origin,
        origin("openai-chat/glm-subscription", "glm-5.3")
    );
}

#[test]
fn the_loaded_environments_carry_the_parsed_profile() {
    let harness = Harness::new(vec![shipped_environments()], &[]);
    for (name, route, profile_id, family) in [
        (
            "deepseek",
            "opencode-go-subscription",
            "deepseek-v4.1-flash",
            "deepseek",
        ),
        ("glm", "glm-subscription", "glm-5.3", "glm"),
    ] {
        let environment = load_environment(name, &harness.deps.environment_dirs).unwrap();
        let profile = environment
            .profile
            .as_ref()
            .unwrap_or_else(|| panic!("{name} must load a profile"));
        assert_eq!(profile.id, profile_id);
        assert_eq!(profile.revision, 1);
        assert_eq!(profile.family, family);
        assert_eq!(environment.provider, route);
        // The family and the wire model are the profile's, not the file's.
        assert_eq!(environment.family, profile.family);
        assert_eq!(environment.model, profile.model_id);
    }
}

// ------------------------------------------------------ one form per key

#[test]
fn a_chat_key_in_the_old_form_is_refused_and_says_which_form_to_write() {
    for key in ["opencode-go-subscription", "glm-subscription"] {
        let root = tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        write_environment(
            &environments,
            "legacy",
            &format!("family = \"legacy\"\nprovider = \"{key}\"\nmodel = \"some-model\"\n"),
        );
        let harness = Harness::new(vec![environments.clone()], &[]);
        let catalog = catalog(&harness);
        let environment = load_environment("legacy", &[environments]).unwrap();
        let workspace = tempdir().unwrap();
        let error = assemble(
            &catalog,
            &environment,
            workspace.path(),
            &substitutions(workspace.path()),
        )
        .unwrap_err();
        let message = error.to_string();
        match &error {
            AssemblyError::FactoryFailed { what, message } => {
                assert_eq!(what, &format!("provider `{key}`"));
                for form in ["`route`", "`profile`", "`provider`", "`model`", "`family`"] {
                    assert!(message.contains(form), "{key}: {message}");
                }
            }
            other => panic!("expected FactoryFailed for {key}, got {other:?}"),
        }
        assert!(message.contains(key), "{message}");
    }
}

#[test]
fn a_whole_provider_in_the_new_form_is_refused_and_says_which_form_to_write() {
    for key in ["anthropic-subscription", "openai-codex-subscription"] {
        let root = tempdir().unwrap();
        let environments = root.path().join("environments");
        std::fs::create_dir_all(&environments).unwrap();
        let profiles = root.path().join("profiles");
        std::fs::create_dir_all(&profiles).unwrap();
        std::fs::copy(
            shipped_profiles().join("deepseek-v4.1-flash.toml"),
            profiles.join("deepseek-v4.1-flash.toml"),
        )
        .unwrap();
        write_environment(
            &environments,
            "routed",
            &format!("route = \"{key}\"\nprofile = \"deepseek-v4.1-flash\"\n"),
        );

        let harness = Harness::new(vec![environments.clone()], &[]);
        let catalog = catalog(&harness);
        let environment = load_environment("routed", &[environments]).unwrap();
        let workspace = tempdir().unwrap();
        let error = assemble(
            &catalog,
            &environment,
            workspace.path(),
            &substitutions(workspace.path()),
        )
        .unwrap_err();
        let message = error.to_string();
        match &error {
            AssemblyError::FactoryFailed { what, message } => {
                assert_eq!(what, &format!("provider `{key}`"));
                for form in ["`route`", "`profile`", "`provider`", "`model`", "`family`"] {
                    assert!(message.contains(form), "{key}: {message}");
                }
            }
            other => panic!("expected FactoryFailed for {key}, got {other:?}"),
        }
        assert!(message.contains(key), "{message}");
    }
}
