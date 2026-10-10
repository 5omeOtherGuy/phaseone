//! Instruction and skill inputs use explicit temporary homes, never process HOME.
mod common;

use std::path::Path;

use common::*;
use p1_assembly::{Catalog, InstructionSettings, InstructionSources, assemble, load_environment};

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn catalog(home: &Path) -> Catalog {
    let skill_home = home.to_path_buf();
    let mut catalog = Catalog::new()
        .with_instruction_sources(InstructionSources {
            home: Some(home.into()),
            global: Some(home.join(".agents/AGENTS.md")),
            credential_paths: Vec::new(),
        })
        .with_skills(
            Box::new(move |workspace, settings| {
                std::sync::Arc::new(p1_skill_fs::FilesystemSkills::discover(
                    workspace,
                    Some(&skill_home),
                    &settings.roots,
                    &[],
                ))
            }),
            p1_tool_skill::listing,
        );
    register_scripted_provider(&mut catalog, "test");
    register_fake_tools(&mut catalog, &["skill"]);
    catalog
}

#[test]
fn global_then_git_root_to_workspace_and_first_existing_name() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let workspace = repo.join("a/b");
    std::fs::create_dir_all(&workspace).unwrap();
    write(&repo.join(".git"), "gitdir: elsewhere");
    write(&home.join(".agents/AGENTS.md"), "GLOBAL");
    write(&repo.join("AGENTS.md"), "ROOT");
    write(&repo.join("a/CLAUDE.md"), "PREFERRED");
    write(&repo.join("a/AGENTS.md"), "SHADOWED");
    write(&workspace.join("AGENTS.md"), "WORKSPACE {{unknown}}\n");
    let mut environment = environment_file("test", "test", &[], "ENVIRONMENT");
    environment.instructions.files = vec!["CLAUDE.md".into(), "AGENTS.md".into()];
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    let data = &assembled.resolved.instruction_data;
    assert_eq!(
        data.files
            .iter()
            .map(|file| file.text.as_str())
            .collect::<Vec<_>>(),
        ["GLOBAL", "ROOT", "PREFERRED", "WORKSPACE {{unknown}}\n"]
    );
    assert!(
        assembled
            .system_prompt
            .starts_with("ENVIRONMENT\n\n<instructions>")
    );
    assert!(assembled.system_prompt.contains(&format!(
        "path=\"{}\"",
        workspace.join("AGENTS.md").display()
    )));
    assert!(!assembled.system_prompt.contains("SHADOWED"));
    assert_eq!(
        assemble(&catalog(&home), &environment, &workspace, &substitutions())
            .unwrap()
            .system_prompt,
        assembled.system_prompt
    );
}

#[test]
fn byte_cap_truncates_utf8_and_drops_later_files_even_with_room_left() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    write(&home.join(".agents/AGENTS.md"), "abcérest");
    write(&workspace.join("AGENTS.md"), "later");
    let mut environment = environment_file("test", "test", &[], "base");
    environment.instructions.max_bytes = 4;
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    let files = &assembled.resolved.instruction_data.files;
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].text, "abc");
    assert_eq!(files[0].loaded_bytes, 3);
    let notice = assembled
        .resolved
        .instruction_data
        .notice
        .as_deref()
        .unwrap();
    assert!(notice.contains(&home.join(".agents/AGENTS.md").display().to_string()));
    assert!(notice.contains(&workspace.join("AGENTS.md").display().to_string()));
    assert!(assembled.system_prompt.contains("truncated:"));
    assert!(!assembled.system_prompt.contains("later"));

    write(&home.join(".agents/AGENTS.md"), "1234");
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    assert_eq!(assembled.resolved.instruction_data.files[0].text, "1234");
    assert!(assembled.system_prompt.contains("dropped:"));
    assert!(!assembled.system_prompt.contains("truncated:"));
}

