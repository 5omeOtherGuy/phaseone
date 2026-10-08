//! The optional `[capabilities]` table of an environment file (ADR-0124): both families
//! default to on, `false` is read as written, and an unknown key is refused.

mod common;

use common::*;
use p1_assembly::{AssemblyError, EnvironmentCapabilities, load_environment};

const BASE: &str = "family = \"test\"\nprovider = \"test-provider\"\nmodel = \"m\"\n";

#[test]
fn without_the_table_both_families_are_on() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "plain", BASE, "hi");
    let environment = load_environment("plain", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(
        environment.capabilities,
        EnvironmentCapabilities {
            workers: true,
            workflows: true
        }
    );
}

#[test]
fn the_table_is_read_as_written() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(
        dir.path(),
        "leaf",
        &format!("{BASE}\n[capabilities]\nworkers = false\n"),
        "hi",
    );
    let environment = load_environment("leaf", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(
        environment.capabilities,
        EnvironmentCapabilities {
            workers: false,
            workflows: true
        }
    );
}

#[test]
fn an_unknown_key_in_the_table_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(
        dir.path(),
        "typo",
        &format!("{BASE}\n[capabilities]\nworker = false\n"),
        "hi",
    );
    let error = load_environment("typo", &[dir.path().to_path_buf()]).unwrap_err();
    assert!(
        matches!(error, AssemblyError::InvalidEnvironmentFile { .. }),
        "{error:?}"
    );
}

#[test]
fn the_shipped_review_environment_is_a_leaf() {
    let shipped = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../environments");
    let environment = load_environment("deepseek-review", &[shipped]).unwrap();
    assert_eq!(
        environment.capabilities,
        EnvironmentCapabilities {
            workers: false,
            workflows: false
        }
    );
    let modules: Vec<&str> = environment
        .tools
        .iter()
        .map(|tool| tool.module.as_str())
        .collect();
    assert_eq!(
        modules,
        ["read", "grep", "shell", "read_output", "write", "finish"]
    );
}
