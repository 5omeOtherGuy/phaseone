//! One test per `AssemblyError` variant: the variant fires and its message names
//! the offending key/placeholder/path.

mod common;

use std::sync::Arc;

use common::*;
use p1_assembly::{AssemblyError, Catalog, ToolSpec, assemble, load_environment};
use p1_contracts::{Provider, Tool};
use p1_testkit::FakeTool;

fn catalog_with_read() -> Catalog {
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    register_fake_tools(&mut catalog, &["read", "shell"]);
    catalog
}

#[test]
fn environment_not_found_names_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let error = load_environment("missing", &[dir.path().to_path_buf()]).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::EnvironmentNotFound { name, searched } => {
            assert_eq!(name, "missing");
            assert_eq!(searched, &[dir.path().to_path_buf()]);
        }
        other => panic!("expected EnvironmentNotFound, got {other:?}"),
    }
    assert!(message.contains("missing"), "{message}");
}

#[test]
fn invalid_environment_file_names_the_typo() {
    let dir = tempfile::tempdir().unwrap();
    let toml = "family = \"x\"\nfamly = \"y\"\nprovider = \"p\"\nmodel = \"m\"\n";
    write_environment(dir.path(), "bad", toml, "hi");
    let error = load_environment("bad", &[dir.path().to_path_buf()]).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::InvalidEnvironmentFile { path, message } => {
            assert!(path.ends_with("environment.toml"), "{path:?}");
            assert!(message.contains("famly"), "{message}");
        }
        other => panic!("expected InvalidEnvironmentFile, got {other:?}"),
    }
    assert!(message.contains("famly"), "{message}");
}

#[test]
fn unknown_options_key_is_named() {
    let dir = tempfile::tempdir().unwrap();
    let toml = "family = \"x\"\nprovider = \"p\"\nmodel = \"m\"\n\n[options]\nreasoning_effrot = \"medium\"\n";
    write_environment(dir.path(), "opts", toml, "hi");
    let error = load_environment("opts", &[dir.path().to_path_buf()]).unwrap_err();
    let message = error.to_string();
    assert!(matches!(
        &error,
        AssemblyError::InvalidEnvironmentFile { .. }
    ));
    assert!(message.contains("reasoning_effrot"), "{message}");
}

#[test]
fn missing_prompt_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let environment_dir = dir.path().join("noprompt");
    std::fs::create_dir_all(&environment_dir).unwrap();
    std::fs::write(
        environment_dir.join("environment.toml"),
        "family = \"x\"\nprovider = \"p\"\nmodel = \"m\"\n",
    )
    .unwrap();

    let error = load_environment("noprompt", &[dir.path().to_path_buf()]).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::MissingPrompt { path } => assert!(path.ends_with("prompt.md"), "{path:?}"),
        other => panic!("expected MissingPrompt, got {other:?}"),
    }
    assert!(message.contains("prompt.md"), "{message}");
}

#[test]
fn unknown_provider_names_the_key() {
    let catalog = Catalog::new();
    let environment = environment_file("env", "nope", &[], "hello");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::UnknownProvider { key, .. } => assert_eq!(key, "nope"),
        other => panic!("expected UnknownProvider, got {other:?}"),
    }
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn unknown_tool_module_names_the_module() {
    let catalog = catalog_with_read();
    let environment = environment_file("env", "test-provider", &["nope"], "hello");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::UnknownToolModule { module, .. } => assert_eq!(module, "nope"),
        other => panic!("expected UnknownToolModule, got {other:?}"),
    }
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn factory_failed_names_the_key_and_reason() {
    let mut catalog = Catalog::new();
    catalog.provider("test-provider", Box::new(|_spec| Err("boom".to_string())));
    let environment = environment_file("env", "test-provider", &[], "hello");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::FactoryFailed { what, message } => {
            assert_eq!(what, "provider `test-provider`");
            assert_eq!(message, "boom");
        }
        other => panic!("expected FactoryFailed, got {other:?}"),
    }
    assert!(message.contains("test-provider"), "{message}");
    assert!(message.contains("boom"), "{message}");
}

#[test]
fn duplicate_tool_name_names_the_name() {
    let catalog = catalog_with_read();
    let environment = environment_file("env", "test-provider", &["read", "read"], "hello");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::DuplicateToolName { name } => assert_eq!(name, "read"),
        other => panic!("expected DuplicateToolName, got {other:?}"),
    }
    assert!(message.contains("read"), "{message}");
}

