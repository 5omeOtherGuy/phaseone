//! The prompt-data port is explicitly granted; no native tool bypasses the module.
use std::sync::Arc;

use p1_contracts::serde_json::json;
use p1_contracts::skill::{LoadedSkill, SkillError, SkillListing, SkillSource};
use p1_module_runtime::{Digest, ExecutionLimits, LoadError, Services, wasm_tool};
use p1_module_tests::Release;
use p1_redact::MaskCounter;

fn release(name: &str, capabilities: serde_json::Value) -> Release {
    let directory = p1_module_tests::fixture_dir()
        .parent()
        .unwrap()
        .join("p1-module-skill");
    let bytes =
        std::fs::read(directory.join("p1-module-skill.wasm")).expect("build skill module first");
    let mut release = Release::empty();
    release.add(
        json!({
            "name": name, "digest": Digest::of(&bytes).to_string(),
            "path": "packages/skill/skill.wasm", "kind": "tool", "world": "p1:module/tool@1.0.0",
            "protocol": "1.0", "capabilities": capabilities, "variant": "default",
        }),
        &bytes,
    );
    release
}

struct EmptySource;
impl SkillSource for EmptySource {
    fn list(&self) -> SkillListing {
        SkillListing::default()
    }
    fn load(&self, name: &str) -> Result<LoadedSkill, SkillError> {
        Err(SkillError(format!("unknown skill: {name}")))
    }
}

#[tokio::test]
async fn skill_requires_declared_grant_and_assembled_source() {
    let denied = release("p1/skill", json!([]));
    assert!(matches!(
        denied.loader().load("p1/skill"),
        Err(LoadError::UndeclaredImport { .. })
    ));
    let foreign = release("p1/other", json!(["skills"]));
    assert!(matches!(
        foreign.loader().load("p1/other"),
        Err(LoadError::UndeclaredImport { .. })
    ));
    // Verification and loading use the same manifest validation, even before compilation.
    let manifest = p1_module_runtime::ReleaseManifest::read(&foreign.manifest_file()).unwrap();
    assert!(
        p1_module_runtime::loader::manifest_field_errors(manifest.entry("p1/other").unwrap())
            .iter()
            .any(|error| matches!(error, LoadError::UndeclaredImport { .. }))
    );

    let allowed = release("p1/skill", json!(["skills"]));
    let loaded = allowed.loader().load("p1/skill").unwrap();
    assert!(
        wasm_tool(
            &loaded,
            Services::default(),
            ExecutionLimits::default(),
            &Arc::new(MaskCounter::new())
        )
        .is_err()
    );
    // Declaration and Shared/ReadOnly descriptions work on the restricted path,
    // where imports trap; none consults the skill source.
    let tool = wasm_tool(
        &loaded,
        Services {
            skills: Some(Arc::new(EmptySource)),
            ..Services::default()
        },
        ExecutionLimits::default(),
        &Arc::new(MaskCounter::new()),
    )
    .unwrap();
    assert_eq!(tool.declaration().name, "skill");
}