#[test]
fn missing_global_no_git_root_and_disabled_instructions() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("parent/workspace");
    write(&temp.path().join("parent/AGENTS.md"), "OUTSIDE");
    write(&workspace.join("AGENTS.md"), "LOCAL");
    let mut environment = environment_file("test", "test", &[], "base");
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    assert_eq!(assembled.resolved.instruction_data.files.len(), 1);
    assert!(assembled.resolved.instruction_data.warnings.is_empty());
    assert!(!assembled.system_prompt.contains("OUTSIDE"));
    environment.instructions.enabled = false;
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    assert_eq!(assembled.system_prompt, "base");
    assert!(assembled.resolved.instruction_data.files.is_empty());
}

#[test]
fn unreadable_first_match_warns_without_falling_back_or_failing_assembly() {
    let temp = tempfile::tempdir().unwrap();
    let workspace = temp.path().join("workspace");
    std::fs::create_dir_all(workspace.join("AGENTS.md")).unwrap();
    write(&workspace.join("CLAUDE.md"), "must not fall back");
    let mut environment = environment_file("test", "test", &[], "base");
    environment.instructions.files = vec!["AGENTS.md".into(), "CLAUDE.md".into()];
    let assembled = assemble(
        &catalog(&temp.path().join("home")),
        &environment,
        &workspace,
        &substitutions(),
    )
    .unwrap();
    assert_eq!(assembled.system_prompt, "base");
    assert!(assembled.resolved.instruction_data.files.is_empty());
    assert_eq!(assembled.resolved.instruction_data.warnings.len(), 1);
    assert!(assembled.resolved.instruction_data.warnings[0].contains("AGENTS.md"));
}

#[test]
fn skills_user_roots_win_yaml_errors_warn_and_listing_requires_tool() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let repo = temp.path().join("repo");
    let workspace = repo.join("nested");
    std::fs::create_dir_all(&workspace).unwrap();
    write(&repo.join(".git"), "gitdir: elsewhere");
    write(
        &home.join(".agents/skills/example/SKILL.md"),
        "---\ndescription: >-\n  User description with\n  two lines & detail\n---\nUSER BODY",
    );
    write(
        &repo.join(".agents/skills/example/SKILL.md"),
        "---\ndescription: project loses\n---\nPROJECT BODY",
    );
    write(
        &workspace.join(".agents/skills/nested/SKILL.md"),
        "---\nname: renamed\ndescription: nested description\n---\nNESTED BODY",
    );
    write(
        &home.join(".agents/skills/bad/SKILL.md"),
        "---\nname: bad\n---\nno description",
    );
    write(
        &home.join(".agents/skills/broken/SKILL.md"),
        "---\ndescription: [unclosed\n---\nbody",
    );
    write(
        &home.join(".agents/skills/too/deep/SKILL.md"),
        "---\ndescription: must not discover recursively\n---\nbody",
    );
    let mut environment = environment_file("test", "test", &["skill"], "base");
    // User roots take precedence even when configured after project roots.
    environment.skills.roots = vec![".agents/skills".into(), "~/.agents/skills".into()];
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    let data = &assembled.resolved.instruction_data;
    assert_eq!(
        data.skills
            .iter()
            .map(|skill| skill.name.as_str())
            .collect::<Vec<_>>(),
        ["example", "renamed"]
    );
    assert_eq!(
        assembled
            .skills
            .as_ref()
            .unwrap()
            .load("example")
            .unwrap()
            .body,
        "USER BODY"
    );
    assert_eq!(
        data.skills[0].description,
        "User description with two lines & detail"
    );
    assert_eq!(data.warnings.len(), 2);
    assert!(
        data.warnings
            .iter()
            .any(|warning| warning.contains("description"))
    );
    assert!(assembled.system_prompt.contains("&amp; detail"));
    assert!(!assembled.system_prompt.contains("USER BODY"));
    environment.skills.max_listing_chars = 100;
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    assert!(assembled.system_prompt.contains("names only"));
    assert!(assembled.system_prompt.contains("<name>example</name>"));
    assert!(!assembled.system_prompt.contains("<description>"));
    environment.tools.clear();
    let assembled = assemble(&catalog(&home), &environment, &workspace, &substitutions()).unwrap();
    assert_eq!(assembled.system_prompt, "base");
    assert!(assembled.resolved.instruction_data.skills.is_empty());
}

