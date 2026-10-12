//! Lead acceptance: prompt/tool coherence of the SHIPPED environments against the
//! GLOBAL set of tool modules (the strict reading of docs/design/assembly.md): a prompt
//! never puts a tool in backticks — by placeholder or by literal name — that its
//! environment does not assemble, and mentions every tool it does assemble.
//!
//! ADR-0050 item 4 adds conditional sections, so this test also RENDERS each shipped
//! prompt for EVERY subset of that environment's tools and checks that the render
//! succeeds and that the rendered text names no tool outside the subset. The full tool
//! set is one of those subsets, so the strict check above is kept for it; the sweep is
//! what a parent granting a worker a subset needs. `finish` is benched (ADR-0193): no
//! shipped environment assembles it, so no render may name it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use p1_assembly::{EnvironmentFile, Substitutions, load_environment, render_prompt};

const ALL_TOOL_MODULES: [&str; 11] = [
    "read",
    "edit",
    "write",
    "grep",
    "shell",
    "apply_patch",
    "finish",
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];

/// The worker tools explicitly listed by general-purpose shipped environments.
const WORKER_MODULES: [&str; 4] = [
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];

fn shipped() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")
}

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
        scratch: String::new(),
    }
}

fn backticked(text: &str) -> BTreeSet<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

/// The environment's tools as `(module, model-facing name)`, in file order.
fn modules(environment: &EnvironmentFile) -> Vec<(String, String)> {
    environment
        .tools
        .iter()
        .map(|spec| {
            (
                spec.module.clone(),
                spec.name.clone().unwrap_or_else(|| spec.module.clone()),
            )
        })
        .collect()
}

/// The name `module` would be assembled under in this environment, if it has it.
fn face_of<'a>(environment: &'a EnvironmentFile, module: &'a str) -> &'a str {
    environment
        .tools
        .iter()
        .find(|spec| spec.module == module)
        .and_then(|spec| spec.name.as_deref())
        .unwrap_or(module)
}

/// Every subset of `assembled`, in environment-file order.
fn subsets(
    assembled: &[(String, String)],
) -> impl ExactSizeIterator<Item = Vec<(String, String)>> + Clone + '_ {
    let optional = assembled.len();
    assert!(
        optional <= 20,
        "enumerating 2^{optional} subsets is too much for a prompt test"
    );
    (0..1u32 << optional).map(move |mask| {
        assembled
            .iter()
            .enumerate()
            .filter(|(bit, _)| mask & (1 << bit) != 0)
            .map(|(_, entry)| entry.clone())
            .collect()
    })
}

fn render(name: &str, environment: &EnvironmentFile, subset: &[(String, String)]) -> String {
    let listed: Vec<&str> = subset.iter().map(|(module, _)| module.as_str()).collect();
    render_prompt(&environment.prompt_template, subset, &substitutions())
        .unwrap_or_else(|error| panic!("{name}: prompt does not render for {listed:?}: {error}"))
}

/// (a) rendered without error and (b) names no tool the subset does not hold.
fn assert_renders_coherently(
    name: &str,
    environment: &EnvironmentFile,
    subset: &[(String, String)],
    rendered: &str,
) {
    assert!(
        !rendered.contains("{{"),
        "{name}: the render for {:?} still contains a placeholder",
        subset
            .iter()
            .map(|(module, _)| module.as_str())
            .collect::<Vec<_>>()
    );
    for module in ALL_TOOL_MODULES {
        if subset.iter().any(|(assembled, _)| assembled == module) {
            continue;
        }
        for mention in [module, face_of(environment, module)] {
            assert!(
                !rendered.contains(&format!("`{mention}`")),
                "{name}: the render for {:?} mentions `{mention}`, which the subset does not have",
                subset
                    .iter()
                    .map(|(module, _)| module.as_str())
                    .collect::<Vec<_>>()
            );
        }
    }
}

