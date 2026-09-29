//! The embedded TOML filter set: every `data/*.toml` file, parsed and compiled
//! once per process.
//!
//! The guest runs as WebAssembly, so it has no filesystem and no build script:
//! each vendored file is embedded with `include_str!` and the whole registry is
//! compiled in a `OnceLock` on first use. The files are the donor's, unchanged,
//! including their inline `[[tests.<name>]]` cases (see `data/NOTICE.md`).
//!
//! A file that fails to parse is skipped rather than fatal: the seam then finds
//! no filter for that command and yields the raw output, which is the same
//! fail-safe a structured decline gives.

use std::sync::OnceLock;

use super::engine::{self, CompiledFilter};

/// Every vendored filter file, sorted by name: a file name sorts before its
/// own `-` and `.` neighbours, so precedence is deterministic.
const FILES: &[(&str, &str)] = &[
    (
        "ansible-playbook.toml",
        include_str!("data/ansible-playbook.toml"),
    ),
    ("basedpyright.toml", include_str!("data/basedpyright.toml")),
    ("biome.toml", include_str!("data/biome.toml")),
    ("brew-install.toml", include_str!("data/brew-install.toml")),
    (
        "bundle-install.toml",
        include_str!("data/bundle-install.toml"),
    ),
    (
        "composer-install.toml",
        include_str!("data/composer-install.toml"),
    ),
    ("df.toml", include_str!("data/df.toml")),
    ("dotnet-build.toml", include_str!("data/dotnet-build.toml")),
    ("du.toml", include_str!("data/du.toml")),
    (
        "fail2ban-client.toml",
        include_str!("data/fail2ban-client.toml"),
    ),
    ("gcc.toml", include_str!("data/gcc.toml")),
    ("gcloud.toml", include_str!("data/gcloud.toml")),
    ("gradle.toml", include_str!("data/gradle.toml")),
    ("hadolint.toml", include_str!("data/hadolint.toml")),
    ("helm.toml", include_str!("data/helm.toml")),
    ("iptables.toml", include_str!("data/iptables.toml")),
    ("jira.toml", include_str!("data/jira.toml")),
    ("jj.toml", include_str!("data/jj.toml")),
    ("jq.toml", include_str!("data/jq.toml")),
    ("just.toml", include_str!("data/just.toml")),
    ("liquibase.toml", include_str!("data/liquibase.toml")),
    ("make.toml", include_str!("data/make.toml")),
    ("markdownlint.toml", include_str!("data/markdownlint.toml")),
    ("mise.toml", include_str!("data/mise.toml")),
    ("mix-compile.toml", include_str!("data/mix-compile.toml")),
    ("mix-format.toml", include_str!("data/mix-format.toml")),
    ("npm-install.toml", include_str!("data/npm-install.toml")),
    ("nx.toml", include_str!("data/nx.toml")),
    ("ollama.toml", include_str!("data/ollama.toml")),
    ("oxlint.toml", include_str!("data/oxlint.toml")),
    ("ping.toml", include_str!("data/ping.toml")),
    ("pio-run.toml", include_str!("data/pio-run.toml")),
    (
        "poetry-install.toml",
        include_str!("data/poetry-install.toml"),
    ),
    ("pre-commit.toml", include_str!("data/pre-commit.toml")),
    ("ps.toml", include_str!("data/ps.toml")),
    (
        "pulumi-destroy.toml",
        include_str!("data/pulumi-destroy.toml"),
    ),
    (
        "pulumi-preview.toml",
        include_str!("data/pulumi-preview.toml"),
    ),
    (
        "pulumi-refresh.toml",
        include_str!("data/pulumi-refresh.toml"),
    ),
    ("pulumi-stack.toml", include_str!("data/pulumi-stack.toml")),
    ("pulumi-up.toml", include_str!("data/pulumi-up.toml")),
    (
        "quarto-render.toml",
        include_str!("data/quarto-render.toml"),
    ),
    ("rsync.toml", include_str!("data/rsync.toml")),
    ("shellcheck.toml", include_str!("data/shellcheck.toml")),
    (
        "shopify-theme.toml",
        include_str!("data/shopify-theme.toml"),
    ),
    ("skopeo.toml", include_str!("data/skopeo.toml")),
    ("sops.toml", include_str!("data/sops.toml")),
    ("spring-boot.toml", include_str!("data/spring-boot.toml")),
    ("ssh.toml", include_str!("data/ssh.toml")),
    ("stat.toml", include_str!("data/stat.toml")),
    ("swift-build.toml", include_str!("data/swift-build.toml")),
    (
        "systemctl-status.toml",
        include_str!("data/systemctl-status.toml"),
    ),
    ("task.toml", include_str!("data/task.toml")),
    (
        "terraform-plan.toml",
        include_str!("data/terraform-plan.toml"),
    ),
    ("tofu-fmt.toml", include_str!("data/tofu-fmt.toml")),
    ("tofu-init.toml", include_str!("data/tofu-init.toml")),
    ("tofu-plan.toml", include_str!("data/tofu-plan.toml")),
    (
        "tofu-validate.toml",
        include_str!("data/tofu-validate.toml"),
    ),
    ("trunk-build.toml", include_str!("data/trunk-build.toml")),
    ("turbo.toml", include_str!("data/turbo.toml")),
    ("ty.toml", include_str!("data/ty.toml")),
    ("uv-sync.toml", include_str!("data/uv-sync.toml")),
    ("xcodebuild.toml", include_str!("data/xcodebuild.toml")),
    ("yadm.toml", include_str!("data/yadm.toml")),
    ("yamllint.toml", include_str!("data/yamllint.toml")),
];

