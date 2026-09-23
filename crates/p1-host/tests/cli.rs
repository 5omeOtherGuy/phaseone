//! The binary's process contract: `--help`, `--version`, usage errors, and
//! `env show` with no credentials and no network. These run the actual `p1`
//! binary, so the argument parsing and exit codes are exercised end to end.

use std::path::Path;
use std::process::Command;

fn p1() -> Command {
    Command::new(env!("CARGO_BIN_EXE_p1"))
}

/// The real binary with a scratch home and NO ambient credential path, so it can
/// never read a real login: every credential variable a route may name is removed
/// and every per-tool directory variable is unset. `P1_CONFIG_DIR` and
/// `P1_ENVIRONMENTS_DIR` are removed too, so the shipped `environments/` directory
/// of the source tree is the one the binary sees.
fn isolated(home: &Path) -> Command {
    let mut command = p1();
    command.env("HOME", home);
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "P1_CONFIG_DIR",
        "P1_ENVIRONMENTS_DIR",
        "PI_CODING_AGENT_DIR",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "OPENCODE_API_KEY",
        "OPENCODE_GO_2_API_KEY",
        "ZAI_API_KEY",
        "KIMI_API_KEY",
    ] {
        command.env_remove(name);
    }
    command
}

#[test]
fn help_version_and_unknown_flag() {
    let output = p1().arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("usage:"));

    let output = p1().arg("--version").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("p1 0.0.1"));

    let output = p1().arg("--bogus").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown flag"));
}

#[test]
fn yes_with_ask_is_a_usage_error() {
    let output = p1().args(["--yes", "--ask", "go"]).output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(
        String::from_utf8_lossy(&output.stderr).contains("--ask"),
        "stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Each flag on its own parses; `--yes` is the default and `--ask` opts in.
    let output = p1().args(["--help"]).output().unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("--ask"), "help must document --ask: {help}");
    assert!(
        help.contains("kept for") && help.contains("default"),
        "help must say --yes is the default and kept for compatibility: {help}"
    );
}

#[test]
fn env_show_runs_without_credentials_or_network() {
    let home = tempfile::tempdir().unwrap();
    for name in ["claude", "gpt", "kimi"] {
        let output = isolated(home.path())
            .args(["env", "show", name])
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "env show {name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8_lossy(&output.stdout);
        let line = stdout.lines().next().unwrap_or_default();
        assert!(
            line.starts_with("credential  "),
            "env show {name} must name the credential source first: {stdout}"
        );
        assert!(
            line.contains("none — "),
            "no source has an entry in a scratch home: {line}"
        );
        assert!(stdout.contains("\"route\""), "missing route for {name}");
        for secret in ["accessToken", "refreshToken", "Bearer"] {
            assert!(!stdout.contains(secret), "env show {name} leaked {secret}");
        }
    }
}

/// The same line with a borrowed login present: the report names WHICH login, and
/// never the value it holds.
#[test]
fn env_show_names_the_borrowed_login_it_would_use() {
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".local/share/opencode");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("auth.json"),
        r#"{"opencode-go":{"type":"api","key":"FAKE-ENV-SHOW-KEY"}}"#,
    )
    .unwrap();

    let output = isolated(home.path())
        .args(["env", "show", "deepseek"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "env show deepseek failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().next().unwrap_or_default(),
        "credential  opencode login"
    );
    assert!(
        !stdout.contains("FAKE-ENV-SHOW-KEY"),
        "the report never contains a value: {stdout}"
    );
}

