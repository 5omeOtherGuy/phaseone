//! Pluggable Librarian agent: Sol/reasoning off, remote repository research.
#![forbid(unsafe_code)]

#[path = "../../p1-module-finder/src/agent.rs"]
mod agent;
use agent::Subagent;

const NAME: &str = "librarian";
const ENVIRONMENT: &str = "librarian";
const TOOLS: &[&str] = &[
    "read_github",
    "list_directory_github",
    "glob_github",
    "search_github",
    "commit_search",
    "diff_github",
    "list_repositories",
];
const DESCRIPTION: &str = "Research remote GitHub repositories, architecture, dependencies and commit history. Uses GPT-5.6 Sol with reasoning off and the adapted ampi Librarian prompt. Name the repository and a specific question; optionally supply context. Uses seven read-only GitHub components over a host-enforced GET-only capability, without shell or local workspace access. Private repositories and code search require a suitable host-held GitHub token. The call waits and returns the complete evidence-backed answer; no conversation history is passed.";

p1_bindings_tool::generated::export!(Subagent);
