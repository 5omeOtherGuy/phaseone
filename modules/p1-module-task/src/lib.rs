//! Pluggable Task agent: Opus 5.5/medium, bounded implementation and verification.
#![forbid(unsafe_code)]

#[path = "../../p1-module-finder/src/agent.rs"]
mod agent;
use agent::Subagent;

const NAME: &str = "Task";
const ENVIRONMENT: &str = "task";
const TOOLS: &[&str] = &[
    "read",
    "edit",
    "write",
    "grep",
    "shell",
    "shell_job",
    "read_output",
];
const DESCRIPTION: &str = "Perform one bounded implementation, investigation or verification task using Opus 5.5 at medium effort and the ampi Task worker role. Include the goal, scope, context, constraints, validation and expected result in prompt, plus a short description. The worker shares the workspace: assign disjoint files and verify its result before integrating. Do small reads, exact searches and localized edits yourself. The call waits for the result; no conversation history is passed and nested delegation is unavailable.";

p1_bindings_tool::generated::export!(Subagent);
