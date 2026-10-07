//! No shipped environment or model profile sends `max` reasoning effort (#426).
//!
//! Owner decision 2026-09-27: DeepSeek V4.1 Flash and MiMo V2.6 Flash run at effort high,
//! never max ("We almost never want max"); the fleet rule since then is that max on any
//! model needs an explicit owner order. A shipped default of `max` would silently cost
//! more time and tokens on every run, so the sweep covers every directory entry and a new
//! environment or profile cannot slip past it by not being named here.

mod common;

use std::path::Path;

use common::shipped_environments;
use p1_assembly::load_environment;
use p1_contracts::Effort;

/// The effort an environment actually sends: its own `reasoning_effort`, else its
/// profile's default. Every shipped environment states it, so the run's `Environment`
/// journal record names the effort sent.
#[test]
fn no_shipped_environment_sends_max_effort() {
    let root = shipped_environments();
    let mut checked: Vec<String> = Vec::new();
    for entry in std::fs::read_dir(&root).expect("the shipped environments directory is readable") {
        let path = entry.expect("a readable entry").path();
        if !path.join("environment.toml").is_file() {
            continue;
        }
        let name = path
            .file_name()
            .expect("a directory name")
            .to_string_lossy()
            .into_owned();
        let environment = load_environment(&name, std::slice::from_ref(&root))
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        // The journal's `Environment` record carries `options`, so an explicit effort is
        // what makes every run log state the effort it sent (#426).
        assert!(
            environment.options.reasoning_effort.is_some(),
            "{name}: a shipped environment states its reasoning_effort explicitly (#426)"
        );
        let effective = environment.options.reasoning_effort.or_else(|| {
            environment
                .profile
                .as_ref()
                .and_then(|profile| profile.default_effort)
        });
        assert_ne!(
            effective,
            Some(Effort::Max),
            "{name}: a shipped environment must not send max effort (owner rule, #426)"
        );
        checked.push(name);
    }
    checked.sort();
    for required in [
        "deepseek",
        "deepseek3",
        "zen",
        "glm",
        "kimi",
        "claude",
        "gpt",
    ] {
        assert!(
            checked.iter().any(|name| name == required),
            "`{required}` was not checked: {checked:?}"
        );
    }
}

/// A profile may list `max` among the efforts a caller can ask for explicitly, but its
/// default is never `max`.
#[test]
fn no_shipped_profile_defaults_to_max_effort() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../profiles");
    let mut checked = 0usize;
    for entry in std::fs::read_dir(&root).expect("the shipped profiles directory is readable") {
        let path = entry.expect("a readable entry").path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("toml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("a readable profile");
        let table: toml::Table =
            toml::from_str(&text).unwrap_or_else(|error| panic!("{}: {error}", path.display()));
        assert_ne!(
            table.get("default_effort").and_then(|value| value.as_str()),
            Some("max"),
            "{}: a shipped profile must not default to max effort (owner rule, #426)",
            path.display()
        );
        checked += 1;
    }
    assert!(checked > 0, "no profile was checked in {}", root.display());
}
