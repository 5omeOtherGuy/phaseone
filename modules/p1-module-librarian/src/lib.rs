//! Pluggable Librarian agent: Sol/reasoning off, remote repository research.
#![forbid(unsafe_code)]

#[path = "../../p1-module-finder/src/agent.rs"]
mod agent;
use agent::Subagent;

const NAME: &str = "librarian";
const ENVIRONMENT: &str = "librarian";
const TOOLS: &[&str] = &["shell", "read_output"];
const DESCRIPTION: &str = "Research remote GitHub repositories, architecture, dependencies and commit history. Uses GPT-5.6 Sol with reasoning off and the adapted ampi Librarian prompt. Name the repository and a specific question; optionally supply context. Not for local workspace inspection. Requires an available GitHub CLI with repository access. Read-only behavior is instructed by the prompt, not enforced by its shell grant. The call waits and returns the complete evidence-backed answer; no conversation history is passed.";

p1_bindings_tool::generated::export!(Subagent);
