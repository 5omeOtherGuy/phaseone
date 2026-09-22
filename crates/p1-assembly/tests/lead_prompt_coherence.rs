//! Lead acceptance: prompt/tool coherence of the SHIPPED environments against the
//! GLOBAL set of tool modules (the strict reading of docs/design/assembly.md): a prompt
//! never puts a tool in backticks — by placeholder or by literal name — that its
//! environment does not assemble, and mentions every tool it does assemble.
//!
//! ADR-0050 item 4 adds conditional sections, so this test also RENDERS each shipped
//! prompt for EVERY subset of that environment's tools that contains `finish` (the
//! module every environment assembles) and checks that the render succeeds and that
//! the rendered text names no tool outside the subset. The full tool set is one of
//! those subsets, so the strict check above is kept for it; the sweep is what a
//! parent granting a worker a subset needs.

use std::collections::BTreeSet;
use std::path::PathBuf;

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

fn shipped() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")
}

fn substitutions() -> Substitutions {
    Substitutions {
        workspace: "/work".into(),
        date: "2026-01-01".into(),
        os: "linux".into(),
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

/// Every subset of `assembled` that contains `finish`, in environment-file order.
fn subsets(assembled: &[(String, String)]) -> Vec<Vec<(String, String)>> {
    let optional = assembled
        .iter()
        .filter(|(module, _)| module != "finish")
        .count();
    assert!(
        optional <= 16,
        "enumerating 2^{optional} subsets is too much for a prompt test"
    );
    (0..1u32 << optional)
        .map(|mask| {
            let mut subset = Vec::new();
            let mut bit = 0;
            for entry in assembled {
                let wanted = entry.0 == "finish" || mask & (1 << bit) != 0;
                if entry.0 != "finish" {
                    bit += 1;
                }
                if wanted {
                    subset.push(entry.clone());
                }
            }
            subset
        })
        .collect()
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

fn check(name: &str) {
    let environment = load_environment(name, &[shipped()]).expect("shipped environment loads");
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
        assembled.iter().any(|(module, _)| module == "finish"),
        "{name}: every shipped environment assembles `finish`"
    );
    let optional = assembled
        .iter()
        .filter(|(module, _)| module != "finish")
        .count();
    let subsets = subsets(&assembled);
    assert_eq!(
        subsets.len(),
        1usize << optional,
        "{name}: every subset of its tools that contains `finish` is rendered"
    );
    assert!(
        subsets.iter().any(|subset| subset == &assembled),
        "{name}: the full tool set is one of the rendered subsets"
    );
    for subset in &subsets {
        let rendered = render(name, &environment, subset);
        assert_renders_coherently(name, &environment, subset, &rendered);
    }
}

#[test]
fn the_claude_prompt_mentions_exactly_its_own_tools() {
    check("claude");
}

#[test]
fn the_gpt_prompt_mentions_exactly_its_own_tools() {
    check("gpt");
}

#[test]
fn the_delegating_claude_prompt_mentions_exactly_its_own_tools() {
    check("claude-delegating");
}

#[test]
fn the_subscription_prompts_mention_exactly_their_own_tools() {
    check("deepseek");
    check("glm");
}

#[test]
fn the_second_deepseek_prompt_mentions_exactly_its_own_tools() {
    check("deepseek2");
}

/// The sweep runs for every directory under `environments/`, so a new shipped prompt
/// cannot slip past the check by not being named above.
#[test]
fn every_shipped_prompt_renders_for_every_subset_of_its_tools() {
    let mut checked: Vec<String> = std::fs::read_dir(shipped())
        .expect("the shipped environments directory is readable")
        .map(|entry| entry.expect("a readable entry").path())
        .filter(|path| path.join("environment.toml").is_file())
        .map(|path| {
            path.file_name()
                .expect("a directory name")
                .to_string_lossy()
                .into_owned()
        })
        .collect();
    checked.sort();
    assert_eq!(
        checked,
        [
            "claude",
            "claude-delegating",
            "deepseek",
            "deepseek2",
            "glm",
            "gpt"
        ],
        "a shipped environment was added or removed without this test being updated"
    );
    for name in &checked {
        check(name);
    }
}