#[test]
fn malformed_environment_blocks_name_the_key() {
    let temp = tempfile::tempdir().unwrap();
    for (block, key) in [
        ("[instructions]\nmax_bytes = 0", "max_bytes"),
        ("[instructions]\nmax_bytes = 1048577", "max_bytes"),
        ("[instructions]\nenabled = 'yes'", "enabled"),
        ("[instructions]\nfiles = 'AGENTS.md'", "files"),
        ("[instructions]\nfiles = ['../AGENTS.md']", "files"),
        ("[instructions]\nunknown = true", "unknown"),
        ("[skills]\nmax_listing_chars = 99", "max_listing_chars"),
        ("[skills]\nmax_listing_chars = 100001", "max_listing_chars"),
        ("[skills]\nroots = true", "roots"),
        ("[skills]\nunknown = true", "unknown"),
    ] {
        write_environment(
            temp.path(),
            "test",
            &format!("provider = 'test'\nmodel = 'test'\nfamily = 'test'\n{block}"),
            "base",
        );
        let error = load_environment("test", &[temp.path().into()])
            .unwrap_err()
            .to_string();
        assert!(error.contains(key), "{block}: {error}");
    }
}

#[test]
fn shipped_instruction_and_skill_inventory_including_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("home");
    let workspace = temp.path().join("workspace");
    write(&workspace.join("AGENTS.md"), "PROJECT");
    write(&home.join(".agents/AGENTS.md"), "GLOBAL");
    write(
        &home.join(".agents/skills/example/SKILL.md"),
        "---\ndescription: example\n---\nbody",
    );
    let mut checked = 0;
    for entry in std::fs::read_dir(shipped_environments()).unwrap() {
        let path = entry.unwrap().path();
        if !path.join("environment.toml").is_file() {
            continue;
        }
        let name = path.file_name().unwrap().to_str().unwrap();
        let environment = load_environment(name, &[shipped_environments()]).unwrap();
        let expected = match name {
            "claude" | "claude2" => vec!["CLAUDE.md", "AGENTS.md"],
            "gpt" => vec!["AGENTS.override.md", "AGENTS.md"],
            _ => vec!["AGENTS.md"],
        };
        assert_eq!(environment.instructions.files, expected, "{name}");
        let expected_roots = if matches!(name, "claude" | "claude2") {
            vec![
                "~/.claude/skills",
                ".claude/skills",
                "~/.agents/skills",
                ".agents/skills",
            ]
        } else {
            vec!["~/.agents/skills", ".agents/skills"]
        };
        assert_eq!(environment.skills.roots, expected_roots, "{name}");
        assert_eq!(environment.skills.max_listing_chars, 8000, "{name}");
        assert_eq!(
            environment.instructions.max_bytes,
            InstructionSettings::default().max_bytes,
            "{name}"
        );
        let enabled = !matches!(name, "zen" | "zen2" | "zen3");
        assert_eq!(environment.instructions.enabled, enabled, "{name}");
        let skills = matches!(
            name,
            "claude"
                | "claude2"
                | "gpt"
                | "glm"
                | "glm-messages"
                | "kimi"
                | "deepseek"
                | "deepseek-messages"
                | "deepseek1"
                | "deepseek2"
                | "deepseek3"
        );
        assert_eq!(
            environment.tools.iter().any(|tool| tool.module == "skill"),
            skills,
            "{name}"
        );
        if skills {
            assert_eq!(environment.tools.last().unwrap().module, "skill", "{name}");
        }
        let mut catalog = catalog(&home);
        register_scripted_provider(&mut catalog, &environment.provider);
        register_fake_tools(
            &mut catalog,
            &environment
                .tools
                .iter()
                .map(|tool| tool.module.as_str())
                .collect::<Vec<_>>(),
        );
        let assembled = assemble(&catalog, &environment, &workspace, &substitutions()).unwrap();
        assert_eq!(
            assembled.system_prompt.contains("<instructions>"),
            enabled,
            "{name}"
        );
        assert_eq!(
            assembled.system_prompt.contains("<skills>"),
            skills,
            "{name}"
        );
        checked += 1;
    }
    assert_eq!(checked, 20);
}
