//! The binary's process contract: `--help`, `--version`, usage errors, and
//! `env show` with no credentials and no network. These run the actual `p1`
//! binary, so the argument parsing and exit codes are exercised end to end.

use std::process::Command;

fn p1() -> Command {
    Command::new(env!("CARGO_BIN_EXE_p1"))
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
    for name in ["claude", "gpt"] {
        let output = p1().args(["env", "show", name]).output().unwrap();
        assert!(
            output.status.success(),
            "env show {name} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let json = String::from_utf8_lossy(&output.stdout);
        assert!(json.contains("\"route\""), "missing route for {name}");
        for secret in ["accessToken", "refreshToken", "Bearer"] {
            assert!(!json.contains(secret), "env show {name} leaked {secret}");
        }
    }
}
