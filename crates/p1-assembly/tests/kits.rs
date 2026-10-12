//! Kits (ADR-0217 Decision 2): an environment leaves tables to the kit of its profile's family.
//! Per table the model's file, else the family's `kit.toml`, else `kits/base/kit.toml`; each
//! file is found across the layers, specificity before layer priority, the first file per
//! relative path winning whole; a table is taken whole and its absent keys take their defaults.

use std::path::{Path, PathBuf};

use p1_assembly::{AssemblyError, load_environment, load_environment_for_profile};

fn write(path: PathBuf, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A catalog layer at `root`: `environments/` and the profiles `alpha` (family `fam`) and
/// `other` (family `elsewhere`, which has no kit).
fn layer(root: &Path) -> PathBuf {
    for (id, family) in [("alpha", "fam"), ("other", "elsewhere")] {
        write(
            root.join(format!("profiles/{id}.toml")),
            &format!(
                "id = \"{id}\"\nrevision = 1\nmodel_id = \"{id}-wire\"\nfamily = \"{family}\"\n\
                 thinking = \"enabled\"\nefforts = [\"high\"]\ndefault_effort = \"high\"\n"
            ),
        );
    }
    std::fs::create_dir_all(root.join("environments")).unwrap();
    root.join("environments")
}

fn tools(modules: &[&str]) -> String {
    modules
        .iter()
        .map(|module| format!("[[tools]]\nmodule = \"{module}\"\n"))
        .collect()
}

fn modules(name: &str, dirs: &[PathBuf]) -> Vec<String> {
    load_environment(name, dirs)
        .unwrap()
        .tools
        .into_iter()
        .map(|tool| tool.module)
        .collect()
}

/// The shipped layer of these tests: a base kit, a family kit and one model's override.
fn shipped(root: &Path) -> PathBuf {
    let environments = layer(root);
    write(
        root.join("kits/base/kit.toml"),
        &format!(
            "prompt_file = \"prompt.md\"\n[options]\nreasoning_effort = \"high\"\n\
             [skills]\nroots = [\"base-skills\"]\n{}",
            tools(&["read", "shell"])
        ),
    );
    write(root.join("kits/base/prompt.md"), "the base prompt");
    write(
        root.join("kits/fam/kit.toml"),
        &format!(
            "prompt_file = \"fam.md\"\n[tool_concurrency]\nmax_parallel = 4\nshell_reads = false\n\
             [context]\noutput_headroom_tokens = 1000\nsummarize_at_tokens = 5000\n\
             keep_recent_tokens = 2000\nuser_verbatim_tokens = 500\nsummary_output_tokens = 800\n\
             {}",
            tools(&["read", "edit", "shell"])
        ),
    );
    write(root.join("kits/fam/fam.md"), "the fam prompt");
    write(root.join("kits/fam/summarize.md"), "summarize the fam way");
    write(
        root.join("kits/fam/alpha.toml"),
        "[instructions]\nfiles = [\"ALPHA.md\"]\n",
    );
    write(
        environments.join("thin/environment.toml"),
        "route = \"r\"\nmodel = \"alpha\"\n",
    );
    environments
}

#[test]
fn each_table_comes_from_the_model_file_then_the_family_then_base() {
    let root = tempfile::tempdir().unwrap();
    let dirs = [shipped(root.path())];
    let environment = load_environment("thin", &dirs).unwrap();
    // The model's file: instructions.
    assert_eq!(environment.instructions.files, ["ALPHA.md"]);
    // The family's: tools, prompt, context with its summarizer, concurrency.
    assert_eq!(modules("thin", &dirs), ["read", "edit", "shell"]);
    assert_eq!(environment.prompt_template, "the fam prompt");
    let context = environment.context.as_ref().expect("the family's context");
    assert_eq!(context.summarize_at_tokens, 5000);
    assert_eq!(context.window_tokens, None, "the window is the wire's");
    assert_eq!(
        environment.summarize_prompt.as_deref(),
        Some("summarize the fam way")
    );
    assert_eq!(environment.tool_concurrency.max_parallel.get(), 4);
    assert!(!environment.tool_concurrency.shell_reads);
    // Base: options and skills.
    assert_eq!(
        environment.options.reasoning_effort,
        Some(p1_contracts::Effort::High)
    );
    assert_eq!(environment.skills.roots, ["base-skills"]);
    let source = |table: &str| {
        environment
            .sources
            .iter()
            .find(|(name, _)| name == table)
            .map(|(_, file)| file.clone())
    };
    let kit = |relative: &str| Some(root.path().join("environments/../kits").join(relative));
    assert_eq!(source("instructions"), kit("fam/alpha.toml"));
    assert_eq!(source("tools"), kit("fam/kit.toml"));
    assert_eq!(source("prompt"), kit("fam/fam.md"));
    assert_eq!(source("options"), kit("base/kit.toml"));
    assert_eq!(source("capabilities"), None, "a table on its defaults");
}

#[test]
fn an_environment_table_replaces_the_kit_table_whole() {
    let root = tempfile::tempdir().unwrap();
    let environments = shipped(root.path());
    write(
        environments.join("own/environment.toml"),
        &format!(
            "route = \"r\"\nmodel = \"alpha\"\n[tool_concurrency]\nmax_parallel = 2\n{}",
            tools(&["grep"])
        ),
    );
    write(environments.join("own/prompt.md"), "my own prompt");
    let dirs = [environments];
    let environment = load_environment("own", &dirs).unwrap();
    assert_eq!(modules("own", &dirs), ["grep"]);
    assert_eq!(environment.prompt_template, "my own prompt");
    // The kit's `shell_reads = false` is not merged in: the absent key takes its default.
    assert_eq!(environment.tool_concurrency.max_parallel.get(), 2);
    assert!(environment.tool_concurrency.shell_reads);
}

#[test]
fn a_family_without_a_kit_takes_base_and_a_selected_profile_takes_its_own_family() {
    let root = tempfile::tempdir().unwrap();
    let dirs = [shipped(root.path())];
    // `other` is family `elsewhere`, which has no kit: base supplies everything it has.
    let environment = load_environment_for_profile("thin", &dirs, Some("other")).unwrap();
    assert_eq!(environment.family, "elsewhere");
    assert_eq!(environment.prompt_template, "the base prompt");
    assert_eq!(
        environment
            .tools
            .iter()
            .map(|tool| tool.module.as_str())
            .collect::<Vec<_>>(),
        ["read", "shell"]
    );
    assert!(environment.context.is_none());
    assert_eq!(environment.instructions.files, ["AGENTS.md"], "the default");
}

#[test]
fn a_higher_layer_file_wins_whole_and_specificity_beats_layer_priority() {
    let root = tempfile::tempdir().unwrap();
    let shipped_dir = shipped(&root.path().join("shipped"));
    let user = root.path().join("user");
    // The user's family kit replaces the shipped one whole: no tools, so the shipped family
    // kit's tools do not come through; base's do.
    // Its `[instructions]` loses to the shipped model file `alpha.toml`: specificity first.
    write(
        user.join("kits/fam/kit.toml"),
        "prompt_file = \"mine.md\"\n[instructions]\nfiles = [\"NOT.md\"]\n",
    );
    write(user.join("kits/fam/mine.md"), "the user's fam prompt");
    let dirs = [user.join("environments"), shipped_dir];
    let environment = load_environment("thin", &dirs).unwrap();
    assert_eq!(environment.prompt_template, "the user's fam prompt");
    assert_eq!(modules("thin", &dirs), ["read", "shell"]);
    assert!(
        environment.context.is_none(),
        "the shipped family kit is shadowed whole, its [context] with it"
    );
    assert_eq!(environment.instructions.files, ["ALPHA.md"]);
}

#[test]
fn a_kit_names_its_prompt_and_descriptions_relative_to_itself() {
    let root = tempfile::tempdir().unwrap();
    let environments = shipped(root.path());
    write(
        root.path().join("kits/fam/alpha.toml"),
        "[[tools]]\nmodule = \"read\"\ndescription_file = \"read.txt\"\n",
    );
    write(root.path().join("kits/fam/read.txt"), "read, the fam way");
    let environment = load_environment("thin", &[environments]).unwrap();
    assert_eq!(
        environment.tools[0].description.as_deref(),
        Some("read, the fam way")
    );
}

#[test]
fn without_a_prompt_anywhere_the_environment_is_missing_its_prompt() {
    let root = tempfile::tempdir().unwrap();
    let environments = layer(root.path());
    write(
        environments.join("bare/environment.toml"),
        "route = \"r\"\nmodel = \"alpha\"\n",
    );
    let error = load_environment("bare", &[environments]).unwrap_err();
    match error {
        AssemblyError::MissingPrompt { path } => assert!(path.ends_with("bare/prompt.md")),
        other => panic!("expected MissingPrompt, got {other:?}"),
    }
}

#[test]
fn an_unknown_key_in_a_kit_is_an_error_naming_the_file() {
    let root = tempfile::tempdir().unwrap();
    let environments = shipped(root.path());
    write(root.path().join("kits/fam/kit.toml"), "route = \"r\"\n");
    let error = load_environment("thin", &[environments]).unwrap_err();
    let message = error.to_string();
    assert!(message.contains("kits/fam/kit.toml"), "{message}");
    assert!(message.contains("route"), "{message}");
}
