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
/// and every per-tool directory variable is unset.
fn isolated(home: &Path) -> Command {
    let mut command = p1();
    command.env("HOME", home);
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "PI_CODING_AGENT_DIR",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "OPENCODE_API_KEY",
        "OPENCODE_GO_2_API_KEY",
        "ZAI_API_KEY",
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
    for name in ["claude", "gpt"] {
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
