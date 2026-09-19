//! Lead acceptance: prompt/tool coherence of the SHIPPED environments against the
//! GLOBAL set of tool modules (the strict reading of docs/design/assembly.md): a prompt
//! never puts a tool in backticks — by placeholder or by literal name — that its
//! environment does not assemble, and mentions every tool it does assemble.

use std::collections::BTreeSet;
use std::path::PathBuf;

use p1_assembly::load_environment;

const ALL_TOOL_MODULES: [&str; 10] = [
    "read",
    "edit",
    "write",
    "grep",
    "shell",
    "apply_patch",
    "worker_start",
    "worker_result",
    "worker_continue",
    "worker_cancel",
];

fn shipped() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../environments")
}

fn backticked(text: &str) -> BTreeSet<String> {
    text.split('`')
        .skip(1)
        .step_by(2)
        .map(str::to_string)
        .collect()
}

fn check(name: &str) {
    let environment = load_environment(name, &[shipped()]).expect("shipped environment loads");
    let assembled: BTreeSet<&str> = environment
        .tools
        .iter()
        .map(|t| t.module.as_str())
        .collect();
    let ticks = backticked(&environment.prompt_template);

    for module in ALL_TOOL_MODULES {
        let literal = ticks.contains(module);
        let placeholder = ticks.contains(&format!("{{{{tool:{module}}}}}"));
        if assembled.contains(module) {
            assert!(
                placeholder,
                "{name}: prompt never mentions its tool `{module}` by placeholder"
            );
            assert!(
                !literal,
                "{name}: prompt hard-codes `{module}`; use {{{{tool:{module}}}}}"
            );
        } else {
            assert!(
                !literal && !placeholder,
                "{name}: prompt mentions `{module}`, which it does not have"
            );
        }
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