fn check(name: &str, dirs: &[PathBuf]) {
    let environment = load_environment(name, dirs).expect("shipped environment loads");
    let assembled = modules(&environment);
    let ticks = backticked(&environment.prompt_template);

    for module in ALL_TOOL_MODULES {
        let literal = ticks.contains(module);
        let placeholder = ticks.contains(&format!("{{{{tool:{module}}}}}"));
        if assembled.iter().any(|(held, _)| held == module) {
            assert!(
                placeholder,
                "{name}: prompt never mentions its tool `{module}` by placeholder"
            );
            assert!(
                !literal,
                "{name}: prompt hard-codes `{module}`; use {{{{tool:{module}}}}}"
            );
        } else {
            // A module this environment does not assemble may appear in the template
            // only inside its own conditional section, which the render drops; a
            // literal name never.
            assert!(
                !literal,
                "{name}: prompt hard-codes `{module}`, which it does not have"
            );
        }
    }

    assert!(
        assembled.iter().all(|(module, _)| module != "finish"),
        "{name}: `finish` is benched (ADR-0193); no shipped environment assembles it"
    );
    let subsets = subsets(&assembled);
    assert_eq!(
        subsets.len(),
        1usize << assembled.len(),
        "{name}: every subset of its tools is rendered"
    );
    assert!(
        subsets.clone().any(|subset| subset == assembled),
        "{name}: the full tool set is one of the rendered subsets"
    );
    for subset in subsets {
        let rendered = render(name, &environment, &subset);
        assert_renders_coherently(name, &environment, &subset, &rendered);
        assert!(
            !rendered.contains("# Finishing"),
            "{name}: a Finishing section is rendered without the finish tool for {subset:?}"
        );
    }

    // A leaf environment (ADR-0124, `[capabilities] workers = false`) gets no worker
    // tools as a main agent either, so its full render carries no Workers section.
    if !environment.capabilities.workers {
        let rendered = render(name, &environment, &assembled);
        assert!(
            !rendered.contains("# Workers"),
            "{name}: a leaf environment's render carries a Workers section"
        );
        return;
    }
    // The main-agent case: general-purpose environments explicitly list the worker tools.
    let rendered = render(name, &environment, &assembled);
    for module in WORKER_MODULES {
        assert!(
            rendered.contains(&format!("`{module}`")),
            "{name}: the main-agent render does not name the worker tool `{module}`"
        );
    }
    assert!(
        rendered.contains("# Workers (optional)"),
        "{name}: the main-agent render does not carry the Workers section"
    );
}

/// The environment names under `dir`, sorted.
fn names(dir: &Path) -> Vec<String> {
    let mut names: Vec<String> = std::fs::read_dir(dir)
        .expect("the environments directory is readable")
        .map(|entry| entry.expect("a readable entry").path())
        .filter(|path| path.join("environment.toml").is_file())
        .map(|path| {
            path.file_name()
                .expect("a directory name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    names.sort();
    names
}

/// The sweep runs for every directory under `environments/`, so a new shipped prompt
/// cannot slip past the check by not being named here.
#[test]
fn every_shipped_prompt_renders_for_every_subset_of_its_tools() {
    let checked = names(&shipped());
    assert_eq!(
        checked,
        ["default"],
        "a shipped environment was added or removed without this test being updated"
    );
    for name in &checked {
        check(name, &[shipped()]);
    }
}

/// The owner pack's prompts (#853), layered over the shipped catalog as the owner's settings
/// select it. The pack's own check renders them through `p1 env show` with their full tool
/// sets (`packs/owner/tests/test_pack.py`); only this sweep renders every subset a parent may
/// grant a worker, which the binary has no command for.
#[test]
fn every_owner_pack_prompt_renders_for_every_subset_of_its_tools() {
    let pack = shipped().join("../packs/owner/environments");
    let checked = names(&pack);
    assert_eq!(checked.len(), 21, "{checked:?}");
    for name in &checked {
        check(name, &[pack.clone(), shipped()]);
    }
}
