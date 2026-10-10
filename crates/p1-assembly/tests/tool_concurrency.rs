//! The optional `[tool_concurrency]` table of an environment file (ADR-0118, owner amendment
//! 2026-10-09): parsed, validated per key, defaulted per key and exposed on
//! `ResolvedEnvironment`; and the values the shipped environments resolve to.

mod common;

use std::num::NonZeroUsize;

use common::*;
use p1_assembly::{AssemblyError, ToolConcurrency, assemble, load_environment};

const BASE: &str = "family = \"test\"\nprovider = \"test-provider\"\nmodel = \"m\"\n";

fn load(table: &str) -> Result<ToolConcurrency, AssemblyError> {
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "env", &format!("{BASE}\n{table}"), "hi");
    load_environment("env", &[dir.path().to_path_buf()]).map(|env| env.tool_concurrency)
}

fn values(max_parallel: usize, shell_reads: bool) -> ToolConcurrency {
    ToolConcurrency {
        max_parallel: NonZeroUsize::new(max_parallel).unwrap(),
        shell_reads,
    }
}

#[test]
fn an_absent_table_takes_the_adr_default() {
    assert_eq!(load("").unwrap(), values(10, true));
    assert_eq!(load("").unwrap(), ToolConcurrency::default());
    assert_eq!(load("[tool_concurrency]\n").unwrap(), values(10, true));
}

#[test]
fn each_key_alone_keeps_the_others_default() {
    assert_eq!(
        load("[tool_concurrency]\nmax_parallel = 4\n").unwrap(),
        values(4, true)
    );
    assert_eq!(
        load("[tool_concurrency]\nshell_reads = false\n").unwrap(),
        values(10, false)
    );
    assert_eq!(
        load("[tool_concurrency]\nmax_parallel = 3\nshell_reads = false\n").unwrap(),
        values(3, false)
    );
}

#[test]
fn the_bounds_one_and_ten_are_accepted() {
    assert_eq!(
        load("[tool_concurrency]\nmax_parallel = 1\n").unwrap(),
        values(1, true)
    );
    assert_eq!(
        load("[tool_concurrency]\nmax_parallel = 10\n").unwrap(),
        values(10, true)
    );
}

#[test]
fn values_out_of_range_are_refused_naming_the_key() {
    for value in ["0", "11", "-1", "1000000000000"] {
        let error = load(&format!("[tool_concurrency]\nmax_parallel = {value}\n")).unwrap_err();
        assert!(
            matches!(error, AssemblyError::InvalidToolConcurrency { .. }),
            "{value}: {error:?}"
        );
        let message = error.to_string();
        assert!(
            message.contains(&format!("max_parallel ({value}) must be from 1 to 10")),
            "{message}"
        );
        assert!(message.contains("environment.toml"), "{message}");
    }
}

#[test]
fn a_wrong_type_is_refused_naming_the_key() {
    for (table, key) in [
        ("max_parallel = \"4\"", "max_parallel"),
        ("max_parallel = 2.5", "max_parallel"),
        ("shell_reads = \"no\"", "shell_reads"),
        ("shell_reads = 0", "shell_reads"),
    ] {
        let error = load(&format!("[tool_concurrency]\n{table}\n")).unwrap_err();
        assert!(
            matches!(error, AssemblyError::InvalidEnvironmentFile { .. }),
            "{table}: {error:?}"
        );
        assert!(error.to_string().contains(key), "{table}: {error}");
    }
}

#[test]
fn an_unknown_key_is_refused_naming_it() {
    let error =
        load("[tool_concurrency]\nmax_parallel = 2\nparallel_tool_calls = true\n").unwrap_err();
    assert!(matches!(
        error,
        AssemblyError::InvalidEnvironmentFile { .. }
    ));
    assert!(error.to_string().contains("parallel_tool_calls"), "{error}");
}

#[test]
fn the_table_reaches_the_resolved_environment_and_the_tool_services() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(
        dir.path(),
        "env",
        &format!("{BASE}\n[tool_concurrency]\nmax_parallel = 2\nshell_reads = false\n"),
        "hi",
    );
    let environment = load_environment("env", &[dir.path().to_path_buf()]).unwrap();
    let mut catalog = p1_assembly::Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap();
    assert_eq!(assembled.resolved.tool_concurrency, values(2, false));
    let json = serde_json::to_value(&assembled.resolved).unwrap();
    assert_eq!(
        json["tool_concurrency"],
        serde_json::json!({"max_parallel": 2, "shell_reads": false})
    );
}

/// Requirement 3 of #592's brief: Claude (and its alias `claude2`) run every shell call
/// alone; every other shipped environment omits the table and gets the ADR's default.
#[test]
fn shipped_environments_resolve_to_their_vendors_behaviour() {
    let dirs = [shipped_environments()];
    for name in ["claude", "claude2"] {
        let environment = load_environment(name, &dirs).unwrap();
        assert_eq!(environment.tool_concurrency, values(10, false), "{name}");
    }
    for name in ["gpt", "glm", "glm-messages", "glm-go", "deepseek", "kimi"] {
        let environment = load_environment(name, &dirs).unwrap();
        assert_eq!(environment.tool_concurrency, values(10, true), "{name}");
    }
}
