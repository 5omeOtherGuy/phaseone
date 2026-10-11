//! Temporary catalog trees for behavior tests. No shipped catalog data is read.

use std::path::{Path, PathBuf};

/// The sibling layout the host and assembly loader search. The directory lives until
/// the fixture is dropped; callers can edit individual files to exercise refusals.
pub struct CatalogFixture {
    pub root: tempfile::TempDir,
}

impl Default for CatalogFixture {
    fn default() -> Self {
        Self::new()
    }
}

impl CatalogFixture {
    pub fn new() -> Self {
        let root = tempfile::tempdir().unwrap();
        for directory in ["environments", "routes", "accounts", "profiles"] {
            std::fs::create_dir_all(root.path().join(directory)).unwrap();
        }
        Self { root }
    }

    pub fn environment_dirs(&self) -> Vec<PathBuf> {
        vec![self.root.path().join("environments")]
    }

    /// Write catalog data, including optional `modules.lock` and `subagents.toml`.
    pub fn write(&self, relative: &str, text: &str) {
        write(&self.root.path().join(relative), text);
    }

    pub fn write_profile(&self, id: &str, text: &str) {
        self.write(&format!("profiles/{id}.toml"), text);
    }

    pub fn write_route(&self, id: &str, text: &str) {
        write(&self.route_path(id), text);
    }

    /// Where [`write_route`](Self::write_route) put route `id`: the file the host's
    /// `load_route` reads.
    pub fn route_path(&self, id: &str) -> PathBuf {
        self.root.path().join(format!("routes/{id}.toml"))
    }

    pub fn write_account(&self, id: &str, text: &str) {
        self.write(&format!("accounts/{id}.toml"), text);
    }

    pub fn write_environment(&self, name: &str, text: &str, prompt: &str) {
        self.write(&format!("environments/{name}/environment.toml"), text);
        self.write(&format!("environments/{name}/prompt.md"), prompt);
    }

    /// One synthetic routed environment. IDs are supplied by the test, not taken
    /// from the shipped pack (replay tests may deliberately retain recorded IDs).
    pub fn adapter(&self, name: &str, adapter: &str, route: &str, profile: &str) {
        self.write_route(route, &route_toml(adapter, route, profile));
        self.write_profile(profile, &profile_toml(adapter, profile));
        self.write_environment(
            name,
            &format!("route = \"{route}\"\nprofile = \"{profile}\"\n"),
            "Synthetic {{tool_names}}",
        );
    }
}

/// Promote the whole-provider environment writer used by the host harness.
pub fn write_environment(
    environments: &Path,
    name: &str,
    provider: &str,
    model: &str,
    tools: &[&str],
    prompt: &str,
) {
    let mut text = format!("family = \"{name}\"\nprovider = \"{provider}\"\nmodel = \"{model}\"\n");
    for tool in tools {
        text.push_str(&format!("[[tools]]\nmodule = \"{tool}\"\n"));
    }
    write(&environments.join(name).join("environment.toml"), &text);
    write(&environments.join(name).join("prompt.md"), prompt);
}

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

/// A profile appropriate for one adapter; tests add capacities and modalities
/// explicitly when those properties are the subject of an assertion.
pub fn profile_toml(adapter: &str, id: &str) -> String {
    let thinking = match adapter {
        "anthropic-messages" | "openai-responses" => "effort-level",
        "openai-chat" => "preserved",
        _ => panic!("unknown fixture adapter {adapter}"),
    };
    let efforts = if adapter == "openai-chat" {
        "[\"low\", \"medium\", \"high\", \"max\"]"
    } else {
        "[\"low\", \"medium\", \"high\", \"extra_high\", \"max\"]"
    };
    let default = if adapter == "openai-chat" {
        "default_effort = \"high\"\n"
    } else {
        ""
    };
    format!(
        "id = \"{id}\"\nrevision = 1\nmodel_id = \"{id}\"\nfamily = \"test\"\nthinking = \"{thinking}\"\nefforts = {efforts}\n{default}"
    )
}

/// Subscription dialects and a retained-thinking Chat route, all with synthetic
/// origins. Borrowing tests explicitly replace the endpoint with the trusted CLI
/// origin; no fixture opens a connection.
pub fn route_toml(adapter: &str, id: &str, profile: &str) -> String {
    let (kind, settings) = match adapter {
        "anthropic-messages" => (
            "claude-code-oauth",
            "dialect = \"claude-code-subscription\"\nlong_context = true\n",
        ),
        "openai-responses" => (
            "codex-oauth",
            "dialect = \"codex-subscription\"\ntransport = \"websocket\"\n",
        ),
        "openai-chat" => ("api-key", "dialect = \"retained-thinking\"\n"),
        _ => panic!("unknown fixture adapter {adapter}"),
    };
    let env = if adapter == "openai-chat" {
        "env = \"TEST_KEY\"\n"
    } else {
        ""
    };
    format!(
        "id = \"{id}\"\norigin_route = \"{adapter}/{id}\"\nadapter = \"{adapter}\"\nendpoint = \"https://example.invalid/v1\"\n[credential]\nkind = \"{kind}\"\nstore_only = true\n{env}[adapter_settings]\n{settings}[models.\"{profile}\"]\nwire_model = \"{profile}\"\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_files_are_siblings_and_live_until_the_fixture_is_dropped() {
        let root;
        {
            let fixture = CatalogFixture::new();
            root = fixture.root.path().to_path_buf();
            fixture.adapter("messages", "anthropic-messages", "messages", "effort");
            fixture.adapter("responses", "openai-responses", "responses", "effort");
            fixture.adapter("chat", "openai-chat", "chat", "thinking");
            fixture.write_account("example", "id = 'example'\n");
            fixture.write("modules.lock", "version = 1\n");
            fixture.write("subagents.toml", "[finder]\n");
            assert_eq!(fixture.environment_dirs(), vec![root.join("environments")]);
            for name in ["messages", "responses", "chat"] {
                assert!(
                    root.join(format!("environments/{name}/environment.toml"))
                        .is_file()
                );
                assert!(
                    root.join(format!("environments/{name}/prompt.md"))
                        .is_file()
                );
                assert!(root.join(format!("routes/{name}.toml")).is_file());
            }
            assert!(root.join("profiles/effort.toml").is_file());
            assert!(root.join("profiles/thinking.toml").is_file());
            assert!(root.join("accounts/example.toml").is_file());
            assert!(root.join("modules.lock").is_file());
            assert!(root.join("subagents.toml").is_file());
        }
        assert!(!root.exists());
    }
}
