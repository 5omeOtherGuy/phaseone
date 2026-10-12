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
/// and every per-tool directory variable is unset. `P1_CONFIG_DIR`,
/// `P1_ENVIRONMENTS_DIR` and `P1_PACKS` are removed too, so the shipped `environments/`
/// directory of the source tree is the one the binary sees.
fn isolated(home: &Path) -> Command {
    let mut command = p1();
    command.env("HOME", home);
    for name in [
        "XDG_CONFIG_HOME",
        "XDG_DATA_HOME",
        "P1_CONFIG_DIR",
        "P1_ENVIRONMENTS_DIR",
        "P1_PACKS",
        "PI_CODING_AGENT_DIR",
        "CLAUDE_CONFIG_DIR",
        "CODEX_HOME",
        "OPENCODE_API_KEY",
        "OPENCODE_GO_1_API_KEY",
        "OPENCODE_GO_2_API_KEY",
        "OPENCODE_GO_3_API_KEY",
        "OPENCODE_ZEN_1_API_KEY",
        "OPENCODE_ZEN_2_API_KEY",
        "OPENCODE_ZEN_3_API_KEY",
        "OPENCODE_ZEN_API_KEY",
        "CLINE_PASS_1_API_KEY",
        "CLINE_PASS_2_API_KEY",
        "ZAI_API_KEY",
        "KIMI_API_KEY",
        "TEST_KEY",
        "FAKE_LEGACY_KEY",
    ] {
        command.env_remove(name);
    }
    command
}

/// [`isolated`] with the owner pack layered over the shipped catalog (`P1_PACKS`,
/// ADR-0201): the owner's distribution, whose many models the listing tests pin (#853).
fn with_owner_pack(home: &Path) -> Command {
    let mut command = isolated(home);
    command.env(
        "P1_PACKS",
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../packs/owner"),
    );
    command
}

/// A scratch environments root whose one route explicitly opts into the LEGACY
/// borrowed-login chain. Every shipped route is self-contained since ADR-0061, so a
/// route WITHOUT `store_only` is the only way left to exercise the borrowed-source
/// reporting end to end.
fn legacy_borrow_root() -> p1_testkit::catalog::CatalogFixture {
    let fixture = p1_testkit::catalog::CatalogFixture::new();
    fixture.write_profile(
        "borrowed-model",
        &p1_testkit::catalog::profile_toml("openai-chat", "borrowed-model"),
    );
    fixture.write_route("legacy-borrow", "id = \"legacy-borrow\"\norigin_route = \"openai-chat/legacy-borrow\"\nadapter = \"openai-chat\"\nendpoint = \"https://opencode.ai/zen/go/v1/chat/completions\"\n[credential]\nkind = \"api-key\"\nenv = \"FAKE_LEGACY_KEY\"\nborrow = [\"opencode:opencode-go\"]\n[adapter_settings]\ndialect = \"retained-thinking\"\n[models.\"borrowed-model\"]\nwire_model = \"borrowed-wire\"\n");
    fixture.write_environment(
        "legacy-borrow",
        "route = \"legacy-borrow\"\nprofile = \"borrowed-model\"\n",
        "prompt",
    );
    fixture
}

fn env_show_fixture() -> p1_testkit::catalog::CatalogFixture {
    let fixture = p1_testkit::catalog::CatalogFixture::new();
    for (name, adapter) in [
        ("messages", "anthropic-messages"),
        ("responses", "openai-responses"),
        ("chat", "openai-chat"),
    ] {
        fixture.adapter(name, adapter, name, name);
        // Inspection reports missing sources, not custom-origin approval refusals.
        fixture.write_route(
            name,
            &p1_testkit::catalog::route_toml(adapter, name, name)
                .replace("https://example.invalid/v1", "https://localhost/v1"),
        );
        fixture.write_environment(
            name,
            &format!(
                "route = {name:?}\nprofile = {name:?}\n[options]\nreasoning_effort = \"high\"\n"
            ),
            "Synthetic",
        );
    }
    fixture
}

