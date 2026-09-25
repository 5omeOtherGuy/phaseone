//! The optional `[context]` table and `summarize.md` override of an environment
//! file: parsed, validated and exposed on `ResolvedEnvironment`.
//!
//! Specification: `docs/design/context.md` §3.

mod common;

use common::*;
use p1_assembly::{AssemblyError, assemble, load_environment};

const BASE: &str = "family = \"test\"\nprovider = \"test-provider\"\nmodel = \"m\"\n";

const TABLE: &str = "[context]\nwindow_tokens = 200000\noutput_headroom_tokens = 16000\nsummarize_at_tokens = 120000\nkeep_recent_tokens = 30000\nuser_verbatim_tokens = 8000\n";

fn catalog() -> p1_assembly::Catalog {
    let mut catalog = p1_assembly::Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    catalog
}

#[test]
fn a_context_table_is_parsed_and_exposed() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "ctx", &format!("{BASE}\n{TABLE}"), "hi");
    let environment = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap();
    let settings = environment.context.clone().expect("the table is present");
    assert_eq!(settings.window_tokens, 200_000);
    assert_eq!(settings.output_headroom_tokens, 16_000);
    assert_eq!(settings.summarize_at_tokens, 120_000);
    assert_eq!(settings.keep_recent_tokens, 30_000);
    assert_eq!(settings.user_verbatim_tokens, 8_000);
    // `tool_result_excerpt_chars` is optional and defaults to 2000.
    assert_eq!(settings.tool_result_excerpt_chars, 2_000);
    assert!(environment.summarize_prompt.is_none());

    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog(), &environment, workspace.path(), &substitutions()).unwrap();
    assert_eq!(assembled.resolved.context.as_ref(), Some(&settings));
    let json = serde_json::to_string(&assembled.resolved).unwrap();
    assert!(json.contains("\"summarize_at_tokens\":120000"), "{json}");
}

#[test]
fn an_environment_without_the_table_is_passthrough() {
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "plain", BASE, "hi");
    let environment = load_environment("plain", &[dir.path().to_path_buf()]).unwrap();
    assert!(environment.context.is_none());
    assert!(environment.summarize_prompt.is_none());

    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog(), &environment, workspace.path(), &substitutions()).unwrap();
    assert!(assembled.resolved.context.is_none());
    let json = serde_json::to_string(&assembled.resolved).unwrap();
    assert!(!json.contains("\"context\""), "{json}");
}

#[test]
fn an_optional_excerpt_budget_is_honoured() {
    let dir = tempfile::tempdir().unwrap();
    let toml = format!("{BASE}\n{TABLE}tool_result_excerpt_chars = 500\n");
    write_environment(dir.path(), "ctx", &toml, "hi");
    let environment = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(environment.context.unwrap().tool_result_excerpt_chars, 500);
}

// The summary-output cap of context.md "Revision 2026-09-20": optional, defaulted,
// validated and shown on the resolved environment.
#[test]
fn the_summary_output_cap_is_optional_validated_and_shown() {
    // Absent: the compiled-in default, so existing environments are unchanged.
    let dir = tempfile::tempdir().unwrap();
    write_environment(dir.path(), "ctx", &format!("{BASE}\n{TABLE}"), "hi");
    let environment = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(environment.context.unwrap().summary_output_tokens, 4_000);

    // Present: parsed, kept, and displayed in the resolved environment.
    let dir = tempfile::tempdir().unwrap();
    let toml = format!("{BASE}\n{TABLE}summary_output_tokens = 12000\n");
    write_environment(dir.path(), "ctx", &toml, "hi");
    let environment = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(
        environment.context.clone().unwrap().summary_output_tokens,
        12_000
    );
    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog(), &environment, workspace.path(), &substitutions()).unwrap();
    let json = serde_json::to_string(&assembled.resolved).unwrap();
    assert!(json.contains("\"summary_output_tokens\":12000"), "{json}");

    // Zero is refused...
    let dir = tempfile::tempdir().unwrap();
    let toml = format!("{BASE}\n{TABLE}summary_output_tokens = 0\n");
    write_environment(dir.path(), "ctx", &toml, "hi");
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    match &error {
        AssemblyError::InvalidContext { message } => {
            assert!(message.contains("summary_output_tokens"), "{message}");
        }
        other => panic!("expected InvalidContext, got {other:?}"),
    }

    // ...and so is a cap that leaves no room under the wall (200000 - 16000).
    let dir = tempfile::tempdir().unwrap();
    let toml = format!("{BASE}\n{TABLE}summary_output_tokens = 184000\n");
    write_environment(dir.path(), "ctx", &toml, "hi");
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    match &error {
        AssemblyError::InvalidContext { message } => {
            assert!(message.contains("summary_output_tokens"), "{message}");
            assert!(message.contains("184000"), "{message}");
        }
        other => panic!("expected InvalidContext, got {other:?}"),
    }
}

#[test]
fn summarize_md_is_read_as_the_prompt_override() {
    let dir = tempfile::tempdir().unwrap();
    let environment_dir = write_environment(dir.path(), "ctx", &format!("{BASE}\n{TABLE}"), "hi");
    std::fs::write(environment_dir.join("summarize.md"), "CUSTOM PROMPT\n").unwrap();
    let environment = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(
        environment.summarize_prompt.as_deref(),
        Some("CUSTOM PROMPT\n")
    );

    let workspace = tempfile::tempdir().unwrap();
    let assembled = assemble(&catalog(), &environment, workspace.path(), &substitutions()).unwrap();
    assert_eq!(
        assembled.resolved.summarize_prompt.as_deref(),
        Some("CUSTOM PROMPT\n")
    );
}