/// The compiled registry: every filter of every file, in file-name order.
pub(super) fn registry() -> &'static [CompiledFilter] {
    static REGISTRY: OnceLock<Vec<CompiledFilter>> = OnceLock::new();
    REGISTRY.get_or_init(|| {
        let mut all = Vec::new();
        for (name, content) in FILES {
            match engine::parse_and_compile(content, name) {
                Ok(filters) => all.extend(filters),
                // Fail-safe: one unparsable file costs its own filters only.
                Err(_) => continue,
            }
        }
        all
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every vendored file must be embedded: a file added to `data/` but not to
    /// `FILES` would be silently dead weight.
    #[test]
    fn all_64_data_files_are_embedded() {
        assert_eq!(FILES.len(), 64, "the vendored filter set must be complete");
    }

    /// Every embedded file must parse with schema version 1.
    #[test]
    fn every_embedded_file_parses() {
        for (name, content) in FILES {
            engine::parse_file(content).unwrap_or_else(|e| panic!("{name} must parse: {e}"));
        }
    }

    /// No vendored filter definition may fail to compile: a compile drop is
    /// silent at runtime, so this is the only proof it does not happen.
    #[test]
    fn every_embedded_filter_compiles() {
        let mut total = 0usize;
        for (name, content) in FILES {
            let parsed = engine::parse_file(content).expect("file parses");
            let compiled =
                engine::parse_and_compile(content, name).unwrap_or_else(|e| panic!("{name}: {e}"));
            assert_eq!(
                compiled.len(),
                parsed.filter_count(),
                "{name}: a vendored filter definition failed to compile"
            );
            total += compiled.len();
        }
        assert!(
            total >= 64,
            "expected at least one filter per file, got {total}"
        );
    }

    /// The one corpus test: every `[[tests.<name>]]` case of every file runs
    /// against its own filter and must match.
    #[test]
    fn every_inline_test_case_passes() {
        let mut failures = Vec::new();
        let mut cases = 0usize;
        for (name, content) in FILES {
            let file = engine::parse_file(content).expect("file parses");
            let filters = engine::parse_and_compile(content, name).expect("file compiles");
            for f in &filters {
                let Some(tests) = file.tests.get(&f.name) else {
                    failures.push(format!("{name}: filter '{}' has no inline tests", f.name));
                    continue;
                };
                assert!(
                    !tests.is_empty(),
                    "{name}: filter '{}' has an empty test list",
                    f.name
                );
                for t in tests {
                    cases += 1;
                    let actual = engine::apply_filter(f, &t.input, true);
                    // TOML multiline strings carry a trailing newline; compare
                    // trimmed, as the donor's runner does.
                    if actual.trim_end_matches('\n') != t.expected.trim_end_matches('\n') {
                        failures.push(format!(
                            "[{}] {}\n--- expected ---\n{}\n--- actual ---\n{}",
                            f.name,
                            t.name,
                            t.expected.trim_end_matches('\n'),
                            actual.trim_end_matches('\n'),
                        ));
                    }
                }
            }
            // Typo guard: a test section must not name a filter that is gone.
            let compiled_names: std::collections::HashSet<_> =
                filters.iter().map(|f| f.name.as_str()).collect();
            for key in file.tests.keys() {
                assert!(
                    compiled_names.contains(key.as_str()),
                    "{name}: [[tests.{key}]] references an unknown filter"
                );
            }
        }
        assert!(
            cases > 100,
            "expected a substantial inline corpus, ran {cases}"
        );
        assert!(
            failures.is_empty(),
            "{} inline test failure(s):\n\n{}",
            failures.len(),
            failures.join("\n\n")
        );
    }

    /// The registry is compiled once and every vendored filter is in it.
    #[test]
    fn the_registry_holds_every_embedded_filter() {
        let registry = registry();
        assert!(registry.len() >= 64, "registry holds {}", registry.len());
        let from_files: usize = FILES
            .iter()
            .map(|(_, content)| {
                engine::parse_file(content)
                    .expect("file parses")
                    .filter_count()
            })
            .sum();
        assert_eq!(registry.len(), from_files);
    }

    /// A representative filter from the vendored set is found by its command.
    #[test]
    fn a_vendored_filter_matches_its_command() {
        let f = registry()
            .iter()
            .find(|f| f.matches("shellcheck deploy.sh"))
            .expect("the vendored shellcheck filter matches");
        assert_eq!(f.name, "shellcheck");
        assert!(
            !registry()
                .iter()
                .any(|f| f.matches("some-unknown-tool --flag"))
        );
    }
}