#[test]
fn help_version_and_unknown_flag() {
    let output = p1().arg("--help").output().unwrap();
    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("usage:"));

    let output = p1().arg("--version").output().unwrap();
    assert!(output.status.success());
    let version = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let prefix = format!("p1 {} (", env!("CARGO_PKG_VERSION"));
    assert!(version.starts_with(&prefix), "{version:?}");
    assert!(version.ends_with(')'), "{version:?}");
    // The sha and the build date, in that order and each non-empty; `unknown` (no git,
    // no date source) is a valid field.
    let fields: Vec<&str> = version
        .trim_start_matches(prefix.as_str())
        .trim_end_matches(')')
        .split(' ')
        .collect();
    assert_eq!(fields.len(), 2, "{version:?}");
    assert!(fields.iter().all(|field| !field.is_empty()), "{version:?}");

    let output = p1().arg("--bogus").output().unwrap();
    assert_eq!(output.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&output.stderr).contains("unknown flag"));
}

/// A typo of a subcommand is a usage error: exit 2 with no provider request, no
/// journal and no run state — the bug in issue #83 was that `p1 envs` started a
/// headless agent instead (issue #83).
#[test]
fn a_subcommand_typo_exits_two_without_running() {
    // `envs` (the reported typo) and the two the owner named, `modles` and `loginn`.
    // `modles` is one edit from `modules` and two from `models`, and the suggestion is
    // the nearest command, so it now means `modules` (S1.6 added the subcommand).
    for (typo, meant) in [("envs", "env"), ("modles", "modules"), ("loginn", "login")] {
        let home = tempfile::tempdir().unwrap();
        let output = isolated(home.path()).arg(typo).output().unwrap();
        assert_eq!(
            output.status.code(),
            Some(2),
            "{typo} stderr: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains(&format!("unknown command '{typo}'")),
            "{stderr}"
        );
        assert!(
            stderr.contains(&format!("did you mean '{meant}'?")),
            "{stderr}"
        );
        assert!(stderr.contains(&format!("p1 -- {typo}")), "{stderr}");
        assert!(
            stderr.contains("usage:"),
            "a usage error prints the usage: {stderr}"
        );
        // No agent ran: nothing was written under the scratch home.
        let leftovers: Vec<_> = std::fs::read_dir(home.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert!(
            leftovers.is_empty(),
            "no state written for {typo}, found {leftovers:?}"
        );
    }

    // `--help` documents `--`.
    let output = p1().arg("--help").output().unwrap();
    let help = String::from_utf8_lossy(&output.stdout);
    assert!(help.contains("p1 -- envs"), "help must document --: {help}");
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
    let fixture = env_show_fixture();

    for name in ["messages", "responses", "chat"] {
        let output = isolated(home.path())
            .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
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
/// never the value it holds. The shipped routes are self-contained now (ADR-0061),
/// so this runs against an explicit legacy route — the one configuration that still
/// borrows another CLI's login.
#[test]
fn env_show_names_the_borrowed_login_it_would_use() {
    let root = legacy_borrow_root();
    let home = tempfile::tempdir().unwrap();
    let data = home.path().join(".local/share/opencode");
    std::fs::create_dir_all(&data).unwrap();
    std::fs::write(
        data.join("auth.json"),
        r#"{"opencode-go":{"type":"api","key":"FAKE-ENV-SHOW-KEY"}}"#,
    )
    .unwrap();

    // ADR-0110: the custom route borrows only once its origin is approved.
    let trusted = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", root.root.path().join("environments"))
        .args(["login", "legacy-borrow", "--trust-endpoint"])
        .output()
        .unwrap();
    assert!(
        trusted.status.success(),
        "login --trust-endpoint failed: {}",
        String::from_utf8_lossy(&trusted.stderr)
    );
    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", root.root.path().join("environments"))
        .args(["env", "show", "legacy-borrow"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "env show legacy-borrow failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().next().unwrap_or_default(),
        "credential  opencode login",
        "a route without `store_only` still borrows: {stdout}"
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
    let fixture = env_show_fixture();

    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["env", "show", "messages"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "env show messages failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(
        stdout.lines().nth(1).unwrap_or_default(),
        "model  messages/messages:high",
        "{stdout}"
    );

    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["env", "show", "chat"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with("credential  none — set TEST_KEY"),
        "{stdout}"
    );
    assert_eq!(stdout.lines().nth(1), Some("model  chat/chat:high"));
    assert!(stdout.contains("\"model\": \"chat\""), "{stdout}");
}

/// Every main agent gets the worker tools from the host (ADR-0050 item 1): `env show
/// claude` lists the four worker modules and its rendered prompt carries the Workers
/// section, even though `claude`'s environment file names none of them.
#[test]
fn env_show_gives_claude_the_worker_tools() {
    let home = tempfile::tempdir().unwrap();
    let output = with_owner_pack(home.path())
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

/// `p1 models` against the shipped `environments/`, `routes/` and `profiles/` with the
/// owner pack: one aligned row per model, the default marked, no scope and no credential
/// value.
#[test]
fn models_lists_every_shipped_model() {
    let home = tempfile::tempdir().unwrap();
    let output = with_owner_pack(home.path()).arg("models").output().unwrap();
    assert!(
        output.status.success(),
        "p1 models failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    let lines: Vec<&str> = stdout.lines().collect();
    // One row per model (ADR-0217), not per environment: the twenty profiles the shipped routes
    // bind (six Claude, seven GPT, three GLM with ClinePass's Flash, DeepSeek, Kimi, MiMo and
    // Space Bunny), whatever environments the owner pack adds.
    assert_eq!(lines.len(), 20, "one row per model: {stdout}");
    assert!(lines[0].starts_with("claude-fable-5 "), "{stdout}");
    assert!(
        lines[0].contains(" claude ") && lines[0].contains(" anthropic "),
        "the kit and the default environment's route: {stdout}"
    );
    assert!(
        lines[0].contains("low,medium,high,extra_high,max"),
        "{stdout}"
    );
    assert!(
        lines
            .iter()
            .any(|line| line.starts_with("claude-sonnet-5 ") && line.ends_with("default")),
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
        lines.iter().any(|line| line.starts_with("gpt-5.5 ")
            && line.contains("one of openai, openai-chat, openai-codex-subscription")
            && line.contains("low,medium,high,extra_high ")),
        "several routes and no preference: the candidates: {stdout}"
    );
    let kimi: Vec<&&str> = lines
        .iter()
        .filter(|line| line.starts_with("kimi-k3 "))
        .collect();
    assert_eq!(kimi.len(), 1, "{stdout}");
    assert!(kimi[0].contains("kimi-coding-subscription"), "{stdout}");
    assert!(kimi[0].contains("low,high,max"), "{stdout}");
    assert!(kimi[0].contains("KIMI_API_KEY"), "{stdout}");
    for secret in ["accessToken", "refreshToken", "Bearer", "sk-"] {
        assert!(!stdout.contains(secret), "p1 models leaked {secret}");
    }

    // SEARCH is a case-insensitive substring of the model.
    let output = with_owner_pack(home.path())
        .args(["models", "KIMI"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert_eq!(stdout.lines().count(), 1, "{stdout}");
    assert!(stdout.starts_with("kimi-k3 "), "{stdout}");
}

/// `--models` scopes the listing; a pattern that matches nothing is an error.
#[test]
fn models_scopes_the_listing() {
    let home = tempfile::tempdir().unwrap();
    let fixture = env_show_fixture();
    fixture.write_profile(
        "other",
        &p1_testkit::catalog::profile_toml("openai-responses", "other"),
    );
    fixture.write_route(
        "responses",
        &(p1_testkit::catalog::route_toml("openai-responses", "responses", "responses")
            + "[models.other]\nwire_model = \"other-wire\"\n"),
    );
    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["models", "--models", "responses/*"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    let scoped: Vec<&str> = stdout
        .lines()
        .filter(|line| line.ends_with("scoped"))
        .collect();
    // One row per model (ADR-0217): `responses/*` matches the `responses` environment's two
    // pairs, whose models are `responses` and `other`.
    assert_eq!(scoped.len(), 2, "{stdout}");
    assert!(
        scoped
            .iter()
            .all(|line| line.starts_with("responses ") || line.starts_with("other ")),
        "{stdout}"
    );

    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
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
    let fixture = env_show_fixture();

    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["--env", "messages", "--model", "responses/responses", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("--env `messages`"), "{stderr}");
    assert!(stderr.contains("--model `responses/responses`"), "{stderr}");
    assert!(stderr.contains("usage:"), "a usage error prints the usage");

    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["--effort", "loud", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown effort `loud`"), "{stderr}");
    assert!(stderr.contains("extra_high"), "{stderr}");

    // A reference that names no model lists the candidates.
    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["--model", "messages/nope", "go"])
        .output()
        .unwrap();
    assert_eq!(output.status.code(), Some(2));
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown model `messages/nope`"), "{stderr}");
    assert!(stderr.contains("messages/messages"), "{stderr}");

    // A pattern that matches nothing is caught on a run too.
    let output = isolated(home.path())
        .env("P1_ENVIRONMENTS_DIR", &fixture.environment_dirs()[0])
        .args(["--models", "nope/*", "--env", "messages", "go"])
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
