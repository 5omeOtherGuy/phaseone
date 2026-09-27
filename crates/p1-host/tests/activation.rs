//! S3.8 (ADR-0083, D083b): the host assembles `shell` and `finish` BY NAME as the
//! components `p1/shell` (`modules/p1-module-shell/`) and `p1/finish`
//! (`modules/p1-module-finish/`). Each is an official-release HOST ENTRY: it loads its
//! package from the release manifest through the shared host-entry step, with no
//! `modules.lock` entry, so what the model sees is the package's own name, description and
//! variant — the sandbox paragraph and the `+sandbox` variant stay the host's
//! (`tests/sandbox.rs`).
//!
//! A granted import without its service fails assembly (`MissingService`), never runs
//! unlinked, and a release that does not carry the package fails naming the module: there
//! is no native fallback left to fall back to. Those two cases drive the loading step the
//! host entries share and live with it (`catalog/tools.rs`, `catalog/modules.rs`).
//!
//! The cases need the packages `scripts/build-modules.sh` publishes; a missing artifact
//! (or a missing release manifest) fails with the loader's own message, never a skip.

mod common;

use std::sync::Arc;

use common::{Harness, isolated_environment, provider_hook, run_args, write_environment};
use p1_contracts::Tool;
use p1_testkit::{ScriptedProvider, text_response};
use tempfile::tempdir;

/// The native `finish` tool's session record, empty: only its declaration is compared.
struct NoActivity;

impl p1_tool_finish::SessionActivity for NoActivity {
    fn last_file_change(&self) -> Option<u64> {
        None
    }

    fn shell_runs(&self) -> Vec<p1_finish_guest::ShellRun> {
        Vec::new()
    }
}

/// The two packages S3.8 activates, with the variant their manifests declare (the
/// environment below names no variant, so the loader's own stands).
const PACKAGES: [(&str, &str); 2] = [("shell", "p1/shell"), ("finish", "p1/finish")];

/// (a) An environment that names `shell` and `finish` assembles the two components, and
/// `env show` prints the loader-built identity of each: the implementation is the release
/// manifest's package name, never the native adapter's crate name.
#[tokio::test]
async fn the_shell_and_finish_assemble_as_their_packages() {
    let environments = tempdir().unwrap();
    let tools = PACKAGES.map(|(module, _)| module);
    write_environment(
        environments.path(),
        "act",
        "fake",
        "fake-model",
        &tools,
        "test",
    );
    let mut harness = Harness::new(vec![environments.path().to_path_buf()], &[]);
    harness.deps.catalog_hook = Some(provider_hook(vec![(
        "fake",
        ScriptedProvider::new(vec![text_response("never reached")]),
    )]));
    isolated_environment(&mut harness);

    let code = run_args(&mut harness, &["env", "show", "act"]).await;
    assert_eq!(code, 0, "stderr: {}", harness.stderr.text());
    let resolved = common::env_show_json(&harness.stdout.text());
    let tools = resolved["tools"]
        .as_array()
        .unwrap_or_else(|| panic!("env show prints the tools: {resolved}"));
    for (module, package) in PACKAGES {
        let tool = tools
            .iter()
            .find(|tool| tool["module"] == module)
            .unwrap_or_else(|| panic!("no `{module}` tool in {resolved}"));
        assert_eq!(
            tool["identity"]["implementation"], package,
            "the identity is the release manifest's, built by the loader: {tool}"
        );
        assert_eq!(
            tool["identity"]["variant"], "claude",
            "the loader's variant, unless the host applies the sandbox: {tool}"
        );
        assert_eq!(
            tool["declaration"]["name"], module,
            "the model-facing name is unchanged: {tool}"
        );
    }
    // The whole model-visible surface, byte for byte: each component's declaration is the
    // one the native tool built, because both read the same shared guest crate.
    let workspace = tempdir().unwrap();
    let native = [
        (
            "shell",
            p1_tool_shell::ShellTool::new(
                p1_workspace::Workspace::new(workspace.path()).expect("a workspace"),
            )
            .declaration()
            .clone(),
        ),
        (
            "finish",
            p1_tool_finish::FinishTool::new(
                Arc::new(NoActivity),
                p1_tool_finish::FinishOutcome::default(),
            )
            .declaration()
            .clone(),
        ),
    ];
    for (module, declaration) in native {
        let tool = tools
            .iter()
            .find(|tool| tool["module"] == module)
            .expect("the tool is assembled");
        assert_eq!(
            tool["declaration"],
            p1_contracts::serde_json::to_value(&declaration).expect("a declaration serialises"),
            "`{module}` presents what the native tool presented"
        );
    }
}

// (b) A package is linked with exactly the services its manifest grants: the shell's package
// imports `process`, so linking it without the host's process service is a `MissingService`,
// never a component that runs unlinked; and a name the release does not carry is refused naming
// it. Both cases drive the loading step the host entries share — the shared host-entry step
// `catalog::modules` runs for `shell` and `finish` (D-XO-49) — so they live beside the release
// fixtures they need: `catalog/modules.rs`'s
// `a_package_the_release_does_not_carry_fails_naming_the_key` and `catalog/tools.rs`'s
// `a_shell_service_that_is_absent_fails_assembly`. The completion half of the same rule is
// `crates/p1-module-tests/tests/finish_boundary.rs`.
