//! Catalog contracts through the installed command path, with no live credentials or traffic.
mod common;

use std::path::Path;
#[cfg(feature = "subscriptions")]
use std::path::PathBuf;

use common::{Harness, isolated_environment, run_args, shipped_environments};
use tempfile::TempDir;

const ENVIRONMENT: &str =
    "route = \"check-route\"\nprofile = \"check-one\"\n[[tools]]\nmodule = \"read\"\n";
const ROUTE: &str = r#"
id = "check-route"
origin_route = "openai-chat/check-route"
adapter = "openai-chat"
endpoint = "https://example.invalid/v1/chat/completions"
[credential]
kind = "api-key"
env = "CHECK_API_KEY"
store_only = true
[adapter_settings]
dialect = "thinking-with-reasoning-alias"
[models.check-one]
wire_model = "wire-one"
[models.check-two]
wire_model = "wire-two"
"#;

fn write(path: &Path, text: &str) {
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn fixture() -> TempDir {
    let root = tempfile::tempdir().unwrap();
    environment(
        root.path(),
        "chosen",
        ENVIRONMENT,
        "Use {{tool:read}} in {{workspace}} on {{date}} ({{os}}). {{scratch}} {{tool_names}}",
    );
    write(&root.path().join("routes/check-route.toml"), ROUTE);
    for id in ["check-one", "check-two"] {
        write(
            &root.path().join(format!("profiles/{id}.toml")),
            &format!(
                r#"
id = "{id}"
revision = 1
model_id = "{id}-model"
family = "check"
thinking = "enabled"
efforts = ["low", "high"]
default_effort = "high"
"#
            ),
        );
    }
    root
}

fn environment(root: &Path, name: &str, definition: &str, prompt: &str) {
    write(
        &root.join(format!("environments/{name}/environment.toml")),
        definition,
    );
    write(&root.join(format!("environments/{name}/prompt.md")), prompt);
}

fn harness(root: &Path) -> Harness {
    let mut harness = Harness::new(vec![root.join("environments")], &[]);
    isolated_environment(&mut harness);
    harness
}

async fn failure(root: &Path, environment: &str, file: &str, cause: &str) {
    let mut harness = harness(root);
    assert_eq!(
        run_args(&mut harness, &["env", "check"]).await,
        1,
        "{}",
        harness.stderr.text()
    );
    let output = harness.stdout.text();
    let line = output
        .lines()
        .find(|line| line.starts_with(&format!("{environment}  ")))
        .unwrap_or_else(|| panic!("missing {environment}: {output}"));
    assert!(line.contains(file), "{line}");
    assert!(line.contains(cause), "{line}");
    assert!(output.contains("failed"), "{output}");
}

#[tokio::test]
async fn clean_catalog_checks_every_profile_and_alias_in_sorted_json() {
    let root = fixture();
    write(
        &root.path().join("environments/alias/environment.toml"),
        "alias_of = \"chosen\"\naccount = \"alternate\"\n",
    );
    write(
        &root.path().join("accounts/alternate.toml"),
        "id = \"alternate\"\norigins = [\"https://example.invalid\"]\n[credential]\nmethod = \"api-key\"\nenv = \"CHECK_API_KEY\"\nstore_only = true\n",
    );
    let mut harness = harness(root.path());
    assert_eq!(
        run_args(&mut harness, &["env", "check", "--json"]).await,
        0,
        "{}",
        harness.stdout.text()
    );
    let reports: serde_json::Value = serde_json::from_str(&harness.stdout.text()).unwrap();
    assert_eq!(reports.as_array().unwrap().len(), 2);
    assert_eq!(reports[0]["environment"], "alias");
    assert_eq!(reports[1]["environment"], "chosen");
    assert_eq!(
        reports[0]["profiles"],
        serde_json::json!(["check-one", "check-two"])
    );
    for report in reports.as_array().unwrap() {
        assert_eq!(report["ok"], true);
        assert_eq!(report["failures"], serde_json::json!([]));
    }
    assert!(!harness.stderr.text().contains("error"));
    let mut harness = crate::harness(root.path());
    assert_eq!(run_args(&mut harness, &["env", "check"]).await, 0);
    assert_eq!(
        harness.stdout.text(),
        "alias  ok\nchosen  ok\n2 environments: 2 ok, 0 failed\n"
    );
}

#[tokio::test]
async fn unknown_placeholder_names_prompt_and_environment() {
    let root = fixture();
    write(
        &root.path().join("environments/chosen/prompt.md"),
        "{{unknown}}",
    );
    failure(root.path(), "chosen", "prompt.md", "unknown").await;
}

#[tokio::test]
async fn placeholder_for_an_unassembled_tool_fails() {
    let root = fixture();
    write(
        &root.path().join("environments/chosen/prompt.md"),
        "{{tool:write}}",
    );
    failure(root.path(), "chosen", "prompt.md", "write").await;
}

#[tokio::test]
async fn unknown_tool_names_definition_and_environment() {
    let root = fixture();
    environment(
        root.path(),
        "chosen",
        &ENVIRONMENT.replace("module = \"read\"", "module = \"missing-tool\""),
        "hi",
    );
    failure(root.path(), "chosen", "environment.toml", "missing-tool").await;
}

#[tokio::test]
async fn route_binding_to_missing_profile_fails() {
    let root = fixture();
    write(
        &root.path().join("routes/check-route.toml"),
        &format!("{ROUTE}\n[models.missing-profile]\nwire_model = \"no-model\"\n"),
    );
    failure(root.path(), "chosen", "profiles", "missing-profile").await;
}

#[tokio::test]
async fn environment_profile_not_served_by_route_fails() {
    let root = fixture();
    write(
        &root.path().join("routes/check-route.toml"),
        &ROUTE.replace("[models.check-one]\nwire_model = \"wire-one\"", ""),
    );
    failure(root.path(), "chosen", "environment.toml", "check-one").await;
}

#[tokio::test]
async fn every_profile_is_assembled_not_just_enumerated() {
    let root = fixture();
    write(
        &root.path().join("profiles/check-two.toml"),
        r#"
id = "check-two"
revision = 1
model_id = "two"
family = "check"
thinking = "enabled"
efforts = ["low"]
default_effort = "low"
max_output_tokens = 10
"#,
    );
    environment(
        root.path(),
        "chosen",
        "route = \"check-route\"\nprofile = \"check-one\"\n[options]\nmax_output_tokens = 20\n",
        "hi",
    );
    failure(root.path(), "chosen", "environment.toml", "check-two").await;
}

#[tokio::test]
async fn account_must_cover_endpoint_origin() {
    let root = fixture();
    let route = ROUTE.replace(
        "[credential]\nkind = \"api-key\"\nenv = \"CHECK_API_KEY\"\nstore_only = true\n",
        "",
    );
    write(
        &root.path().join("routes/check-route.toml"),
        &route.replace(
            "[adapter_settings]",
            "account = \"elsewhere\"\n[adapter_settings]",
        ),
    );
    write(
        &root.path().join("accounts/elsewhere.toml"),
        "id = \"elsewhere\"\norigins = [\"https://elsewhere.invalid\"]\n[credential]\nmethod = \"api-key\"\nenv = \"CHECK_API_KEY\"\nstore_only = true\n",
    );
    failure(root.path(), "chosen", "environment.toml", "origin").await;
}

#[cfg(feature = "delegation")]
fn subagent(root: &Path, environment: &str, prompt: &str, tools: &str, models: &str) {
    write(
        &root.join("subagents.toml"),
        &format!(
            r#"
[[subagents]]
subagent_type = "search"
environment = "{environment}"
description = "Find files"
prompt_file = "{prompt}"
tools = [{tools}]
models = [{models}]
"#
        ),
    );
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn dangling_subagent_environment_names_configuration_and_environment() {
    let root = fixture();
    write(&root.path().join("role.md"), "Search");
    subagent(
        root.path(),
        "missing-worker",
        "role.md",
        "\"read\"",
        "\"chosen/check-one\"",
    );
    failure(
        root.path(),
        "missing-worker",
        "subagents.toml",
        "missing-worker",
    )
    .await;
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn subagent_prompt_tools_and_models_resolve() {
    for (prompt, tools, models, expected) in [
        ("absent.md", "\"read\"", "\"chosen/check-one\"", "absent.md"),
        (
            "role.md",
            "\"unknown-tool\"",
            "\"chosen/check-one\"",
            "unknown-tool",
        ),
        (
            "role.md",
            "\"read\"",
            "\"chosen/missing\"",
            "chosen/missing",
        ),
    ] {
        let root = fixture();
        write(&root.path().join("role.md"), "Search");
        subagent(root.path(), "chosen", prompt, tools, models);
        let mut harness = harness(root.path());
        assert_eq!(run_args(&mut harness, &["env", "check"]).await, 1);
        let output = harness.stdout.text();
        assert!(
            output.contains("subagent") && output.contains(expected),
            "{output}"
        );
    }
}

#[tokio::test]
async fn alias_of_missing_environment_fails() {
    let root = fixture();
    write(
        &root
            .path()
            .join("environments/broken-alias/environment.toml"),
        "alias_of = \"absent\"\naccount = \"alternate\"\n",
    );
    failure(
        root.path(),
        "broken-alias",
        "broken-alias/environment.toml",
        "absent",
    )
    .await;
}

#[tokio::test]
async fn malformed_tables_and_mcp_and_lock_fail_without_execution() {
    for (suffix, expected) in [
        ("[tool_concurrency]\nmax_parallel = 11\n", "max_parallel"),
        (
            "[context]\nwindow_tokens = 100\noutput_headroom_tokens = 100\nsummarize_at_tokens = 50\n",
            "context",
        ),
        ("[capabilities]\nworker = false\n", "worker"),
        (
            "[[mcp_servers]]\nname = \"BAD\"\ncommand = \"never-run\"\n",
            "mcp_servers",
        ),
    ] {
        let root = fixture();
        environment(
            root.path(),
            "chosen",
            &format!("{ENVIRONMENT}\n{suffix}"),
            "hi",
        );
        failure(root.path(), "chosen", "environment.toml", expected).await;
    }
    let root = fixture();
    write(&root.path().join("modules.lock"), "version = 999\n");
    failure(root.path(), "chosen", "modules.lock", "lock").await;
}

#[tokio::test]
async fn valid_mcp_is_only_parsed_never_connected_or_spawned() {
    let root = fixture();
    environment(
        root.path(),
        "chosen",
        &format!(
            "{ENVIRONMENT}\n[[mcp_servers]]\nname = \"docs\"\ncommand = \"/nonexistent-do-not-spawn\"\nenv_pass = [\"MCP_KEY\"]\n[[mcp_servers]]\nname = \"web\"\nurl = \"https://example.invalid/mcp\"\nbearer_token_env = \"MCP_TOKEN\"\n"
        ),
        "hi",
    );
    let mut harness = harness(root.path());
    assert_eq!(
        run_args(&mut harness, &["env", "check"]).await,
        0,
        "{}",
        harness.stdout.text()
    );
}

#[cfg(feature = "subscriptions")]
#[tokio::test]
async fn explicit_roots_shadow_shipped_environment_in_given_order() {
    let first = fixture();
    let second = fixture();
    // This pack replaces claude's models, so it also replaces configured roles
    // that otherwise refer to the shipped claude model names.
    write(&first.path().join("subagents.toml"), "subagents = []\n");
    environment(second.path(), "second-only", ENVIRONMENT, "second layer");
    environment(first.path(), "claude", ENVIRONMENT, "first layer");
    environment(
        first.path(),
        "claude2",
        ENVIRONMENT,
        "first layer alias replacement",
    );
    environment(
        second.path(),
        "claude",
        ENVIRONMENT,
        "{{broken-later-layer}}",
    );
    let ambient = fixture();
    environment(
        ambient.path(),
        "ambient-only",
        ENVIRONMENT,
        "{{broken-ambient}}",
    );
    let mut harness = harness(ambient.path());
    let original = harness.deps.environment_dirs.clone();
    assert_eq!(
        run_args(
            &mut harness,
            &[
                "env",
                "check",
                "--dir",
                first.path().to_str().unwrap(),
                "--dir",
                second.path().to_str().unwrap(),
                "--json"
            ]
        )
        .await,
        0,
        "{}",
        harness.stdout.text()
    );
    let reports: Vec<serde_json::Value> = serde_json::from_str(&harness.stdout.text()).unwrap();
    let claude = reports
        .iter()
        .find(|report| report["environment"] == "claude")
        .unwrap();
    assert_eq!(claude["ok"], true);
    assert_eq!(
        PathBuf::from(claude["file"].as_str().unwrap()),
        first.path().join("environments/claude/environment.toml")
    );
    assert!(
        reports.iter().any(|report| report["environment"] == "glm"),
        "shipped layer remains"
    );
    assert!(
        reports
            .iter()
            .any(|report| report["environment"] == "second-only")
    );
    assert!(
        !reports
            .iter()
            .any(|report| report["environment"] == "ambient-only")
    );
    assert_eq!(harness.deps.environment_dirs, original);
}

#[tokio::test]
async fn credential_content_is_not_read_on_success_or_refusal() {
    use std::os::unix::fs::PermissionsExt;
    let root = fixture();
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join(".config/p1");
    std::fs::create_dir_all(&store).unwrap();
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
    // As in credential_origins.rs: any attempted store read refuses this symlink.
    std::os::unix::fs::symlink("missing-credential-document", store.join("auth.json")).unwrap();
    std::os::unix::fs::symlink("missing-origin-document", store.join("auth.json.origins")).unwrap();
    for bad in [false, true] {
        if bad {
            write(
                &root.path().join("environments/chosen/prompt.md"),
                "{{bad}}",
            );
        }
        let mut harness = harness(root.path());
        harness.deps.home = Some(home.path().into());
        harness.deps.shell_env = Some(vec![
            ("HOME".into(), home.path().as_os_str().to_owned()),
            (
                "XDG_CONFIG_HOME".into(),
                home.path().join(".config").into_os_string(),
            ),
        ]);
        assert_eq!(
            run_args(&mut harness, &["env", "check"]).await,
            i32::from(bad),
            "{}",
            harness.stdout.text()
        );
        assert!(!harness.stdout.text().contains("symlink"));
        assert!(!harness.stderr.text().contains("symlink"));
    }
}

#[test]
fn command_parsing_rejects_missing_directory_and_unknown_flags() {
    for args in [
        vec!["env", "check", "--dir"],
        vec!["env", "check", "--dir", "--json"],
        vec!["env", "check", "--network"],
    ] {
        assert!(
            p1_host::cli::parse(&args.into_iter().map(str::to_string).collect::<Vec<_>>()).is_err()
        );
    }
    assert!(p1_host::cli::usage().contains("p1 env check [--dir CATALOG_ROOT]... [--json]"));
    assert!(shipped_environments().is_dir());
}

#[cfg(feature = "subscriptions")]
#[tokio::test]
async fn borrowed_origin_and_shipped_origin_overrides_fail_without_credentials() {
    let root = fixture();
    write(
        &root.path().join("routes/check-route.toml"),
        &ROUTE.replace(
            "kind = \"api-key\"\nenv = \"CHECK_API_KEY\"\nstore_only = true",
            "kind = \"claude-code-oauth\"",
        ),
    );
    failure(root.path(), "chosen", "check-route.toml", "borrowed").await;
    let root = fixture();
    write(
        &root.path().join("routes/glm-subscription.toml"),
        &ROUTE.replace("check-route", "glm-subscription"),
    );
    environment(
        root.path(),
        "chosen",
        &ENVIRONMENT.replace("check-route", "glm-subscription"),
        "hi",
    );
    failure(root.path(), "chosen", "glm-subscription.toml", "ships").await;
}

#[cfg(feature = "delegation")]
#[tokio::test]
async fn clean_subagent_configuration_resolves_and_renders() {
    let root = fixture();
    write(
        &root.path().join("role.md"),
        "Use {{tool:read}} to search {{workspace}}.",
    );
    subagent(
        root.path(),
        "chosen",
        "role.md",
        "\"read\"",
        "\"chosen/check-one:high\", \"chosen/check-two\"",
    );
    let mut harness = harness(root.path());
    assert_eq!(
        run_args(&mut harness, &["env", "check"]).await,
        0,
        "{}",
        harness.stdout.text()
    );
}

#[tokio::test]
async fn missing_explicit_root_is_not_a_silent_pass() {
    let root = fixture();
    let missing = root.path().join("missing");
    let mut harness = harness(root.path());
    assert_eq!(
        run_args(
            &mut harness,
            &["env", "check", "--dir", missing.to_str().unwrap()]
        )
        .await,
        1
    );
    assert!(
        harness
            .stdout
            .text()
            .contains("missing environments directory")
    );
}

#[cfg(target_os = "linux")]
#[test]
fn inspection_opens_no_credential_file_for_reading_as_observed_by_system_calls() {
    // strace is additional evidence when installed; the poisoned-store test
    // above remains portable and unconditional.
    if std::process::Command::new("strace")
        .arg("--version")
        .output()
        .is_err()
    {
        return;
    }
    let root = fixture();
    let home = tempfile::tempdir().unwrap();
    let store = home.path().join(".config/p1");
    std::fs::create_dir_all(&store).unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o700)).unwrap();
    let sentinel = "catalog-inspection-must-not-read-this-document";
    for path in [
        store.join("auth.json"),
        store.join("auth.json.origins"),
        store.join(".auth.json.p1-unsaved"),
        home.path().join(".claude/.credentials.json"),
        home.path().join(".codex/auth.json"),
    ] {
        write(&path, sentinel);
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000)).unwrap();
    }
    let trace = home.path().join("opens.trace");
    for bad in [false, true] {
        if bad {
            write(
                &root.path().join("environments/chosen/prompt.md"),
                "{{bad}}",
            );
        }
        let output = std::process::Command::new("strace")
            .args(["-f", "-e", "trace=open,openat,openat2,connect", "-o"])
            .arg(&trace)
            .arg(env!("CARGO_BIN_EXE_p1"))
            .args(["env", "check"])
            .env_clear()
            .env("HOME", home.path())
            .env("XDG_CONFIG_HOME", home.path().join(".config"))
            .env("P1_CONFIG_DIR", &store)
            .env("P1_ENVIRONMENTS_DIR", root.path().join("environments"))
            .current_dir(root.path())
            .output()
            .unwrap();
        assert_eq!(
            output.status.code(),
            Some(i32::from(bad)),
            "{}",
            String::from_utf8_lossy(&output.stdout)
        );
        assert!(!String::from_utf8_lossy(&output.stdout).contains(sentinel));
        assert!(!String::from_utf8_lossy(&output.stderr).contains(sentinel));
        let opens = std::fs::read_to_string(&trace).unwrap();
        for line in opens.lines() {
            // The existing protected reader uses O_PATH to inspect identities.
            // Such descriptors cannot read content; preserve that security policy.
            assert!(
                line.contains("O_PATH")
                    || (!line.contains("auth.json") && !line.contains(".credentials.json")),
                "credential opened for reading: {line}"
            );
            assert!(!line.contains("connect("), "network connection: {line}");
        }
    }
}