/// `p1 env show` names the resolved model (ADR-0049 stage 1, spec §2) right after
/// the credential source.
#[test]
fn env_show_prints_the_resolved_model() {
    let home = tempfile::tempdir().unwrap();
    let output = isolated(home.path())
        .args(["env", "show", "claude"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "env show claude failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().nth(1).unwrap_or_default(),
        "model  claude/claude-sonnet-5:medium",
        "{stdout}"
    );

    let output = isolated(home.path())
        .args(["env", "show", "kimi"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("credential  none — set KIMI_API_KEY"),
        "{stdout}"
    );
    assert_eq!(stdout.lines().nth(1), Some("model  kimi/kimi-k3:high"));
    assert!(stdout.contains("\"model\": \"k3\""), "{stdout}");
}

/// Every main agent gets the worker tools from the host (ADR-0050 item 1): `env show
/// claude` lists the four worker modules and its rendered prompt carries the Workers
/// section, even though `claude`'s environment file names none of them.
#[test]
fn env_show_gives_claude_the_worker_tools() {
    let home = tempfile::tempdir().unwrap();
    let output = isolated(home.path())
        .args(["env", "show", "claude"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "env show claude failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    for module in [
        "worker_start",
        "worker_result",
        "worker_continue",
        "worker_cancel",
    ] {
        assert!(
            stdout.contains(&format!("\"module\": \"{module}\"")),
            "env show claude must list `{module}`: {stdout}"
        );
    }
    assert!(
        stdout.contains("# Workers (optional)"),
        "the claude prompt must carry the Workers section: {stdout}"
    );
}

/// `p1 models` against the shipped `environments/`, `routes/` and `profiles/`: one
/// aligned row per model, the default marked, no scope and no credential value.
#[test]
fn models_lists_every_shipped_model() {
    let home = tempfile::tempdir().unwrap();
    let output = isolated(home.path()).arg("models").output().unwrap();
    assert!(
        output.status.success(),
        "p1 models failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // One row per model: every environment × every profile its route binds. The
    // anthropic-subscription route binds 6 profiles, and `claude-delegating` is gone
    // (ADR-0050), so 6 Claude + 11 others = 17.
    assert_eq!(lines.len(), 17, "one row per model: {stdout}");
    assert!(lines[0].starts_with("claude/claude-fable-5"), "{stdout}");
    assert!(lines[0].contains("anthropic-subscription"), "{stdout}");
    assert!(
        lines[0].contains("low,medium,high,extra_high,max"),
        "{stdout}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("claude/claude-sonnet-5") && line.ends_with("default")),
        "the model a bare `p1` would run is marked: {stdout}"
    );
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.ends_with("default"))
            .count(),
        1,
        "{stdout}"
    );
    assert!(
        !stdout.contains("scoped"),
        "no settings, no scope: {stdout}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("gpt/gpt-5.5 ")
                && line.contains("low,medium,high,extra_high ")),
        "{stdout}"
    );
    let kimi: Vec<&&str> = lines
        .iter()
        .filter(|line| line.starts_with("kimi/kimi-k3 "))
        .collect();
    assert_eq!(kimi.len(), 1, "{stdout}");
    assert!(kimi[0].contains("kimi-coding-subscription"), "{stdout}");
    assert!(kimi[0].contains("low,high,max"), "{stdout}");
    assert!(kimi[0].contains("KIMI_API_KEY"), "{stdout}");
    for secret in ["accessToken", "refreshToken", "Bearer", "sk-"] {
        assert!(!stdout.contains(secret), "p1 models leaked {secret}");
    }

    // SEARCH is a case-insensitive substring of `E/P`.
    let output = isolated(home.path())
        .args(["models", "DEEPSEEK2"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(
        stdout.starts_with("deepseek2/deepseek-v4.1-flash"),
        "{stdout}"
    );

    let output = isolated(home.path())
        .args(["models", "kimi"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.starts_with("kimi/kimi-k3"), "{stdout}");
}

/// `--models` scopes the listing; a pattern that matches nothing is an error.
#[test]
fn models_scopes_the_listing() {
    let home = tempfile::tempdir().unwrap();
    let output = isolated(home.path())
        .args(["models", "--models", "gpt/*"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let scoped: Vec<&str> = stdout
        .lines()
        .filter(|line| line.ends_with("scoped"))
        .collect();
    assert_eq!(scoped.len(), 7, "{stdout}");
    assert!(scoped.iter().all(|line| line.starts_with("gpt/")));

    let output = isolated(home.path())
        .args(["models", "--models", "nope/*"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--models pattern `nope/*`"), "{stderr}");
}

/// A `--env` and a `--model` that name different environments is a usage error,
/// and so is an effort level that is not one of the five.
#[test]
fn a_conflicting_model_reference_is_a_usage_error() {
    let home = tempfile::tempdir().unwrap();

    let output = isolated(home.path())
        .args(["--env", "claude", "--model", "gpt/gpt-5.6-sol", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--env `claude`"), "{stderr}");
    assert!(stderr.contains("--model `gpt/gpt-5.6-sol`"), "{stderr}");
    assert!(stderr.contains("usage:"), "a usage error prints the usage");

    let output = isolated(home.path())
        .args(["--effort", "loud", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown effort `loud`"), "{stderr}");
    assert!(stderr.contains("extra_high"), "{stderr}");

    // A reference that names no model lists the candidates.
    let output = isolated(home.path())
        .args(["--model", "claude/nope", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown model `claude/nope`"), "{stderr}");
    assert!(stderr.contains("claude/claude-opus-5"), "{stderr}");

    // A pattern that matches nothing is caught on a run too.
    let output = isolated(home.path())
        .args(["--models", "nope/*", "--env", "claude", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("matches no model"), "{stderr}");
}

/// The usage text documents the whole model-selection surface.
#[test]
fn help_documents_the_model_flags() {
    let output = p1().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    for needle in [
        "--model REF",
        "--effort LEVEL",
        "--models PATTERNS",
        "p1 models [SEARCH]",
        "extra_high",
    ] {
        assert!(help.contains(needle), "help must document {needle}: {help}");
    }
}