#[test]
fn face_not_applied_names_the_module_and_names() {
    let mut catalog = Catalog::new();
    register_scripted_provider(&mut catalog, "test-provider");
    // A factory that ignores the requested face, as a buggy one would.
    catalog.tool(
        "read",
        Box::new(
            |_spec: &ToolSpec, _services| Ok(Arc::new(FakeTool::new("read")) as Arc<dyn Tool>),
        ),
    );
    let environment = environment_with_named_tool("env", "read", "read_file");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::FaceNotApplied {
            module,
            expected,
            got,
        } => {
            assert_eq!(module, "read");
            assert_eq!(expected, "read_file");
            assert_eq!(got, "read");
        }
        other => panic!("expected FaceNotApplied, got {other:?}"),
    }
    assert!(message.contains("read_file"), "{message}");
    assert!(message.contains("read"), "{message}");
}

#[test]
fn unknown_placeholder_names_the_placeholder() {
    let catalog = catalog_with_read();
    let environment = environment_file("env", "test-provider", &["read"], "hello {{nope}}");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::UnknownPlaceholder { placeholder } => assert_eq!(placeholder, "nope"),
        other => panic!("expected UnknownPlaceholder, got {other:?}"),
    }
    assert!(message.contains("nope"), "{message}");
}

#[test]
fn tool_not_in_environment_names_the_module() {
    let catalog = catalog_with_read();
    let environment = environment_file("env", "test-provider", &["read"], "{{tool:shell}}");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::ToolNotInEnvironment { module } => assert_eq!(module, "shell"),
        other => panic!("expected ToolNotInEnvironment, got {other:?}"),
    }
    assert!(message.contains("shell"), "{message}");
}

// ------------------------------------------------------ (d) provider rejection

#[test]
fn freeform_tool_on_a_function_only_route_is_provider_rejected() {
    let mut catalog = Catalog::new();
    catalog.provider(
        "function-only",
        Box::new(|_spec| Ok(Arc::new(FunctionOnlyProvider) as Arc<dyn Provider>)),
    );
    catalog.tool(
        "apply_patch",
        Box::new(|_spec: &ToolSpec, _services| {
            Ok(Arc::new(FreeformTool::new("apply_patch")) as Arc<dyn Tool>)
        }),
    );

    let environment = environment_file("freeform", "function-only", &["apply_patch"], "patch");
    let workspace = tempfile::tempdir().unwrap();
    let error = assemble(&catalog, &environment, workspace.path(), &substitutions()).unwrap_err();
    let message = error.to_string();
    match &error {
        AssemblyError::ProviderRejected(provider_error) => {
            assert!(provider_error.message.contains("apply_patch"));
        }
        other => panic!("expected ProviderRejected, got {other:?}"),
    }
    assert!(message.contains("apply_patch"), "{message}");
}

#[test]
fn invalid_workspace_names_the_path() {
    let catalog = catalog_with_read();
    let environment = environment_file("env", "test-provider", &["read"], "hello");
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("does-not-exist");
    let error = assemble(&catalog, &environment, &missing, &substitutions()).unwrap_err();
    match &error {
        AssemblyError::InvalidWorkspace { message } => {
            assert!(message.contains("does-not-exist"), "{message}");
        }
        other => panic!("expected InvalidWorkspace, got {other:?}"),
    }
}

// ------------------------------------------------------ description_file

#[test]
fn description_file_is_read_relative_to_the_environment() {
    let dir = tempfile::tempdir().unwrap();
    let environment_dir = write_environment(
        dir.path(),
        "described",
        "family = \"test\"\nprovider = \"test-provider\"\nmodel = \"m\"\n\n[[tools]]\nmodule = \"read\"\ndescription_file = \"read.md\"\n",
        "hello",
    );
    std::fs::write(environment_dir.join("read.md"), "Custom read description.").unwrap();

    let environment = load_environment("described", &[dir.path().to_path_buf()]).unwrap();
    assert_eq!(
        environment.tools[0].description.as_deref(),
        Some("Custom read description.")
    );
}

// ------------------------------------------------------ helpers

fn environment_with_named_tool(
    name: &str,
    module: &str,
    tool_name: &str,
) -> p1_assembly::EnvironmentFile {
    let mut environment = environment_file(name, "test-provider", &[module], "hello");
    environment.tools[0].name = Some(tool_name.to_string());
    environment
}
