//! The embedded declarative filter set: every `data/*.json` file, parsed and
//! compiled once per process.
//!
//! The guest runs as WebAssembly, so it has no filesystem and no build script:
//! each file is embedded with `include_str!` and the whole registry is compiled
//! in a `OnceLock` on first use. The files are one-off JSON conversions of the
//! donor's TOML files (the guest may depend only on serde, serde_json and
//! regex: ADR-0081), inline `tests` cases included. `npm-install` and
//! `shellcheck` are left out: the frozen filter corpus requires those classes
//! to pass through raw (see `data/NOTICE.md`).
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
        "ansible-playbook.json",
        include_str!("data/ansible-playbook.json"),
    ),
    ("basedpyright.json", include_str!("data/basedpyright.json")),
    ("biome.json", include_str!("data/biome.json")),
    ("brew-install.json", include_str!("data/brew-install.json")),
    (
        "bundle-install.json",
        include_str!("data/bundle-install.json"),
    ),
    (
        "composer-install.json",
        include_str!("data/composer-install.json"),
    ),
    ("df.json", include_str!("data/df.json")),
    ("dotnet-build.json", include_str!("data/dotnet-build.json")),
    ("du.json", include_str!("data/du.json")),
    (
        "fail2ban-client.json",
        include_str!("data/fail2ban-client.json"),
    ),
    ("gcc.json", include_str!("data/gcc.json")),
    ("gcloud.json", include_str!("data/gcloud.json")),
    ("gradle.json", include_str!("data/gradle.json")),
    ("hadolint.json", include_str!("data/hadolint.json")),
    ("helm.json", include_str!("data/helm.json")),
    ("iptables.json", include_str!("data/iptables.json")),
    ("jira.json", include_str!("data/jira.json")),
    ("jj.json", include_str!("data/jj.json")),
    ("jq.json", include_str!("data/jq.json")),
    ("just.json", include_str!("data/just.json")),
    ("liquibase.json", include_str!("data/liquibase.json")),
    ("make.json", include_str!("data/make.json")),
    ("markdownlint.json", include_str!("data/markdownlint.json")),
    ("mise.json", include_str!("data/mise.json")),
    ("mix-compile.json", include_str!("data/mix-compile.json")),
    ("mix-format.json", include_str!("data/mix-format.json")),
    ("nx.json", include_str!("data/nx.json")),
    ("ollama.json", include_str!("data/ollama.json")),
    ("oxlint.json", include_str!("data/oxlint.json")),
    ("ping.json", include_str!("data/ping.json")),
    ("pio-run.json", include_str!("data/pio-run.json")),
    (
        "poetry-install.json",
        include_str!("data/poetry-install.json"),
    ),
    ("pre-commit.json", include_str!("data/pre-commit.json")),
    ("ps.json", include_str!("data/ps.json")),
    (
        "pulumi-destroy.json",
        include_str!("data/pulumi-destroy.json"),
    ),
    (
        "pulumi-preview.json",
        include_str!("data/pulumi-preview.json"),
    ),
    (
        "pulumi-refresh.json",
        include_str!("data/pulumi-refresh.json"),
    ),
    ("pulumi-stack.json", include_str!("data/pulumi-stack.json")),
    ("pulumi-up.json", include_str!("data/pulumi-up.json")),
    (
        "quarto-render.json",
        include_str!("data/quarto-render.json"),
    ),
    ("rsync.json", include_str!("data/rsync.json")),
    (
        "shopify-theme.json",
        include_str!("data/shopify-theme.json"),
    ),
    ("skopeo.json", include_str!("data/skopeo.json")),
    ("sops.json", include_str!("data/sops.json")),
    ("spring-boot.json", include_str!("data/spring-boot.json")),
    ("ssh.json", include_str!("data/ssh.json")),
    ("stat.json", include_str!("data/stat.json")),
    ("swift-build.json", include_str!("data/swift-build.json")),
    (
        "systemctl-status.json",
        include_str!("data/systemctl-status.json"),
    ),
    ("task.json", include_str!("data/task.json")),
    (
        "terraform-plan.json",
        include_str!("data/terraform-plan.json"),
    ),
    ("tofu-fmt.json", include_str!("data/tofu-fmt.json")),
    ("tofu-init.json", include_str!("data/tofu-init.json")),
    ("tofu-plan.json", include_str!("data/tofu-plan.json")),
    (
        "tofu-validate.json",
        include_str!("data/tofu-validate.json"),
    ),
    ("trunk-build.json", include_str!("data/trunk-build.json")),
    ("turbo.json", include_str!("data/turbo.json")),
    ("ty.json", include_str!("data/ty.json")),
    ("uv-sync.json", include_str!("data/uv-sync.json")),
    ("xcodebuild.json", include_str!("data/xcodebuild.json")),
    ("yadm.json", include_str!("data/yadm.json")),
    ("yamllint.json", include_str!("data/yamllint.json")),
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
    /// `FILES` would be silently dead weight. 64 donor files minus the two the
    /// frozen corpus keeps raw.
    #[test]
    fn all_62_data_files_are_embedded() {
        assert_eq!(FILES.len(), 62, "the vendored filter set must be complete");
    }

    /// The classes the frozen corpus (`tests/output_filters.rs`) requires to
    /// pass through untouched have no filter.
    #[test]
    fn corpus_passthrough_classes_have_no_filter() {
        for command in [
            "npm install gulp@4 request@2 bower@1",
            "shellcheck /tmp/fixgen/sh/deploy.sh",
            "git log --oneline -n 12",
        ] {
            assert!(!registry().iter().any(|f| f.matches(command)), "{command}");
        }
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
            total >= 62,
            "expected at least one filter per file, got {total}"
        );
    }

    /// The one corpus test: every inline `tests.<name>` case of every file runs
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
                    // The donor's TOML multiline strings carried a trailing newline;
                    // compare trimmed, as the donor's runner does.
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
        assert!(registry.len() >= 62, "registry holds {}", registry.len());
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
            .find(|f| f.matches("helm upgrade app ./chart"))
            .expect("the vendored helm filter matches");
        assert_eq!(f.name, "helm");
        assert!(
            !registry()
                .iter()
                .any(|f| f.matches("some-unknown-tool --flag"))
        );
    }
}
