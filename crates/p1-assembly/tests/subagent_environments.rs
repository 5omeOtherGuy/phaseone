//! Owner-confirmed worker models and donor roles, with real shipped configuration
//! and no transport or user directories.
mod common;

use common::{shipped_environments, substitutions};
use p1_assembly::{load_environment, render_prompt};
use p1_contracts::Effort;

#[test]
fn subagent_presets_pin_models_efforts_tools_and_leaf_prompts() {
    for (name, model, effort, tools, role) in [
        (
            "finder",
            "gpt-5.6-terra",
            Some(Effort::Low),
            vec!["read", "grep", "finish"],
            "You are a fast, parallel code search agent.",
        ),
        (
            "librarian",
            "gpt-5.6-sol",
            None,
            vec![
                "read_github",
                "list_directory_github",
                "glob_github",
                "search_github",
                "commit_search",
                "diff_github",
                "list_repositories",
                "finish",
            ],
            "You are Librarian, a specialized repository research worker.",
        ),
        (
            "task",
            "claude-opus-5-5",
            Some(Effort::Medium),
            vec![
                "read",
                "edit",
                "write",
                "grep",
                "shell",
                "shell_job",
                "read_output",
                "finish",
            ],
            "## Task Worker Role",
        ),
    ] {
        let environment = load_environment(name, &[shipped_environments()]).unwrap();
        assert_eq!(environment.model, model);
        assert_eq!(environment.options.reasoning_effort, effort);
        assert!(!environment.capabilities.workers && !environment.capabilities.workflows);
        assert_eq!(
            environment
                .tools
                .iter()
                .map(|tool| tool.module.as_str())
                .collect::<Vec<_>>(),
            tools
        );
        if name == "librarian" {
            assert_eq!(
                environment.options.native["openai-responses.reasoning_enabled"],
                serde_json::json!(false)
            );
        } else {
            assert!(environment.options.native.is_empty());
        }
        let pairs = environment
            .tools
            .iter()
            .map(|tool| (tool.module.clone(), tool.module.clone()))
            .collect::<Vec<_>>();
        let prompt = render_prompt(&environment.prompt_template, &pairs, &substitutions()).unwrap();
        assert!(prompt.contains(role));
        assert!(!prompt.contains("{{"));
        assert!(prompt.contains("finish"));
        if name == "librarian" {
            assert!(prompt.contains("read-only GitHub provider"));
            assert!(prompt.contains("host\npermits only GET requests"));
            assert!(!prompt.contains("gh api"));
            assert!(!prompt.contains("`shell`") && !prompt.contains("`read_output`"));
        }
        // Restricted grants must not leave unresolved tool references in the donor prompt.
        let restricted = render_prompt(
            &environment.prompt_template,
            &[("finish".into(), "report".into())],
            &substitutions(),
        )
        .unwrap();
        assert!(restricted.contains(role));
        assert!(restricted.contains("`report`"));
    }
}
