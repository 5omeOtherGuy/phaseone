//! Pluggable Finder agent: Terra/low, with a fixed read/search grant.
#![forbid(unsafe_code)]

mod agent;
use agent::Subagent;

const NAME: &str = "finder";
const ENVIRONMENT: &str = "finder";
const TOOLS: &[&str] = &["read", "grep"];
const DESCRIPTION: &str = "Find code by behavior or correlate implementations across the local workspace. Uses GPT-5.6 Terra at low effort with the ampi Finder prompt. Supply one precise query and explicit success criteria. Use direct read or grep for a known path or exact symbol. Returns verified file paths and line ranges, not an implementation. The call waits for the worker's result; no conversation history is passed.";

p1_bindings_tool::generated::export!(Subagent);