#[test]
fn a_missing_token_field_is_an_environment_file_error() {
    let dir = tempfile::tempdir().unwrap();
    let toml = "family = \"test\"\nprovider = \"p\"\nmodel = \"m\"\n\n[context]\nwindow_tokens = 100\noutput_headroom_tokens = 10\nsummarize_at_tokens = 50\nkeep_recent_tokens = 10\n";
    write_environment(dir.path(), "ctx", toml, "hi");
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    assert!(matches!(
        &error,
        AssemblyError::InvalidEnvironmentFile { .. }
    ));
    assert!(
        error.to_string().contains("user_verbatim_tokens"),
        "{error}"
    );
}

#[test]
fn an_unknown_context_key_is_an_environment_file_error() {
    let dir = tempfile::tempdir().unwrap();
    let toml = format!("{BASE}\n{TABLE}summarize_everything = true\n");
    write_environment(dir.path(), "ctx", &toml, "hi");
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    assert!(matches!(
        &error,
        AssemblyError::InvalidEnvironmentFile { .. }
    ));
    assert!(
        error.to_string().contains("summarize_everything"),
        "{error}"
    );
}

#[test]
fn a_threshold_at_the_wall_names_the_environment_file() {
    let dir = tempfile::tempdir().unwrap();
    let toml = "family = \"test\"\nprovider = \"p\"\nmodel = \"m\"\n\n[context]\nwindow_tokens = 100\noutput_headroom_tokens = 20\nsummarize_at_tokens = 80\nkeep_recent_tokens = 10\nuser_verbatim_tokens = 10\n";
    write_environment(dir.path(), "ctx", toml, "hi");
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    match &error {
        AssemblyError::InvalidContext { message } => {
            assert!(message.contains("environment.toml"), "{message}");
            assert!(message.contains("summarize_at_tokens"), "{message}");
        }
        other => panic!("expected InvalidContext, got {other:?}"),
    }
}

#[test]
fn a_zero_field_is_an_invalid_context() {
    let dir = tempfile::tempdir().unwrap();
    let toml = "family = \"test\"\nprovider = \"p\"\nmodel = \"m\"\n\n[context]\nwindow_tokens = 100\noutput_headroom_tokens = 20\nsummarize_at_tokens = 50\nkeep_recent_tokens = 0\nuser_verbatim_tokens = 10\n";
    write_environment(dir.path(), "ctx", toml, "hi");
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    match &error {
        AssemblyError::InvalidContext { message } => {
            assert!(message.contains("keep_recent_tokens"), "{message}");
        }
        other => panic!("expected InvalidContext, got {other:?}"),
    }
}

#[test]
fn an_empty_summarize_md_names_that_file() {
    let dir = tempfile::tempdir().unwrap();
    let environment_dir = write_environment(dir.path(), "ctx", &format!("{BASE}\n{TABLE}"), "hi");
    std::fs::write(environment_dir.join("summarize.md"), "\n  \n").unwrap();
    let error = load_environment("ctx", &[dir.path().to_path_buf()]).unwrap_err();
    match &error {
        AssemblyError::InvalidContext { message } => {
            assert!(message.contains("summarize.md"), "{message}");
        }
        other => panic!("expected InvalidContext, got {other:?}"),
    }
}

/// Every SHIPPED environment's `[context]` table loads and validates. The per-route windows
/// came from public sources (docs/design/context-windows.md) and are hand-written data: a
/// typo would otherwise only surface when an agent is built. The sweep runs over the
/// directory, so a new environment cannot slip past the check by not being named here.
#[test]
fn every_shipped_environment_has_a_valid_context_table() {
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
        let context = environment
            .context
            .as_ref()
            .unwrap_or_else(|| panic!("{name} ships without a [context] table"));
        context
            .validate()
            .unwrap_or_else(|error| panic!("{name}: {error}"));
        // The two rules the table exists for, spelled out: both numbers stay under the wall,
        // and the threshold is a round 10k (the rounding rule of the #125 retune).
        let wall = context.window_tokens - context.output_headroom_tokens;
        assert!(
            context.summarize_at_tokens < wall,
            "{name}: summarize_at_tokens ({}) is not below the wall ({wall})",
            context.summarize_at_tokens
        );
        assert!(
            context.summary_output_tokens < wall,
            "{name}: summary_output_tokens ({}) is not below the wall ({wall})",
            context.summary_output_tokens
        );
        assert_eq!(
            context.summarize_at_tokens % 10_000,
            0,
            "{name}: summarize_at_tokens ({}) is not rounded to 10k",
            context.summarize_at_tokens
        );
        checked.push(name);
    }
    checked.sort();
    assert_eq!(
        checked.len(),
        13,
        "every shipped environment was checked: {checked:?}"
    );
    for required in ["claude", "gpt", "deepseek3", "zen", "kimi"] {
        assert!(
            checked.iter().any(|name| name.as_str() == required),
            "`{required}` was not checked: {checked:?}"
        );
    }
}
